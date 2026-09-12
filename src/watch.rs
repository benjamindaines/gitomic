// The background watcher. It observes the repository work tree recursively and, on a quiescent period after a
// burst of changes, records one atomic commit bearing an empty placeholder message. The paths named by each
// external event are accumulated across the debounce window: under the default `observed` staging mode the
// capture stages exactly those paths that git also reports as changed, so a new, renamed, or copy-over file is
// recorded while an untracked file the watcher never saw is left untouched. When the settled paths match the
// paths of the session's most recent atomic commit (coalesce_same_file, on by default), the capture extends
// that commit instead of starting a new one, so returning to the same file across several debounce cycles does
// not fragment into a commit per cycle; moving to a different file still starts a fresh commit. Events
// originating inside the git directory are discarded to prevent a feedback loop, since committing itself
// writes to that directory. On a shutdown signal the watcher does not commit; it persists any observed-but-
// uncommitted paths so the foreground `finish`/`stop` capture can stage them, then exits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::config::{Config, StageMode};
use crate::{git, proc, Res};

// Upper bound on how long the loop sleeps between shutdown-flag checks, so a termination signal is observed
// promptly even when the debounce window is long or the tree is idle.
const POLL: Duration = Duration::from_millis(200);

// Run the watch loop until a shutdown signal arrives. `root` is the work tree, `git_dir` the repository's git
// directory. Diagnostics are written to the already-redirected stdout/stderr.
pub fn run(root: &Path, git_dir: &Path, cfg: &Config, verbose: bool) -> Res<()> {
    let debounce = Duration::from_millis(cfg.debounce_ms);
    let git_dir = git_dir.to_path_buf();

    let (tx, rx) = channel::<Vec<PathBuf>>();
    // The event handler forwards the external paths each event names; the debounce timer, not the event
    // payload, drives commits, but the paths are retained so observed staging can act on exactly the paths
    // that changed. Paths inside the git directory are filtered here so watcher-induced writes never rearm the
    // timer or enter the observed set. When verbose tracing is enabled, every received event is logged first,
    // before the filter, so the operator sees changes that were ignored as git-internal as well as those that
    // armed the timer.
    let gd = git_dir.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                let external: Vec<PathBuf> = event
                    .paths
                    .iter()
                    .filter(|p| !is_within(p, &gd))
                    .cloned()
                    .collect();
                if verbose {
                    trace_event(&event, !external.is_empty());
                }
                if !external.is_empty() {
                    let _ = tx.send(external);
                }
            }
            Err(e) => {
                if verbose {
                    log(&format!("watch error: {e}"));
                }
            }
        })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    let sdir = state_dir(&git_dir);

    log(&format!(
        "watching {} (debounce {} ms)",
        root.display(),
        cfg.debounce_ms
    ));

    let mut last_event: Option<Instant> = None;
    // Paths observed changing since the last commit cycle, accumulated for observed staging. Cleared after
    // each cycle; persisted on shutdown so a change made after the last cycle is not lost to the foreground
    // final capture.
    let mut observed: BTreeSet<String> = BTreeSet::new();
    loop {
        if proc::shutdown_requested() {
            // Exit promptly on signal without a final commit. The capture of any change observed after the
            // last debounce cycle is performed by the foreground `finish`/`stop` command, where its output is
            // visible to the operator and no in-flight staging blocks the watcher's exit. This keeps watcher
            // shutdown bounded by at most one already-running commit cycle rather than by a fresh flush of the
            // whole tree. The observed-but-uncommitted set is handed to that foreground capture through the
            // state directory; an empty set clears any stale file from a prior session.
            if observed.is_empty() {
                let _ = std::fs::remove_file(pending_observed_path(&sdir));
            } else {
                persist_pending_observed(&sdir, &observed);
            }
            log("shutdown signal received; watcher exiting");
            return Ok(());
        }

        match rx.recv_timeout(POLL) {
            Ok(paths) => {
                record_observed(&mut observed, root, paths);
                last_event = Some(Instant::now());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        // Commit once the tree has been quiescent for the full debounce window. Draining any events that
        // arrived during the poll keeps a steady edit stream from being chopped into multiple commits.
        while let Ok(paths) = rx.try_recv() {
            record_observed(&mut observed, root, paths);
            last_event = Some(Instant::now());
        }
        if let Some(t) = last_event {
            if t.elapsed() >= debounce {
                if verbose {
                    log("debounce window elapsed; running commit cycle");
                }
                commit_cycle(root, &git_dir, cfg, verbose, &observed);
                observed.clear();
                last_event = None;
            }
        }
    }
}

// Add the work-tree-relative form of each observed path to the accumulating set. Absolute paths from the
// watcher are made relative to the work tree so they compare directly against git's reported paths; a path
// that does not lie under the work tree (an unexpected event outside the watched root) is dropped.
fn record_observed(observed: &mut BTreeSet<String>, root: &Path, paths: Vec<PathBuf>) {
    for p in paths {
        if let Ok(rel) = p.strip_prefix(root) {
            let s = rel.to_string_lossy().to_string();
            if !s.is_empty() {
                observed.insert(s);
            }
        }
    }
}

// The state directory holding gitomic's per-repository files, mirroring commands.rs::state_dir.
fn state_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("gitomic")
}

