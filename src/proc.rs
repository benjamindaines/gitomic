// Process control for the background watcher: detachment into an independent session, pidfile management,
// and cooperative shutdown signalling. The watcher is not a system service; it is a child process spawned by
// `gitomic init` that outlives the invoking shell and is reaped by `gitomic finish` or `gitomic stop`.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Res;

// Set by the signal handler when SIGTERM or SIGINT is received. The watcher loop polls this flag and, on
// observing it, performs a final flush and exits. Only an atomic store occurs in signal context, which is
// async-signal-safe.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

// True once a shutdown signal has been delivered.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

// Install handlers for SIGTERM and SIGINT. Establishing these before the watch loop begins guarantees the
// flush-on-exit contract that `finish` and `stop` rely upon.
pub fn install_signal_handlers() {
    // The handler is coerced to an `extern "C"` function pointer before the numeric `sighandler_t` cast that
    // `libc::signal` requires. The intermediate pointer avoids a direct function-item-to-integer cast; the
    // two-step form is the documented way to express this conversion without tripping fn-to-numeric lints.
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: on_signal only performs an atomic store, which is permitted in a signal handler.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

// Outcome of an attempt to detach into the background.
pub enum Fork {
    // The calling (foreground) process; the field carries the intermediate child's pid, already reaped.
    Parent,
    // The detached grandchild; execution continues here into the watch loop.
    Child,
}

// Detach the current process into an independent session via the double-fork idiom. The first fork lets the
// parent return promptly; setsid in the intermediate removes the controlling terminal; the second fork ensures
// the resulting grandchild can never reacquire one. Standard streams are redirected in the caller once the
// watcher's log path is known. This must be invoked before any threads are created, since fork does not
// duplicate threads and the process is single-threaded at this point.
pub fn daemonize() -> Res<Fork> {
    // SAFETY: fork is called from a single-threaded process; the child paths avoid running at-exit handlers by
    // using _exit for the throwaway intermediate.
    unsafe {
        match libc::fork() {
            -1 => return Err(io_err("fork")),
            0 => {} // intermediate child continues below
            _ => {
                // Parent: reap the intermediate immediately so it does not linger as a zombie, then report.
                let mut status = 0;
                libc::wait(&mut status);
                return Ok(Fork::Parent);
            }
        }

        if libc::setsid() == -1 {
            libc::_exit(1);
        }

        match libc::fork() {
            -1 => libc::_exit(1),
            0 => Ok(Fork::Child), // grandchild: the watcher
            _ => libc::_exit(0),  // intermediate exits, orphaning the grandchild to init
        }
    }
}

// Redirect stdin from /dev/null and stdout/stderr to `log` (append), so watcher diagnostics survive the
// detachment and the closed controlling terminal. Called only in the detached grandchild.
pub fn redirect_stdio(log: &Path) -> Res<()> {
    let devnull = fs::OpenOptions::new().read(true).open("/dev/null")?;
    let out = fs::OpenOptions::new().create(true).append(true).open(log)?;
    // SAFETY: dup2 onto the standard descriptors; the source descriptors are valid open files.
    unsafe {
        libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO);
        libc::dup2(out.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(out.as_raw_fd(), libc::STDERR_FILENO);
    }
    Ok(())
}

// Current process id.
pub fn pid() -> i32 {
    // SAFETY: getpid has no failure mode.
    unsafe { libc::getpid() }
}

// Whether a process with `target` pid exists and is signalable by this user. Implemented via signal 0, which
// performs permission and existence checks without delivering a signal.
pub fn alive(target: i32) -> bool {
    // SAFETY: kill with signal 0 only probes; it does not affect the target.
    unsafe { libc::kill(target, 0) == 0 }
}

// Request cooperative termination of `target` via SIGTERM. The watcher's handler flushes and exits.
pub fn request_stop(target: i32) {
    // SAFETY: kill delivers SIGTERM to a specific pid; a stale pid simply yields ESRCH.
    unsafe {
        libc::kill(target, libc::SIGTERM);
    }
}

// Path of the watcher pidfile within the gitomic state directory under the git directory.
pub fn pidfile(gitomic_dir: &Path) -> PathBuf {
    gitomic_dir.join("watch.pid")
}

// Path of the watcher log within the gitomic state directory.
pub fn logfile(gitomic_dir: &Path) -> PathBuf {
    gitomic_dir.join("watch.log")
}

// Read the pid recorded in the pidfile, if the file exists and parses. A stale file whose process is gone is
// reported as None by the caller after an aliveness check.
pub fn read_pid(gitomic_dir: &Path) -> Option<i32> {
    fs::read_to_string(pidfile(gitomic_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

// Write the current pid to the pidfile, creating the state directory if necessary.
pub fn write_pid(gitomic_dir: &Path) -> Res<()> {
    fs::create_dir_all(gitomic_dir)?;
    fs::write(pidfile(gitomic_dir), format!("{}\n", pid()))?;
    Ok(())
}

// Remove the pidfile if present; absence is not an error.
pub fn clear_pid(gitomic_dir: &Path) {
    let _ = fs::remove_file(pidfile(gitomic_dir));
}

fn io_err(what: &str) -> Box<dyn std::error::Error> {
    format!("{}: {}", what, std::io::Error::last_os_error()).into()
}
