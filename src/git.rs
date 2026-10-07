// Thin wrappers over the git binary. Every repository operation shells out so that git configuration,
// .gitignore semantics, and object storage are inherited unchanged. All commands are scoped to an explicit
// repository directory via `git -C <dir>` rather than relying on the process working directory.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::Res;

// Ref marking where a branch's gitomic session began (`base_ref(branch)..tip` is that branch's pending batch).
// One marker per branch, so a session on one branch is unaffected by another branch's session and by which
// branch happens to be checked out at any given moment (issue #4: a single, unscoped marker let a mid-session
// branch switch silently misattribute commits to whatever branch was current when the watcher next ran).
// Shared between commands.rs, which owns session lifecycle, and watch.rs, which needs to know whether a prior
// atomic commit already exists in the active session before deciding whether to coalesce into it.
pub fn base_ref(branch: &str) -> String {
    format!("refs/gitomic/base/{branch}")
}

// The single, unscoped session marker used before issue #4's fix. A loose ref at this exact path and the
// per-branch refs under refs/gitomic/base/ cannot coexist (git cannot make "base" both a file and a
// directory), so the two schemes are mutually exclusive on disk. Retained only so `init` and `status` can
// detect a marker left behind by an older binary and report it plainly instead of either failing on the
// resulting ref conflict with an opaque git error, or silently proceeding as if no session existed.
pub const LEGACY_BASE_REF: &str = "refs/gitomic/base";

// Branch names (with the refs/gitomic/base/ prefix stripped) of every branch currently holding an open
// session, in for-each-ref's lexical order. An empty vector means no branch has one; it says nothing about
// LEGACY_BASE_REF, which callers check separately. Backs `status`'s all-branches listing. Named for what it
// returns — the branches that have a session — rather than for the refs it reads: the earlier name
// `session_branches` read as bookkeeping over a set of dedicated branches, which these refs are not.
pub fn branches_with_session(dir: &Path) -> Res<Vec<String>> {
    let out = run(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:strip=3)",
            "refs/gitomic/base",
        ],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

// Per-branch state directory: pidfile, log, and finalize template for one branch's session, nested under the
// repository-wide gitomic state directory by branch name. Shared by commands.rs and watch.rs so the two never
// drift onto different paths for the same session. Branch names cannot contain characters that are unsafe as
// path components (git itself forbids the ones that would be), so no sanitisation is needed; a branch name
// containing '/' (e.g. "feature/foo") nests as subdirectories, mirroring how it already nests under
// refs/heads/.
pub fn state_dir(git_dir: &Path, branch: &str) -> PathBuf {
    git_dir.join("gitomic").join(branch)
}

// A NUL byte separates fields when a single git invocation must return several values, since commit metadata
// may contain newlines but never a NUL. The byte is requested via git's %x00 format token so the argument
// itself stays NUL-free (process arguments cannot contain NUL); git emits the NUL into its output, where it
// is split on below.
const NUL: char = '\u{0}';

// Run git with the given arguments in `dir` and return trimmed stdout. A non-zero exit is converted into an
// error carrying the command's stderr, so callers receive git's own diagnostic rather than a bare code.
pub fn run(dir: &Path, args: &[&str]) -> Res<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {}: {}", args.join(" "), msg.trim()).into());
    }
    Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
}

// Message of the error returned when a `Cancel` fired; callers that cancel on purpose ignore it.
pub const CANCELLED: &str = "cancelled";

// Handle by which another thread abandons a git command that is running or about to run. Firing it
// kills the child process registered by `run_capped`, so a superseded request stops consuming CPU and
// disk at once instead of running to completion. A fired handle stays fired: every later command run
// with it fails at once with `CANCELLED`.
#[derive(Default)]
pub struct Cancel {
    fired: AtomicBool,
    // Process id of the child currently running under this handle; 0 when there is none.
    pid: AtomicU32,
}

