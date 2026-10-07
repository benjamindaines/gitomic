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
//
// Each watcher is bound to the branch it was started for (issue #4), which is the branch the operator was
// standing on when `gitomic init` ran. A single work tree can only have one branch checked out at a time, so a
// mid-session `git checkout`/`git switch` is detected, not prevented: on every debounce-elapsed cycle and on
// shutdown, the watcher compares the currently checked-out branch against its own and stands down (skips the
// capture, or drops rather than persists observed-but-uncommitted paths) whenever they differ, rather than
// blindly committing onto whatever branch happens to be current. Running `gitomic init` on the newly
// checked-out branch starts that branch's own watcher with its own pidfile and base marker; switching back
// makes the original watcher active again with no re-init needed, since it never stopped polling — it was only
// refusing to act while its branch was not the one checked out.
//
// That comparison is against the operator's own branch, which is what makes standing down a rare and correct
// outcome rather than a permanent one. Under issue #10 the watcher was bound to a private branch that `init`
// checked out, so any return of HEAD to the operator's branch satisfied the mismatch condition and muted the
// watcher for the rest of the session while it went on reporting itself as running (issue #22). Recording in
// place leaves nothing for HEAD to be away from.
//
// A commit that reaches base..<branch> without being one of this watcher's placeholders — an out-of-band
// commit, a merge, a pull that advanced the branch mid-session — is reported in the log and does not stop the
// capture. Recording continues deliberately: silently declining to record is the failure mode this issue is
// about, and a foreign commit is something `finish` can account for (it restamps only the placeholders and
// returns a foreign commit its own message) rather than a reason to stop capturing the operator's work. The
// decision about whether the batch can be finalized belongs to `finish`, where it is reported to the operator
// instead of to a log file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::config::{glob_match, Config, StageMode};
use crate::{git, proc, Res};

// Upper bound on how long the loop sleeps between shutdown-flag checks, so a termination signal is observed
// promptly even when the debounce window is long or the tree is idle.
const POLL: Duration = Duration::from_millis(200);

