// Thin wrappers over the git binary. Every repository operation shells out so that git configuration,
// .gitignore semantics, and object storage are inherited unchanged. All commands are scoped to an explicit
// repository directory via `git -C <dir>` rather than relying on the process working directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Res;

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

// True when git holds the index lock, indicating another git process is mid-write. The watcher retries on a
// later cycle rather than contending for the lock.
pub fn index_locked(git_dir: &Path) -> bool {
    git_dir.join("index.lock").exists()
}