impl Cancel {
    pub fn cancel(&self) {
        self.fired.store(true, Ordering::SeqCst);
        let pid = self.pid.load(Ordering::SeqCst);
        if pid != 0 {
            // SAFETY: `kill` only sends a signal. A stale id can at worst name a process that has
            // exited and is reaped by its own parent within the same instant; the id is cleared
            // before the child is waited for.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

// The output of `run_capped`.
pub struct Capped {
    pub text: String,
    // True when the output exceeded the cap and the command was stopped early.
    pub truncated: bool,
}

// Run git and return at most `max` bytes of stdout, converted lossily so that a byte sequence that is
// not UTF-8 is shown rather than turned into an error. When the output is longer than `max` the
// command is killed, so a diff of a huge generated file is never produced in full only to be cut
// afterwards. With a `cancel` handle the command can be abandoned from another thread.
pub fn run_capped(dir: &Path, args: &[&str], max: usize, cancel: Option<&Cancel>) -> Res<Capped> {
    if cancel.is_some_and(Cancel::is_cancelled) {
        return Err(CANCELLED.into());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(c) = cancel {
        c.pid.store(child.id(), Ordering::SeqCst);
        // A cancel that arrived between the check above and the registration kills nothing, so it
        // is honoured here.
        if c.is_cancelled() {
            let _ = child.kill();
        }
    }
    let mut stdout = child.stdout.take().ok_or("git: no stdout pipe")?;
    let mut stderr = child.stderr.take().ok_or("git: no stderr pipe")?;
    // Standard error is drained on its own thread so that a chatty command cannot block on a full pipe
    // while stdout is being read.
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let mut buf = Vec::new();
    let read = (&mut stdout).take(max as u64 + 1).read_to_end(&mut buf);
    let truncated = buf.len() > max;
    if truncated {
        buf.truncate(max);
        let _ = child.kill();
    }
    drop(stdout);
    let status = child.wait();
    if let Some(c) = cancel {
        c.pid.store(0, Ordering::SeqCst);
        if c.is_cancelled() {
            return Err(CANCELLED.into());
        }
    }
    let err = err_thread.join().unwrap_or_default();
    read?;
    let status = status?;
    if !truncated && !status.success() {
        let msg = String::from_utf8_lossy(&err);
        return Err(format!("git {}: {}", args.join(" "), msg.trim()).into());
    }
    Ok(Capped {
        text: String::from_utf8_lossy(&buf).trim_end().to_string(),
        truncated,
    })
}

// Run a prepared git command to completion and return its whole output, like `Command::output`,
// but abandonable: with a `cancel` handle the child is killed when it fires and the result is
// `CANCELLED`. For commands whose output is binary or structured and needs no cap.
pub fn output_cancellable(mut cmd: Command, cancel: Option<&Cancel>) -> Res<std::process::Output> {
    let Some(cancel) = cancel else {
        return Ok(cmd.output()?);
    };
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    cancel.pid.store(child.id(), Ordering::SeqCst);
    // A cancel that arrived between the check above and the registration killed nothing.
    if cancel.is_cancelled() {
        unsafe {
            // SAFETY: signals the child just started, which has not been waited for.
            libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
        }
    }
    let out = child.wait_with_output();
    cancel.pid.store(0, Ordering::SeqCst);
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    Ok(out?)
}

// For each spec (`<commit>:<path>`, `<commit>^`, ...) the id of the object it names and its type
// (`blob`, `commit`, ...), or None when it names nothing. One `cat-file --batch-check` process
// answers for all of them, in order.
pub fn resolve_objects(dir: &Path, specs: &[String]) -> Res<Vec<Option<(String, String)>>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("git cat-file: no stdin pipe")?;
    // A spec holding a line break could not be told apart from two specs; it names nothing.
    let input: String = specs
        .iter()
        .map(|s| format!("{}\n", s.replace('\n', " ")))
        .collect();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    let text = String::from_utf8_lossy(&out.stdout);
    let answers: Vec<Option<(String, String)>> = text
        .lines()
        .map(|l| {
            let mut f = l.split(' ');
            match (f.next(), f.next(), f.next()) {
                (Some(id), Some(kind), None)
                    if kind != "missing" && id.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    Some((id.to_string(), kind.to_string()))
                }
                _ => None,
            }
        })
        .collect();
    if answers.len() != specs.len() {
        return Err("git cat-file: unexpected number of answers".into());
    }
    Ok(answers)
}

// Sizes in bytes of the objects `ids`, in the same order, read from their headers alone; None for an
// id that names no object. One `cat-file --batch-check` process answers for all of them.
pub fn blob_sizes(dir: &Path, ids: &[String]) -> Res<Vec<Option<u64>>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "--batch-check=%(objectsize)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("git cat-file: no stdin pipe")?;
    let input: String = ids.iter().map(|i| format!("{i}\n")).collect();
    // The ids are written on their own thread, so a long list cannot fill the pipe that the answers
    // are waiting to leave through.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    let text = String::from_utf8_lossy(&out.stdout);
    let sizes: Vec<Option<u64>> = text.lines().map(|l| l.trim().parse::<u64>().ok()).collect();
    if sizes.len() != ids.len() {
        return Err("git cat-file: unexpected number of answers".into());
    }
    Ok(sizes)
}

// Run git purely for its exit status, treating a clean non-zero exit as `false` rather than an error. Used
// for predicate-style commands such as `diff --quiet`, where exit code 1 is a valid answer, not a fault.
pub fn succeeds(dir: &Path, args: &[&str]) -> Res<bool> {
    // Output is captured and discarded rather than inherited, so a predicate command such as
    // `rev-parse --verify` does not print its resolved object to the caller's stdout.
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    Ok(out.status.success())
}

// Run git with every standard stream inherited from this process, rather than captured. Every other wrapper
// in this file captures stdout because its result feeds back into gitomic's own logic; this one is for output
// meant to go straight to a human terminal — `diff` is the first such case — so the caller's pager and color
// configuration apply exactly as they would for git invoked directly, not gitomic's own re-printed text.
pub fn spawn_inherit(dir: &Path, args: &[&str]) -> Res<std::process::ExitStatus> {
    Ok(Command::new("git").arg("-C").arg(dir).args(args).status()?)
}

// Absolute path to the repository work tree containing `start`. Errors when `start` is not inside a work tree.
pub fn work_tree(start: &Path) -> Res<PathBuf> {
    let dir = run(start, &["rev-parse", "--show-toplevel"])?;
    if dir.is_empty() {
        return Err("not inside a git work tree".into());
    }
    Ok(PathBuf::from(dir))
}

// Absolute path to the repository's git directory (resolves through worktrees and GIT_DIR). Errors outside a
// repository.
pub fn git_dir(start: &Path) -> Res<PathBuf> {
    let dir = run(start, &["rev-parse", "--absolute-git-dir"])?;
    if dir.is_empty() {
        return Err("not inside a git repository".into());
    }
    Ok(PathBuf::from(dir))
}

// Resolve any revision expression to a full object id.
pub fn rev_parse(dir: &Path, rev: &str) -> Res<String> {
    run(dir, &["rev-parse", "--verify", "--quiet", rev])
}

// True when `rev` resolves to an existing object; false otherwise. Distinguishes "absent ref" from "git error".
pub fn rev_exists(dir: &Path, rev: &str) -> Res<bool> {
    succeeds(dir, &["rev-parse", "--verify", "--quiet", rev])
}

// Short branch name of the current HEAD. Errors when HEAD is detached, since a detached HEAD gives finalize no
// branch ref to advance.
pub fn current_branch(dir: &Path) -> Res<String> {
    let name = run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|_| "HEAD is detached; gitomic requires a checked-out branch")?;
    Ok(name)
}