// Run the watch loop until a shutdown signal arrives. `root` is the work tree, `git_dir` the repository's git
// directory, `branch` the branch this watcher's session belongs to (the branch checked out when `gitomic init`
// started it). Diagnostics are written to the already-redirected stdout/stderr.
pub fn run(root: &Path, git_dir: &Path, branch: &str, cfg: &Config, verbose: bool) -> Res<()> {
    let debounce = Duration::from_millis(cfg.debounce_ms);
    let git_dir = git_dir.to_path_buf();
    let base_ref = git::base_ref(branch);

    let (tx, rx) = channel::<Vec<PathBuf>>();
    // The event handler forwards the external, non-ignored paths each event names; the debounce timer, not
    // the event payload, drives commits, but the paths are retained so observed staging can act on exactly
    // the paths that changed. Two filters run here rather than at staging time: paths inside the git
    // directory, so watcher-induced writes never rearm the timer or enter the observed set, and paths matching
    // a configured ignore pattern (issue #8) — editor swap/lock/backup files whose own create/remove churn
    // would otherwise arm the timer independently of the file actually being edited, and whose alternation
    // with that file's real changes defeats coalesce_same_file's exact-set match. Filtering here, rather than
    // in stage_observed, means such churn never enters the observed set at all, so debounce cycles form around
    // the real edits instead of being fragmented by the noise around them. When verbose tracing is enabled,
    // every received event is logged first, before the filter, so the operator sees changes that were dropped
    // for either reason as well as those that armed the timer.
    let gd = git_dir.clone();
    let ignore_patterns = cfg.ignore_patterns.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                let external: Vec<PathBuf> = event
                    .paths
                    .iter()
                    .filter(|p| !is_within(p, &gd) && !is_ignored_path(p, &ignore_patterns))
                    .cloned()
                    .collect();
                if verbose {
                    trace_event(&event, &gd, &ignore_patterns);
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
    let sdir = git::state_dir(&git_dir, branch);

    log(&format!(
        "watching {} for branch '{branch}' (debounce {} ms)",
        root.display(),
        cfg.debounce_ms
    ));

    // The watch is armed before the sweep, so an edit made while the sweep runs is observed as well.
    startup_sweep(root, &git_dir, branch);

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
            // state directory; an empty set clears any stale file from a prior session. If the checked-out
            // branch no longer matches this watcher's branch, whatever was observed happened while this
            // session was not the active one and belongs to no capture of this watcher's — it is dropped
            // rather than persisted, the same as an empty set.
            let on_branch = git::current_branch(root)
                .map(|c| c == branch)
                .unwrap_or(false);
            if !on_branch || observed.is_empty() {
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
                // A mid-session `checkout`/`switch` away from this watcher's branch must not commit onto
                // whatever branch is now checked out (issue #4's root cause). Observed paths from a mismatched
                // period describe changes on someone else's branch or session and are simply discarded, not
                // captured under any base; capture resumes automatically once `branch` is checked out again.
                match git::current_branch(root) {
                    Ok(cur) if cur == branch => {
                        if verbose {
                            log("debounce window elapsed; running commit cycle");
                        }
                        // Reported once per cycle in which it is present rather than tracked across cycles: the
                        // condition persists until the operator acts on it, and a log line per debounce window
                        // is the signal that something landed in the batch from outside.
                        if let Some((id, subject)) = foreign_in_batch(root, &base_ref, "HEAD") {
                            let what = if subject.is_empty() {
                                "a merge or an empty-message commit".to_string()
                            } else {
                                format!("'{subject}'")
                            };
                            log(&format!(
                                "note: the pending batch contains a commit this session did not record \
                                 ({} — {what}); recording continues, and 'gitomic status' reports it",
                                short_id(&id)
                            ));
                        }
                        commit_cycle(root, &git_dir, &base_ref, cfg, verbose, &observed);
                    }
                    Ok(cur) => log(&format!(
                        "checked-out branch is '{cur}', this session is for '{branch}'; skipping capture until \
                         '{branch}' is checked out again"
                    )),
                    Err(_) => log(&format!(
                        "HEAD is detached; this session is for '{branch}'; skipping capture"
                    )),
                }
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
    // Tracked files only, with submodules left out entirely (see `stage_tracked_files`).
    TrackedFiles,
    All,
}

// The first commit in the pending batch that this session did not record, if any. An atomic commit carries an
// empty message and exactly one parent, so a commit in base..tip with either a subject or a second parent came
// from somewhere else: an out-of-band commit, a merge, or a pull that landed on the branch mid-session. Returned
// rather than logged so the caller reports it in its own register. A read failure yields None so that an
// unreadable range degrades to the prior behaviour of attempting the capture rather than blocking it; `finish`
// checks the same range again before rewriting anything, so a missed detection here is not the last line of
// defence.
pub(crate) fn foreign_in_batch(root: &Path, base_ref: &str, tip: &str) -> Option<(String, String)> {
    if base_ref.is_empty() {
        return None;
    }
    let summaries = git::commit_summaries(root, &format!("{base_ref}..{tip}")).ok()?;
    summaries
        .into_iter()
        .find(|(_, parents, subject)| !subject.is_empty() || parents.len() != 1)
        .map(|(id, _, subject)| (id, subject))
}

// Capture one atomic commit on the watcher's configured staging policy. `observed` is the set of paths the
// watcher saw change this cycle, used only under `StageMode::Observed`; the tracked and all modes ignore it.
// Also honours coalesce_same_file, so a capture whose staged paths match the prior atomic commit's paths
// extends that commit rather than starting a new one. A thin selector over flush_staged, which holds the
// shared capture logic.
pub(crate) fn flush_once(
    root: &Path,
    git_dir: &Path,
    base_ref: &str,
    cfg: &Config,
    observed: &BTreeSet<String>,
) -> Flush {
    let breadth = match cfg.stage {
        StageMode::Observed => Breadth::Observed(observed),
        StageMode::Tracked => Breadth::Tracked,
        StageMode::All => Breadth::All,
    };
    flush_staged(root, git_dir, base_ref, breadth, cfg.coalesce_same_file)
}

// Capture one atomic commit staging every change including untracked files (`git add -A`), irrespective of the
// configured staging policy. Used by `exec`, where the operator has explicitly wrapped a command to record its
// result: a file the command created is part of that intended result, so it must be staged. The watcher itself
// never calls this; it honours the configured policy. Same-file coalescing is never applied here: an exec
// capture is a deliberate, explicitly requested result and always stands on its own, whether or not it happens
// to touch the same paths as the preceding commit. Coalescing is off, so no base ref is needed to evaluate it;
// the empty string is passed and never read (see flush_staged).
pub(crate) fn flush_all(root: &Path, git_dir: &Path) -> Flush {
    flush_staged(root, git_dir, "", Breadth::All, false)
}

// Record, as one atomic commit, the changes to tracked files that were made while no watcher ran
// (issue #24). The observed set only holds paths seen changing since the watch was armed, so an edit
// made before a session started or resumed would otherwise wait for its file to change again, and a
// restore would overwrite it unrecorded. The sweep stages what `git add -u` stages: modifications
// and deletions of tracked files. Untracked files are left to the watcher, because sweeping them
// would pull build output into the session. It does nothing when the branch checked out is not the
// session's, or when the session has no base marker to commit on top of.
fn startup_sweep(root: &Path, git_dir: &Path, branch: &str) {
    if git::current_branch(root)
        .map(|c| c != branch)
        .unwrap_or(true)
    {
        return;
    }
    if git::rev_parse(root, &git::base_ref(branch)).is_err() {
        return;
    }
    match flush_tracked(root, git_dir) {
        Flush::Committed(sha) if !sha.is_empty() => {
            log(&format!("start-up sweep: recorded earlier edits as {sha}"))
        }
        Flush::Committed(_) => log("start-up sweep: recorded earlier edits"),
        Flush::Nothing => {}
        Flush::Skipped(why) => log(&format!("start-up sweep skipped: {why}")),
        Flush::Failed(msg) => log(&format!("start-up sweep: {msg}")),
    }
}

// Capture one atomic commit staging every change to files git already tracks (`git add -u`),
// irrespective of the configured staging policy and of what the watcher observed. Used before a
// restore (issue #24) so that edits made while no watcher ran are recorded before they are
// overwritten. Untracked files are left alone, and so are submodules. Coalescing is off: the capture stands as its own
// step, which keeps the pre-restore state one `drop` away.
pub(crate) fn flush_tracked(root: &Path, git_dir: &Path) -> Flush {
    flush_staged(root, git_dir, "", Breadth::TrackedFiles, false)
}

// Paths of the submodules (gitlinks) in the index.
fn gitlinks(root: &Path) -> Res<Vec<String>> {
    let out = git::run(root, &["ls-files", "--stage", "-z"])?;
    Ok(out
        .split('\0')
        .filter_map(|rec| rec.strip_prefix("160000 "))
        .filter_map(|rec| rec.split_once('\t').map(|(_, path)| path.to_string()))
        .collect())
}

// Stage modifications and deletions of tracked files (`git add -u`) without touching any submodule.
// A submodule checked out at another commit than the one recorded shows up as a modified path, and
// `git add -u` would record the move; a submodule is the operator's to update deliberately, so its
// pointer is excluded from the pathspec. A pointer the operator already staged would still enter
// the commit, so the capture is refused in that case and the index is left as it is.
fn stage_tracked_files(root: &Path) -> Res<()> {
    let links = gitlinks(root)?;
    if links.is_empty() {
        return git::run(root, &["add", "-u"]).map(|_| ());
    }
    let excludes: Vec<String> = links
        .iter()
        .map(|p| format!(":(exclude,literal){p}"))
        .collect();
    let mut add: Vec<&str> = vec!["add", "-u", "--", "."];
    add.extend(excludes.iter().map(String::as_str));
    git::run(root, &add)?;
    let mut staged: Vec<&str> = vec!["diff", "--cached", "--name-only", "--"];
    staged.extend(links.iter().map(String::as_str));
    if !git::run(root, &staged)?.trim().is_empty() {
        return Err("a submodule pointer is staged; stage or reset it first".into());
    }
    Ok(())
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
// `base_ref` names the active session's base marker, used only when `coalesce` is true (see
// same_paths_as_last_commit); a caller that never coalesces (flush_all) may pass an empty string.
fn flush_staged(
    root: &Path,
    git_dir: &Path,
    base_ref: &str,
    breadth: Breadth,
    coalesce: bool,
) -> Flush {
    if git::operation_in_progress(git_dir) {
        return Flush::Skipped("multi-step git operation in progress".to_string());
    }
    if git::index_locked(git_dir) {
        return Flush::Skipped("index locked by another git process".to_string());
    }

    let staged = match breadth {
        Breadth::Observed(obs) => stage_observed(root, obs),
        Breadth::Tracked => git::run(root, &["add", "-u"]).map(|_| ()),
        Breadth::TrackedFiles => stage_tracked_files(root),
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

    if coalesce && same_paths_as_last_commit(root, base_ref) {
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
fn same_paths_as_last_commit(root: &Path, base_ref: &str) -> bool {
    let base = match git::rev_parse(root, base_ref) {
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
    // Never amend a commit a remote already holds. Amending it rewrites its object id, which diverges the branch
    // from its remote-tracking ref with no indication that anything happened — a push during a session is enough
    // to reach this, since recording continues on the branch that was pushed. A fresh commit is always safe, so
    // coalescing simply declines. A failure to read the published set declines for the same reason.
    match git::published_in_range(root, &format!("{base_ref}..HEAD")) {
        Ok(published) => {
            if published.contains(&head) {
                return false;
            }
        }
        Err(_) => return false,
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
    base_ref: &str,
    cfg: &Config,
    verbose: bool,
    observed: &BTreeSet<String>,
) {
    match flush_once(root, git_dir, base_ref, cfg, observed) {
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

// Whether `path`'s basename matches one of `patterns` (see `config::glob_match`). A path with no basename
// component is never matched on that account alone. Standalone rather than a `Config` method so the event
// closure can capture just the cloned pattern list, not the whole `Config`.
fn is_ignored_path(path: &Path, patterns: &[String]) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => patterns.iter().any(|p| glob_match(p, name)),
        None => false,
    }
}

// Emit one diagnostic line per received file-system event under verbose tracing: the event kind and the paths
// it names, tagged by whether the path lies outside the git directory and does not match a configured ignore
// pattern (arming the debounce timer), lies inside the git directory (ignored to avoid a commit feedback
// loop), or matches an ignore pattern (ignored as editor swap/lock/backup noise, issue #8). This is the
// primary instrument for confirming whether a given change — a patch application, an editor save, a scripted
// file write — is observed by the watcher at all, as opposed to being lost before the recursive watch was
// established or filtered as noise.
fn trace_event(event: &notify::Event, git_dir: &Path, ignore_patterns: &[String]) {
    let tagged: Vec<String> = event
        .paths
        .iter()
        .map(|p| {
            let tag = if is_within(p, git_dir) {
                "ignored (git-internal)"
            } else if is_ignored_path(p, ignore_patterns) {
                "ignored (editor swap/lock/backup pattern)"
            } else {
                "armed"
            };
            format!("{} [{}]", p.display(), tag)
        })
        .collect();
    log(&format!("event {:?} -> {}", event.kind, tagged.join(", ")));
}

// Abbreviate an object id for a log line. Local to the watcher so a diagnostic never depends on the command
// layer, which formats for a terminal rather than a log.
fn short_id(sha: &str) -> String {
    sha.chars().take(12).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_IGNORE_PATTERNS;

    fn default_patterns() -> Vec<String> {
        DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn ignores_kate_swap_file_by_basename() {
        let patterns = default_patterns();
        assert!(is_ignored_path(
            Path::new("/repo/rom/.install.sh.kate-swp"),
            &patterns
        ));
    }

    #[test]
    fn does_not_ignore_the_real_file() {
        let patterns = default_patterns();
        assert!(!is_ignored_path(
            Path::new("/repo/rom/install.sh"),
            &patterns
        ));
    }

    #[test]
    fn ignore_check_is_basename_only_not_full_path() {
        // A directory that happens to be named like a pattern must not make every file beneath it ignored;
        // only the basename is tested.
        let patterns = vec!["*.kate-swp".to_string()];
        assert!(!is_ignored_path(
            Path::new("/repo/foo.kate-swp/real_file.rs"),
            &patterns
        ));
    }

    fn session() -> crate::testrepo::Repo {
        let r = crate::testrepo::Repo::new();
        r.write("a.txt", "a\n");
        r.write("b.txt", "b\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        r.git(&["update-ref", &git::base_ref("main"), "HEAD"]);
        r
    }

    #[test]
    fn the_sweep_records_edits_and_deletions_of_tracked_files_as_one_commit() {
        let r = session();
        let base = r.head();
        r.write("a.txt", "edited while no watcher ran\n");
        std::fs::remove_file(r.0.join("b.txt")).unwrap();
        r.write("new.txt", "untracked\n");
        let git_dir = git::git_dir(&r.0).unwrap();
        startup_sweep(&r.0, &git_dir, "main");
        assert_ne!(r.head(), base);
        assert_eq!(
            r.git(&["rev-list", "--count", &format!("{base}..HEAD")]),
            "1"
        );
        assert_eq!(r.git(&["log", "-1", "--format=%s"]), "");
        assert_eq!(
            r.git(&["show", "HEAD:a.txt"]),
            "edited while no watcher ran"
        );
        assert!(r
            .git(&["ls-tree", "--name-only", "HEAD"])
            .lines()
            .all(|f| f != "b.txt"));
        assert_eq!(r.git(&["status", "--porcelain"]), "?? new.txt");
    }

    #[test]
    fn the_sweep_leaves_a_moved_submodule_pointer_alone_and_refuses_a_staged_one() {
        let sub = crate::testrepo::Repo::new();
        sub.write("f", "1\n");
        sub.git(&["add", "."]);
        sub.git(&["commit", "-q", "-m", "one"]);
        sub.commit_file("f", "2\n", "two");
        let r = crate::testrepo::Repo::new();
        r.write("a.txt", "a\n");
        r.git(&["add", "."]);
        r.git(&["commit", "-q", "-m", "base"]);
        let url = sub.0.to_string_lossy().to_string();
        r.git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &url,
            "sub",
        ]);
        r.git(&["commit", "-q", "-m", "add submodule"]);
        r.git(&["update-ref", &git::base_ref("main"), "HEAD"]);
        let base = r.head();
        let git_dir = git::git_dir(&r.0).unwrap();
        // The submodule moves to another commit and a tracked file is edited.
        r.git(&["-C", "sub", "checkout", "-q", "HEAD~1"]);
        r.write("a.txt", "edited\n");
        startup_sweep(&r.0, &git_dir, "main");
        assert_ne!(r.head(), base);
        assert_eq!(r.git(&["show", "HEAD:a.txt"]), "edited");
        assert_eq!(
            r.git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]),
            "a.txt"
        );
        assert_eq!(
            r.git(&["status", "--porcelain"]),
            " M sub",
            "the pointer stays a local edit"
        );
        // A pointer the operator staged is not swept in; nothing is committed.
        r.git(&["add", "sub"]);
        r.write("a.txt", "edited again\n");
        let head = r.head();
        startup_sweep(&r.0, &git_dir, "main");
        assert_eq!(r.head(), head);
    }

    #[test]
    fn the_sweep_does_nothing_on_a_clean_tree_another_branch_or_without_a_session() {
        let r = session();
        let head = r.head();
        let git_dir = git::git_dir(&r.0).unwrap();
        startup_sweep(&r.0, &git_dir, "main");
        assert_eq!(r.head(), head, "clean tree");
        r.write("a.txt", "edit\n");
        startup_sweep(&r.0, &git_dir, "other");
        assert_eq!(
            r.head(),
            head,
            "the branch checked out is not the session's"
        );
        r.git(&["update-ref", "-d", &git::base_ref("main")]);
        startup_sweep(&r.0, &git_dir, "main");
        assert_eq!(r.head(), head, "no session base");
    }

    #[test]
    fn is_within_matches_git_dir_prefix() {
        let git_dir = Path::new("/repo/.git");
        assert!(is_within(Path::new("/repo/.git/index.lock"), git_dir));
        assert!(!is_within(Path::new("/repo/src/main.rs"), git_dir));
    }
}
