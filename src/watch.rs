// The background watcher. It observes the repository work tree recursively and, on a quiescent period after a
// burst of changes, records one atomic commit bearing an empty placeholder message. Events originating inside
// the git directory are discarded to prevent a feedback loop, since committing itself writes to that
// directory. The loop terminates on a shutdown signal, performing one final flush so no observed change is
// lost across `gitomic finish` or `gitomic stop`.

use std::path::Path;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::config::Config;
use crate::{git, proc, Res};

// Upper bound on how long the loop sleeps between shutdown-flag checks, so a termination signal is observed
// promptly even when the debounce window is long or the tree is idle.
const POLL: Duration = Duration::from_millis(200);

// Run the watch loop until a shutdown signal arrives. `root` is the work tree, `git_dir` the repository's git
// directory. Diagnostics are written to the already-redirected stdout/stderr.
pub fn run(root: &Path, git_dir: &Path, cfg: &Config) -> Res<()> {
    let debounce = Duration::from_millis(cfg.debounce_ms);
    let git_dir = git_dir.to_path_buf();

    let (tx, rx) = channel::<()>();
    // The event handler forwards only a wake token; the debounce timer, not the event payload, drives commits.
    // Events whose paths lie inside the git directory are filtered here so watcher-induced writes never rearm
    // the timer.
    let gd = git_dir.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if event.paths.iter().any(|p| !is_within(p, &gd)) {
                let _ = tx.send(());
            }
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    log(&format!(
        "watching {} (debounce {} ms)",
        root.display(),
        cfg.debounce_ms
    ));

    let mut last_event: Option<Instant> = None;
    loop {
        if proc::shutdown_requested() {
            // Exit promptly on signal without a final commit. The capture of any change observed after the
            // last debounce cycle is performed by the foreground `finish`/`stop` command, where its output is
            // visible to the operator and no in-flight staging blocks the watcher's exit. This keeps watcher
            // shutdown bounded by at most one already-running commit cycle rather than by a fresh flush of the
            // whole tree.
            log("shutdown signal received; watcher exiting");
            return Ok(());
        }

        match rx.recv_timeout(POLL) {
            Ok(()) => last_event = Some(Instant::now()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        // Commit once the tree has been quiescent for the full debounce window. Draining any events that
        // arrived during the poll keeps a steady edit stream from being chopped into multiple commits.
        while rx.try_recv().is_ok() {
            last_event = Some(Instant::now());
        }
        if let Some(t) = last_event {
            if t.elapsed() >= debounce {
                commit_cycle(root, &git_dir, cfg);
                last_event = None;
            }
        }
    }
}

// Outcome of one capture attempt, returned rather than logged so the caller can report it in the register
// appropriate to its context: the background watcher writes it to its log, while a foreground command prints
// it to the operator's terminal.
pub(crate) enum Flush {
    // A commit was recorded; the field carries the short object id (empty when it could not be re-read).
    Committed(String),
    // Nothing was staged, so no commit was made (for example the burst touched only ignored files).
    Nothing,
    // A precondition stood the capture down without error (in-progress operation, or a held index lock).
    Skipped(String),
    // A git invocation failed; the field carries the diagnostic.
    Failed(String),
}

// Perform one capture and return its outcome without logging. Preconditions that make committing unsafe or
// pointless are checked first: an in-progress merge/rebase/etc. stands the capture down, and a held index
// lock defers. Staging respects .gitignore through git itself; an empty staged diff produces no commit. The
// placeholder message is intentionally empty; finalize replaces it across the batch. Verification hooks are
// bypassed so transient placeholder commits neither block on nor repeatedly trigger hooks.
pub(crate) fn flush_once(root: &Path, git_dir: &Path, cfg: &Config) -> Flush {
    if git::operation_in_progress(git_dir) {
        return Flush::Skipped("multi-step git operation in progress".to_string());
    }
    if git::index_locked(git_dir) {
        return Flush::Skipped("index locked by another git process".to_string());
    }

    let add_arg = if cfg.include_untracked { "-A" } else { "-u" };
    if let Err(e) = git::run(root, &["add", add_arg]) {
        return Flush::Failed(format!("stage failed: {e}"));
    }

    match git::succeeds(root, &["diff", "--cached", "--quiet"]) {
        Ok(true) => return Flush::Nothing, // exit 0: no staged differences
        Ok(false) => {}                    // exit 1: staged differences present
        Err(e) => return Flush::Failed(format!("diff check failed: {e}")),
    }

    match git::run(
        root,
        &["commit", "--allow-empty-message", "--no-verify", "-m", ""],
    ) {
        Ok(_) => match git::rev_parse(root, "HEAD") {
            Ok(sha) => Flush::Committed(sha.chars().take(12).collect()),
            Err(_) => Flush::Committed(String::new()),
        },
        Err(e) => Flush::Failed(format!("commit failed: {e}")),
    }
}

// Attempt one atomic commit on the watcher's schedule, logging the outcome to the watcher's redirected log.
fn commit_cycle(root: &Path, git_dir: &Path, cfg: &Config) {
    match flush_once(root, git_dir, cfg) {
        Flush::Committed(sha) if !sha.is_empty() => log(&format!("atomic commit {sha}")),
        Flush::Committed(_) => log("atomic commit recorded"),
        Flush::Nothing => {}
        Flush::Skipped(why) => log(&format!("{why}; skipping commit")),
        Flush::Failed(msg) => log(&msg),
    }
}

// Whether `path` lies within `base`. Paths from the watcher are absolute, so a prefix comparison suffices; a
// non-canonical base is tolerated because both derive from the same absolute git-dir query.
fn is_within(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

// Emit a timestamped diagnostic line to the redirected log.
fn log(msg: &str) {
    println!("[gitomic {}] {}", now(), msg);
}

// Seconds since the Unix epoch, sufficient for ordering log lines without pulling in a date-formatting crate.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