// Commits in `range` (e.g. "base..HEAD") oldest-first, one object id per element. An empty range yields an
// empty vector.
pub fn rev_list_reverse(dir: &Path, range: &str) -> Res<Vec<String>> {
    let out = run(dir, &["rev-list", "--reverse", range])?;
    Ok(out.lines().map(|l| l.to_string()).collect())
}

// Commits reachable from HEAD but from no remote-tracking ref, oldest-first, each paired with its
// parent ids. This is the set of commits that have not been published to any known remote, and
// therefore the set whose history may be rewritten without invalidating anything a collaborator
// holds. Without any remote-tracking ref every commit on the branch qualifies. `range` selects the
// walk: `None` uses the unpublished set above, `Some("base..HEAD")` restricts it to an explicit
// revision range (an active session's pending batch).
pub fn commits_with_parents(dir: &Path, range: Option<&str>) -> Res<Vec<(String, Vec<String>)>> {
    let mut args = vec!["rev-list", "--reverse", "--parents"];
    match range {
        Some(r) => args.push(r),
        None => args.extend(["HEAD", "--not", "--remotes"]),
    }
    let out = run(dir, &args)?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut ids = l.split(' ').map(str::to_string);
            let commit = ids.next().unwrap_or_default();
            (commit, ids.collect())
        })
        .collect())
}

