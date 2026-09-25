// Thin wrappers over the git binary. Every repository operation shells out so that git configuration,
// .gitignore semantics, and object storage are inherited unchanged. All commands are scoped to an explicit
// repository directory via `git -C <dir>` rather than relying on the process working directory.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
// LEGACY_BASE_REF, which callers check separately. Backs `status`'s all-branches listing.
pub fn session_branches(dir: &Path) -> Res<Vec<String>> {
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

// True when an interrupted multi-step operation is in progress. Auto-committing during a merge, rebase,
// cherry-pick, revert, or bisect would corrupt the operation's expected state, so the watcher stands down.
pub fn operation_in_progress(git_dir: &Path) -> bool {
    const MARKERS: [&str; 5] = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
    ];
    MARKERS.iter().any(|m| git_dir.join(m).exists())
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