// File under the state directory holding paths observed but not yet committed when the watcher exits.
fn pending_observed_path(sdir: &Path) -> PathBuf {
    sdir.join("PENDING_OBSERVED")
}

// Persist the pending observed-path set (NUL-delimited, work-tree-relative) for the foreground finish/stop
// capture to consume. Best-effort: a failure to write only means the foreground capture proceeds with an empty
// observed set, never a crash.
fn persist_pending_observed(sdir: &Path, observed: &BTreeSet<String>) {
    let mut buf = String::new();
    for p in observed {
        buf.push_str(p);
        buf.push('\u{0}');
    }
    let _ = std::fs::write(pending_observed_path(sdir), buf);
}

// Read and remove the pending observed-path set left by a watcher that exited with uncommitted changes. An
// absent file yields an empty set. Intersecting these paths with the current git status at capture time makes
// a stale file from an earlier session harmless: paths no longer changed are dropped.
pub(crate) fn take_pending_observed(sdir: &Path) -> BTreeSet<String> {
    let path = pending_observed_path(sdir);
    let set = match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .split('\u{0}')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect(),
        Err(_) => BTreeSet::new(),
    };
    let _ = std::fs::remove_file(&path);
    set
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

// Staging breadth for one capture. `Observed` stages the intersection of the supplied observed-path set with
// git's reported changes; `Tracked` and `All` map to `git add -u` and `git add -A` respectively.
enum Breadth<'a> {
    Observed(&'a BTreeSet<String>),
    Tracked,
    All,
}

// Capture one atomic commit on the watcher's configured staging policy. `observed` is the set of paths the
// watcher saw change this cycle, used only under `StageMode::Observed`; the tracked and all modes ignore it.
// Also honours coalesce_same_file, so a capture whose staged paths match the prior atomic commit's paths
// extends that commit rather than starting a new one. A thin selector over flush_staged, which holds the
// shared capture logic.
pub(crate) fn flush_once(
    root: &Path,
    git_dir: &Path,
    cfg: &Config,
    observed: &BTreeSet<String>,
) -> Flush {
    let breadth = match cfg.stage {
        StageMode::Observed => Breadth::Observed(observed),
        StageMode::Tracked => Breadth::Tracked,
        StageMode::All => Breadth::All,
    };
    flush_staged(root, git_dir, breadth, cfg.coalesce_same_file)
}

// Capture one atomic commit staging every change including untracked files (`git add -A`), irrespective of the
// configured staging policy. Used by `exec`, where the operator has explicitly wrapped a command to record its
// result: a file the command created is part of that intended result, so it must be staged. The watcher itself
// never calls this; it honours the configured policy. Same-file coalescing is never applied here: an exec
// capture is a deliberate, explicitly requested result and always stands on its own, whether or not it happens
// to touch the same paths as the preceding commit.
pub(crate) fn flush_all(root: &Path, git_dir: &Path) -> Flush {
    flush_staged(root, git_dir, Breadth::All, false)
}

// Stage the paths the watcher observed changing this cycle, intersected with the paths git reports as actually
// changed. The intersection is what makes observed staging both precise and robust: ignored files, paths with
// no real change, and stale events for paths that have since vanished all drop out, so a single disappeared
// temp file cannot abort the stage and an untracked file the watcher never saw is never swept into the
// session. The retained paths are staged with `-A` breadth, so a rename observed as a delete plus a create is
// recorded as a rename and a newly created file is captured, which tracked-only staging (`git add -u`) would
// miss.
fn stage_observed(root: &Path, observed: &BTreeSet<String>) -> Res<()> {
    if observed.is_empty() {
        return Ok(());
    }
    let changed = git::status_paths(root)?;
    let to_stage: Vec<&str> = observed
        .iter()
        .filter(|p| changed.contains(p.as_str()))
        .map(|s| s.as_str())
        .collect();
    git::add_pathspec(root, &to_stage)
}