// Full commit message (subject and body) of a commit, with trailing whitespace removed. An empty
// result is a valid answer: atomic commits recorded by the watcher carry an empty placeholder
// message.
pub fn commit_message(dir: &Path, commit: &str) -> Res<String> {
    run(dir, &["show", "-s", "--format=%B", commit])
}

// Outcome of applying one commit's change onto a different parent without touching the work tree.
pub enum Pick {
    // The change applied cleanly; carries the id of the resulting tree.
    Tree(String),
    // The change overlaps another change in a way that needs a human decision.
    Conflict,
}

// Compute the tree that results from applying `commit`'s own change (its diff against its first
// parent) onto `onto`, as `git cherry-pick` would, but entirely inside the object database: neither
// the index nor the work tree is read or written, so a session's uncommitted files are never at
// risk. Uses `git merge-tree --write-tree` (git 2.38 or newer), whose exit status is 0 for a clean
// merge and 1 for a conflicted one; any other status is a genuine failure and is returned as an
// error carrying git's stderr.
pub fn cherry_pick_tree(dir: &Path, onto: &str, commit: &str) -> Res<Pick> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["merge-tree", "--write-tree"])
        .arg(format!("--merge-base={commit}^"))
        .arg(onto)
        .arg(commit)
        .output()?;
    match out.status.code() {
        Some(0) => {
            let stdout = String::from_utf8(out.stdout)?;
            let tree = stdout.lines().next().unwrap_or_default().trim().to_string();
            if tree.is_empty() {
                return Err("git merge-tree: no tree id in output".into());
            }
            Ok(Pick::Tree(tree))
        }
        Some(1) => Ok(Pick::Conflict),
        _ => {
            let msg = String::from_utf8_lossy(&out.stderr);
            Err(format!("git merge-tree: {}", msg.trim()).into())
        }
    }
}

// Move the checked-out branch, the index, and the files that differ between the old and new tips to
// `target`, while retaining uncommitted modifications to every other file (`git reset --keep`). The
// command refuses, leaving branch, index, and work tree untouched, when an uncommitted modification
// falls on a file that the move would have to rewrite. `action` labels the reflog entries so the
// move is attributable afterwards.
pub fn reset_keep(dir: &Path, target: &str, action: &str) -> Res<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["reset", "--keep", "--quiet", target])
        .env("GIT_REFLOG_ACTION", action)
        .output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(msg.trim().to_string().into());
    }
    Ok(())
}

// Author name, email, and strict-ISO author date of a commit, preserved verbatim so that a replayed commit
// keeps its original authorship while receiving a fresh committer identity from ambient configuration.
pub fn author_of(dir: &Path, commit: &str) -> Res<(String, String, String)> {
    let out = run(dir, &["show", "-s", "--format=%an%x00%ae%x00%aI", commit])?;
    let mut parts = out.splitn(3, NUL);
    let name = parts.next().unwrap_or_default().to_string();
    let email = parts.next().unwrap_or_default().to_string();
    let date = parts.next().unwrap_or_default().to_string();
    Ok((name, email, date))
}

