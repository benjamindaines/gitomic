// Process control for the background watcher: detachment into an independent session, pidfile management,
// and cooperative shutdown signalling. The watcher is not a system service; it is a child process spawned by
// `gitomic init` that outlives the invoking shell and is reaped by `gitomic finish` or `gitomic stop`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
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

// ---------------------------------------------------------------------------------------------
// Cross-repository session registry (issue #6). Pidfiles above are per-repository, nested under
// that repository's git directory, so nothing already answers "what is running, anywhere" without
// visiting each repository in turn. This section adds a small machine-wide log for that question,
// meant to back a shell-profile hook run on every new terminal rather than a daemon of its own.
// ---------------------------------------------------------------------------------------------

// One live watcher, as reported by the registry: which repository, which branch, and its pid.
pub struct ActiveSession {
    pub repo: String,
    pub branch: String,
    pub pid: i32,
}

// Registry file: an append-only log of START/STOP lines, living beside the configuration file rather
// than under any one repository, since its whole purpose is to outlive and cross repository
// boundaries. None when no configuration directory can be resolved (no $HOME) — registry-backed
// awareness then degrades to a silent no-op rather than a hard error, since it is a convenience
// feature layered on top of the per-repository session, which functions correctly without it.
fn registry_path() -> Option<PathBuf> {
    Config::dir().map(|d| d.join("active-sessions"))
}

// Open the registry (creating its directory and the file itself if necessary), take an exclusive
// flock for the duration of `f`, and run `f` with the open file positioned at the start. The lock is
// released when `file` drops at the end of this call (including on an early return from `f`), so it
// cannot outlive a single call. Contention is rare — once per watcher start/stop, never per commit —
// so one coarse whole-file lock is simpler than anything finer-grained and cheap enough to hold for a
// read plus a possible rewrite. Any failure to open or lock the file is treated as a silent no-op for
// the same reason `registry_path` degrades silently: this is a convenience layered on the
// per-repository session, which does not depend on it.
fn with_registry<F: FnOnce(&mut File)>(f: F) {
    let path = match registry_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut file = match OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    // SAFETY: flock on a valid, just-opened file descriptor. Released automatically when `file` is
    // dropped at the end of this function, closing the descriptor — including if `f` returns early —
    // so a wedged lock cannot outlive this call.
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_EX);
    }
    let _ = file.seek(SeekFrom::Start(0));
    f(&mut file);
}

// Replay a registry's START/STOP lines into the set of entries that are currently started but not
// yet stopped, in file order. Malformed lines are skipped rather than rejected, so a partially
// written line from a process that died mid-write does not make the whole registry unreadable.
fn parse_registry(text: &str) -> Vec<(String, String, i32)> {
    let mut active: Vec<(String, String, i32)> = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(4, '\t');
        let (kind, repo, branch, pid_field) =
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(k), Some(r), Some(b), Some(p)) => (k, r, b, p),
                _ => continue,
            };
        let pid: i32 = match pid_field.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let key = (repo.to_string(), branch.to_string(), pid);
        match kind {
            "START" => {
                if !active.contains(&key) {
                    active.push(key);
                }
            }
            "STOP" => active.retain(|k| k != &key),
            _ => {}
        }
    }
    active
}

// Announce a newly live watcher by appending one START line. Called only once the pidfile itself has
// been written successfully, so the registry and the pidfile can never disagree about whether a
// watcher is up.
pub fn announce_active(repo: &Path, branch: &str) {
    let line = format!("START\t{}\t{}\t{}\n", repo.display(), branch, pid());
    with_registry(|file| {
        let _ = file.seek(SeekFrom::End(0));
        let _ = file.write_all(line.as_bytes());
    });
}

// Append a STOP line for a watcher that is going down, then — if that leaves no other watcher
// outstanding anywhere on the machine — truncate the registry to empty. This is what keeps the log
// from growing without bound under normal use: it never holds more than whatever is currently live,
// and a machine with no gitomic session running carries no leftover content at all. A watcher killed
// uncleanly (`kill -9`, a crash) never reaches this call, leaving an orphaned START behind; that case
// is handled on the read side (see `active_sessions`) by checking liveness rather than trusting STOP
// bookkeeping alone, so an unbounded-growth path exists only under repeated unclean kills with no
// intervening `active` read, not under ordinary use.
pub fn retire_active(repo: &Path, branch: &str) {
    let line = format!("STOP\t{}\t{}\t{}\n", repo.display(), branch, pid());
    with_registry(|file| {
        let _ = file.seek(SeekFrom::End(0));
        if file.write_all(line.as_bytes()).is_err() {
            return;
        }
        let _ = file.seek(SeekFrom::Start(0));
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return;
        }
        let still_active = parse_registry(&text).into_iter().any(|(_, _, p)| alive(p));
        if !still_active {
            let _ = file.set_len(0);
        }
    });
}

// Every watcher the registry currently reports as live, self-healed against unclean exits: an entry
// whose pid is no longer alive is dropped rather than reported, the same treatment `live_watcher` in
// commands.rs already gives a single repository's pidfile. If dropping dead entries empties the
// visible set while the file itself still holds bytes (orphaned STARTs from unclean kills, or stale
// STOPs), the registry is opportunistically truncated here too, so a `kill -9` does not require a
// clean start/stop cycle elsewhere before the log shrinks back down.
pub fn active_sessions() -> Vec<ActiveSession> {
    let mut result = Vec::new();
    with_registry(|file| {
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return;
        }
        let mut live = parse_registry(&text);
        live.retain(|(_, _, p)| alive(*p));
        if live.is_empty() && !text.is_empty() {
            let _ = file.set_len(0);
        }
        result = live
            .into_iter()
            .map(|(repo, branch, pid)| ActiveSession { repo, branch, pid })
            .collect();
    });
    result
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn parse_registry_tracks_start_stop_pairs() {
        let text =
            "START\t/repo/a\tmain\t111\nSTART\t/repo/b\tdev\t222\nSTOP\t/repo/a\tmain\t111\n";
        let active = parse_registry(text);
        assert_eq!(
            active,
            vec![("/repo/b".to_string(), "dev".to_string(), 222)]
        );
    }

    #[test]
    fn parse_registry_ignores_malformed_and_stop_without_start() {
        let text = "not a registry line\nSTOP\t/repo/a\tmain\t111\nSTART\t/repo/a\tmain\t111\n";
        assert_eq!(
            parse_registry(text),
            vec![("/repo/a".to_string(), "main".to_string(), 111)]
        );
    }

    #[test]
    fn parse_registry_start_is_idempotent() {
        // Two START lines for the same key (e.g. a re-announce after a crash and restart with a
        // reused pid) contribute one entry, not two, so a later single STOP fully clears it.
        let text =
            "START\t/repo/a\tmain\t111\nSTART\t/repo/a\tmain\t111\nSTOP\t/repo/a\tmain\t111\n";
        assert!(parse_registry(text).is_empty());
    }
}