// Shared capture body. `breadth` selects which paths are staged. `coalesce` enables the same-file policy: when
// the staged paths exactly match the paths recorded by the most recent atomic commit in the active session,
// the capture amends that commit instead of creating a new one, so a burst of saves that keeps returning to
// the same file(s) collapses to one commit rather than one per debounce cycle. A capture that touches a
// different file, an additional file, or fewer files than the prior commit is judged distinct and starts a
// fresh commit. Preconditions that make committing unsafe or pointless are checked first: an in-progress
// merge/rebase/etc. stands the capture down, and a held index lock defers. Staging respects .gitignore through
// git itself; an empty staged diff produces no commit. The placeholder message is intentionally empty;
// finalize replaces it across the batch. Verification hooks are bypassed so transient placeholder commits
// neither block on nor repeatedly trigger hooks.
fn flush_staged(root: &Path, git_dir: &Path, breadth: Breadth, coalesce: bool) -> Flush {
    if git::operation_in_progress(git_dir) {
        return Flush::Skipped("multi-step git operation in progress".to_string());
    }
    if git::index_locked(git_dir) {
        return Flush::Skipped("index locked by another git process".to_string());
    }

    let staged = match breadth {
        Breadth::Observed(obs) => stage_observed(root, obs),
        Breadth::Tracked => git::run(root, &["add", "-u"]).map(|_| ()),
        Breadth::All => git::run(root, &["add", "-A"]).map(|_| ()),
    };
    if let Err(e) = staged {
        return Flush::Failed(format!("stage failed: {e}"));
    }

    match git::succeeds(root, &["diff", "--cached", "--quiet"]) {
        Ok(true) => return Flush::Nothing, // exit 0: no staged differences
        Ok(false) => {}                    // exit 1: staged differences present
        Err(e) => return Flush::Failed(format!("diff check failed: {e}")),
    }

    if coalesce && same_paths_as_last_commit(root) {
        return match git::run(
            root,
            &[
                "commit",
                "--amend",
                "--allow-empty-message",
                "--no-edit",
                "--no-verify",
            ],
        ) {
            Ok(_) => match git::rev_parse(root, "HEAD") {
                Ok(sha) => Flush::Committed(sha.chars().take(12).collect()),
                Err(_) => Flush::Committed(String::new()),
            },
            Err(e) => Flush::Failed(format!("amend failed: {e}")),
        };
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

// Whether the currently staged paths are exactly the paths recorded by the most recent commit in the active
// session, so the pending capture should extend that commit rather than start a new one. False whenever there
// is nothing yet to coalesce into: no active session, or HEAD still sitting at the session base with no prior
// atomic commit. Any failure to read git state is treated as "not a match" so coalescing degrades to the
// always-safe behaviour of a fresh commit rather than guessing.
fn same_paths_as_last_commit(root: &Path) -> bool {
    let base = match git::rev_parse(root, git::BASE_REF) {
        Ok(b) if !b.is_empty() => b,
        _ => return false,
    };
    let head = match git::rev_parse(root, "HEAD") {
        Ok(h) if !h.is_empty() => h,
        _ => return false,
    };
    if head == base {
        return false; // first capture of the session; nothing to coalesce into yet
    }
    let (mut prior, mut staged) = match (git::commit_paths(root, "HEAD"), git::staged_paths(root)) {
        (Ok(p), Ok(s)) => (p, s),
        _ => return false,
    };
    if prior.is_empty() {
        return false; // guard against an unexpected empty prior diff matching a likewise-empty staged set
    }
    prior.sort();
    staged.sort();
    prior == staged
}

// Attempt one atomic commit on the watcher's schedule, logging the outcome to the watcher's redirected log.
fn commit_cycle(
    root: &Path,
    git_dir: &Path,
    cfg: &Config,
    verbose: bool,
    observed: &BTreeSet<String>,
) {
    match flush_once(root, git_dir, cfg, observed) {
        Flush::Committed(sha) if !sha.is_empty() => log(&format!("atomic commit {sha}")),
        Flush::Committed(_) => log("atomic commit recorded"),
        // Nothing staged is the common quiescent outcome and is silent by default to keep the log terse; under
        // verbose tracing it is reported so a burst that produced no committable change (only ignored files, or
        // a change already reverted) is distinguishable from a burst that was never observed.
        Flush::Nothing => {
            if verbose {
                log("debounce elapsed but nothing was staged (no commit)");
            }
        }
        Flush::Skipped(why) => log(&format!("{why}; skipping commit")),
        Flush::Failed(msg) => log(&msg),
    }
}

// Whether `path` lies within `base`. Paths from the watcher are absolute, so a prefix comparison suffices; a
// non-canonical base is tolerated because both derive from the same absolute git-dir query.
fn is_within(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

// Emit one diagnostic line per received file-system event under verbose tracing: the event kind and the paths
// it names, tagged by whether any path lies outside the git directory (arming the debounce timer) or the event
// was purely git-internal (ignored to avoid a commit feedback loop). This is the primary instrument for
// confirming whether a given change — a patch application, an editor save, a scripted file write — is observed
// by the watcher at all, as opposed to being lost before the recursive watch was established.
fn trace_event(event: &notify::Event, external: bool) {
    let tag = if external {
        "armed"
    } else {
        "ignored (git-internal)"
    };
    let paths: Vec<String> = event
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    log(&format!(
        "event {:?} [{}] -> {}",
        event.kind,
        paths.join(", "),
        tag
    ));
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