// Create a commit object from an existing tree and single parent, carrying `message` and the supplied author
// identity. The committer is left to ambient git configuration and the current time, matching standard
// history-rewrite behaviour. Returns the new commit's object id.
pub fn commit_tree(
    dir: &Path,
    tree: &str,
    parent: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
    author_date: &str,
) -> Res<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["commit-tree", tree, "-p", parent, "-m", message])
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_AUTHOR_DATE", author_date)
        .output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git commit-tree: {}", msg.trim()).into());
    }
    Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
}

// Advance a ref to `new` with a compare-and-swap against `old`, so a concurrent modification aborts the update
// rather than silently overwriting it. `reflog` is recorded in the ref's reflog for auditability.
pub fn update_ref_cas(dir: &Path, refname: &str, new: &str, old: &str, reflog: &str) -> Res<()> {
    run(dir, &["update-ref", "-m", reflog, refname, new, old])?;
    Ok(())
}

// Set a ref to a value without a compare-and-swap precondition. Used to plant the session base marker.
pub fn update_ref(dir: &Path, refname: &str, value: &str, reflog: &str) -> Res<()> {
    run(dir, &["update-ref", "-m", reflog, refname, value])?;
    Ok(())
}

// Delete a ref if present; a missing ref is not an error, so session teardown is idempotent.
pub fn delete_ref(dir: &Path, refname: &str) -> Res<()> {
    if rev_exists(dir, refname)? {
        run(dir, &["update-ref", "-d", refname])?;
    }
    Ok(())
}

// True when a local branch of this name exists. The refs/heads/ prefix distinguishes a branch from a tag or a
// remote-tracking ref of the same short name, so a session branch name is tested against branches alone.
pub fn branch_exists(dir: &Path, name: &str) -> Res<bool> {
    rev_exists(dir, &format!("refs/heads/{name}"))
}

// Switch HEAD and the work tree to an existing branch. Used at session end to return to the branch the operator
// began on. Uncommitted modifications that do not collide with the switch are carried across by git; a switch
// that would overwrite local changes fails and the error is returned rather than forcing the move.
pub fn checkout(dir: &Path, name: &str) -> Res<()> {
    run(dir, &["checkout", "-q", name])?;
    Ok(())
}

// Delete a local branch unconditionally (`git branch -D`), used to retire the ephemeral session branch once its
// commits have been integrated onto the origin branch. The branch being deleted must not be the checked-out one,
// so callers switch to the origin branch first. A missing branch is not an error, so teardown is idempotent.
pub fn delete_branch(dir: &Path, name: &str) -> Res<()> {
    if branch_exists(dir, name)? {
        run(dir, &["branch", "-q", "-D", name])?;
    }
    Ok(())
}

// True when `ancestor` is an ancestor of (or equal to) `descendant`. Distinguishes a fast-forward advance of the
// origin branch (base is still an ancestor of the moved tip) from a divergence (the two share only an older
// commit), which decides whether the finalized batch can be replayed onto the moved tip or must be left on the
// session branch for manual reconciliation.
pub fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Res<bool> {
    succeeds(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
}

// Number of commits reachable in `range`, used to report the pending batch size without materialising the list.
pub fn count(dir: &Path, range: &str) -> Res<usize> {
    let out = run(dir, &["rev-list", "--count", range])?;
    Ok(out.trim().parse().unwrap_or(0))
}

// The effective editor git would launch, resolving GIT_EDITOR, core.editor, VISUAL, EDITOR, and the built-in
// default in git's own precedence order.
pub fn editor(dir: &Path) -> Res<String> {
    run(dir, &["var", "GIT_EDITOR"])
}

// The interrupted multi-step git operation a repository is currently sitting in the middle of. Carried as a
// value rather than a bare boolean so callers can name the operation in diagnostics and select the git
// invocation that clears it; the two were previously collapsed into one predicate, which forced every message
// to list all five possibilities and left no way to act on the state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl InProgress {
    // Operation name as git itself spells it, for diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            InProgress::Merge => "merge",
            InProgress::Rebase => "rebase",
            InProgress::CherryPick => "cherry-pick",
            InProgress::Revert => "revert",
            InProgress::Bisect => "bisect",
        }
    }

    // Argument vector that abandons the operation and returns the repository to the state preceding it.
    // Bisect is the outlier: it has no --abort, its equivalent being `bisect reset`.
    pub fn abort_args(self) -> &'static [&'static str] {
        match self {
            InProgress::Merge => &["merge", "--abort"],
            InProgress::Rebase => &["rebase", "--abort"],
            InProgress::CherryPick => &["cherry-pick", "--abort"],
            InProgress::Revert => &["revert", "--abort"],
            InProgress::Bisect => &["bisect", "reset"],
        }
    }

    // The same invocation as a command line, for printing.
    pub fn abort_command(self) -> String {
        format!("git {}", self.abort_args().join(" "))
    }
}

// Which interrupted multi-step operation is in progress, if any, identified by the marker git leaves in the
// git directory. Order matters: a rebase that stops on a conflict leaves rebase-merge/rebase-apply and may
// additionally leave CHERRY_PICK_HEAD or REVERT_HEAD behind, and `git cherry-pick --abort` is not the command
// that clears a rebase, so the rebase markers are tested first. BISECT_LOG is included because a bisect is a
// multi-step operation on the same footing as the others, and its absence from the marker set meant a bisect
// went undetected while the resulting diagnostics claimed to cover it.
pub fn operation_kind(git_dir: &Path) -> Option<InProgress> {
    const MARKERS: [(&str, InProgress); 7] = [
        ("rebase-merge", InProgress::Rebase),
        ("rebase-apply", InProgress::Rebase),
        ("CHERRY_PICK_HEAD", InProgress::CherryPick),
        ("sequencer", InProgress::CherryPick),
        ("REVERT_HEAD", InProgress::Revert),
        ("MERGE_HEAD", InProgress::Merge),
        ("BISECT_LOG", InProgress::Bisect),
    ];
    MARKERS
        .iter()
        .find(|(m, _)| git_dir.join(m).exists())
        .map(|(_, kind)| *kind)
}

// True when an interrupted multi-step operation is in progress. Auto-committing during a merge, rebase,
// cherry-pick, revert, or bisect would corrupt the operation's expected state, so the watcher stands down.
pub fn operation_in_progress(git_dir: &Path) -> bool {
    operation_kind(git_dir).is_some()
}

// Paths (relative to the work tree) changed by `commit` against its first parent. Used to compare a
// candidate commit's staged paths with the paths recorded by the most recent atomic commit in the active
// session, for the same-file coalescing policy in watch.rs. `commit` is expected to have a parent (true for
// any commit reachable from a session base), so `commit~1` is a valid endpoint.
pub fn commit_paths(dir: &Path, commit: &str) -> Res<Vec<String>> {
    let out = run(
        dir,
        &["diff", "--name-only", &format!("{commit}~1"), commit],
    )?;
    Ok(out.lines().map(|l| l.to_string()).collect())
}

// Paths (relative to the work tree) currently staged in the index.
pub fn staged_paths(dir: &Path) -> Res<Vec<String>> {
    let out = run(dir, &["diff", "--cached", "--name-only"])?;
    Ok(out.lines().map(|l| l.to_string()).collect())
}

// The set of work-tree-relative paths git reports as changed (porcelain v1, NUL-delimited). Ignored files are
// omitted by porcelain and unchanged files never appear, so intersecting an observed-path set with this set
// yields exactly the real, non-ignored changes among the observed paths — the basis of observed staging. A
// rename or copy record contributes both its destination and its source path, so either half of a rename can
// match an observed path.
pub fn status_paths(dir: &Path) -> Res<HashSet<String>> {
    // -z uses NUL terminators and suppresses path quoting, so paths with spaces or unusual bytes are parsed
    // verbatim rather than through git's C-style quoting.
    let out = run(dir, &["status", "--porcelain", "-z"])?;
    let mut set = HashSet::new();
    let mut fields = out.split(NUL).filter(|s| !s.is_empty());
    while let Some(entry) = fields.next() {
        // Each record is "XY <path>": two status columns, a separator space, then the path.
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        set.insert(entry[3..].to_string());
        // A rename/copy record is followed by its source path as a separate NUL-terminated field; consume and
        // record it so a rename observed as delete+create matches on either name.
        if status.as_bytes().iter().any(|&b| b == b'R' || b == b'C') {
            if let Some(src) = fields.next() {
                set.insert(src.to_string());
            }
        }
    }
    Ok(set)
}

// Stage the given work-tree-relative paths as one operation. `-A` breadth within the supplied pathspec records
// modifications, deletions, and newly created files alike, so a rename observed as a delete plus a create is
// staged as a rename. Paths are fed NUL-delimited on stdin (`--pathspec-from-file=-` with `--pathspec-file-nul`)
// to sidestep argument-length limits and any quoting concerns. Callers pass only paths git already reports as
// changed, so no pathspec fails to match; an empty list is a no-op.
pub fn add_pathspec(dir: &Path, paths: &[&str]) -> Res<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut input = String::new();
    for p in paths {
        input.push_str(p);
        input.push(NUL);
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        // The pipe is closed by dropping the handle at the end of this block, signalling end-of-input to git.
        let mut stdin = child.stdin.take().ok_or("git add: stdin unavailable")?;
        stdin.write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git add (pathspec): {}", msg.trim()).into());
    }
    Ok(())
}

// True when git holds the index lock, indicating another git process is mid-write. The watcher retries on a
// later cycle rather than contending for the lock.
pub fn index_locked(git_dir: &Path) -> bool {
    git_dir.join("index.lock").exists()
}

// Raw bytes of a blob or of a `<tree>:<path>` entry. The other wrappers here return trimmed UTF-8
// text; file content may be neither, so this one returns the bytes untouched.
pub fn cat_blob(dir: &Path, spec: &str) -> Res<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "blob", spec])
        .output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git cat-file {spec}: {}", msg.trim()).into());
    }
    Ok(out.stdout)
}

// Store `bytes` as a loose blob and return its object id.
pub fn hash_object_write(dir: &Path, bytes: &[u8]) -> Res<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or("git hash-object: stdin unavailable")?;
        stdin.write_all(bytes)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git hash-object: {}", msg.trim()).into());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

// Record exactly the given work-tree-relative paths as one commit with an empty message, leaving any
// other staged change out of it. The paths are staged first so that a newly created file is known to
// the index; `--only` then restricts the commit to them. Hooks are bypassed, matching the watcher's
// atomic commits. Returns the new commit's object id, or None when the paths held no change.
pub fn commit_only(dir: &Path, paths: &[&str]) -> Res<Option<String>> {
    if paths.is_empty() {
        return Ok(None);
    }
    add_pathspec(dir, paths)?;
    let mut input = String::new();
    for p in paths {
        input.push_str(p);
        input.push(NUL);
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "commit",
            "--only",
            "--allow-empty-message",
            "--no-verify",
            "--quiet",
            "-m",
            "",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().ok_or("git commit: stdin unavailable")?;
        stdin.write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        // Nothing to commit is an answer, not a fault: the paths already matched HEAD.
        if text.contains("nothing to commit") || text.contains("no changes added") {
            return Ok(None);
        }
        return Err(format!("git commit: {}", text.trim()).into());
    }
    Ok(Some(rev_parse(dir, "HEAD")?))
}

// One line per commit in `range`, oldest first: object id, parent ids, and subject. A single `git log` call
// rather than one invocation per commit, so auditing a long batch costs one process. NUL separates the three
// fields: git forbids NUL inside a commit message, and `%s` is the subject alone, so every record occupies
// exactly one line and no field can contain either delimiter.
pub fn commit_summaries(dir: &Path, range: &str) -> Res<Vec<(String, Vec<String>, String)>> {
    let out = run(dir, &["log", "--reverse", "--format=%H%x00%P%x00%s", range])?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut f = l.splitn(3, NUL);
            let id = f.next().unwrap_or_default().to_string();
            let parents = f
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect();
            let subject = f.next().unwrap_or_default().to_string();
            (id, parents, subject)
        })
        .collect())
}

// Commits in `range` that a remote-tracking ref already contains, oldest first. Computed as the range minus the
// set git reports as reachable from no remote, so a repository with no remote-tracking refs at all yields an
// empty result (nothing is published) rather than an error. Callers use this to refuse rewriting history that
// has been handed to a remote, since restamping a message produces new object ids and would require a force
// push to publish; `drop` already applies the same rule to its candidate set.
pub fn published_in_range(dir: &Path, range: &str) -> Res<Vec<String>> {
    let all = rev_list_reverse(dir, range)?;
    let unpublished: HashSet<String> = run(dir, &["rev-list", range, "--not", "--remotes"])?
        .lines()
        .map(str::to_string)
        .collect();
    Ok(all
        .into_iter()
        .filter(|c| !unpublished.contains(c))
        .collect())
}

// Path of the pre-push hook for this repository. A hook lives in the git directory, so it is per-clone and
// never committed.
pub fn pre_push_hook_path(git_dir: &Path) -> PathBuf {
    git_dir.join("hooks").join("pre-push")
}

#[cfg(test)]
mod capped_tests {
    use super::*;
    use crate::testrepo::Repo;

    fn repo_with_blob(bytes: &[u8]) -> Repo {
        let r = Repo::new();
        std::fs::write(r.0.join("blob"), bytes).unwrap();
        r.git(&["add", "blob"]);
        r.git(&["commit", "-q", "-m", "blob"]);
        r
    }

    #[test]
    fn output_below_the_cap_is_returned_whole() {
        let r = repo_with_blob(b"hello\n");
        let out = run_capped(&r.0, &["show", "HEAD:blob"], 1024, None).unwrap();
        assert_eq!(out.text, "hello");
        assert!(!out.truncated);
    }

    #[test]
    fn output_above_the_cap_is_cut_and_the_command_stopped() {
        let r = repo_with_blob(&vec![b'x'; 2 * 1024 * 1024]);
        let out = run_capped(&r.0, &["show", "HEAD:blob"], 1000, None).unwrap();
        assert!(out.truncated);
        assert_eq!(out.text.len(), 1000);
    }

    #[test]
    fn bytes_that_are_not_utf8_are_shown_not_refused() {
        let r = repo_with_blob(&[b'a', 0xff, 0xfe, b'b']);
        let out = run_capped(&r.0, &["show", "HEAD:blob"], 1024, None).unwrap();
        assert_eq!(out.text, "a\u{fffd}\u{fffd}b");
    }

    #[test]
    fn a_failing_command_reports_gits_own_message() {
        let r = repo_with_blob(b"x");
        let err = run_capped(&r.0, &["show", "HEAD:nothere"], 1024, None)
            .err()
            .expect("fails");
        assert!(err.to_string().contains("nothere"), "{err}");
    }

    #[test]
    fn a_fired_cancel_refuses_to_start_and_stops_a_running_command() {
        let r = repo_with_blob(b"x");
        let cancel = Cancel::default();
        cancel.cancel();
        let err = run_capped(&r.0, &["show", "HEAD:blob"], 1024, Some(&cancel))
            .err()
            .expect("cancelled");
        assert_eq!(err.to_string(), CANCELLED);

        // A command that would wait forever: git opens a named pipe that nothing writes to.
        let r = repo_with_blob(b"x");
        let fifo = r.0.join("pipe");
        let c_path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that lives through the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let cancel = std::sync::Arc::new(Cancel::default());
        let c2 = cancel.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            c2.cancel();
        });
        let started = std::time::Instant::now();
        let path = fifo.to_string_lossy().into_owned();
        let err = run_capped(&r.0, &["hash-object", &path], 1024, Some(&cancel))
            .err()
            .expect("cancelled while running");
        t.join().unwrap();
        assert_eq!(err.to_string(), CANCELLED);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
