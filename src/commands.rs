// Command implementations. Each entry point resolves the target repository from the working directory, then
// operates through the git wrappers and process-control helpers. Session state is two artefacts: the ref
// refs/gitomic/base marking where the session began, and a pidfile under <git-dir>/gitomic identifying the
// live watcher. Their presence or absence fully describes the session, so recovery after an unclean exit is a
// matter of inspecting them rather than reconstructing hidden state.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::proc::{self, Fork};
use crate::{git, watch, Res};

const BASE_REF: &str = "refs/gitomic/base";

// Directory holding gitomic's per-repository state (pidfile, log, finalize template).
fn state_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("gitomic")
}

// The pid of a live watcher for this repository, or None when no watcher is running. A pidfile whose process
// has died is treated as absent, so a crash leaves no lingering "running" illusion.
fn live_watcher(git_dir: &Path) -> Option<i32> {
    let pid = proc::read_pid(&state_dir(git_dir))?;
    if proc::alive(pid) {
        Some(pid)
    } else {
        None
    }
}

// Begin or resume a session: plant the base marker at HEAD if absent, then detach a background watcher. A
// second invocation while a watcher already runs is a no-op with a notice. When the base already exists but no
// watcher runs (for example after `stop`), the existing base is preserved and watching resumes from it.
pub fn init(cwd: &Path) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let sdir = state_dir(&git_dir);
    git::current_branch(&root)?; // rejects a detached HEAD before any state is written

    if let Some(pid) = live_watcher(&git_dir) {
        println!(
            "gitomic: watcher already running (pid {pid}) for {}",
            root.display()
        );
        return Ok(());
    }

    let resuming = git::rev_exists(&root, BASE_REF)?;
    if !resuming {
        let head = git::rev_parse(&root, "HEAD")?;
        git::update_ref(&root, BASE_REF, &head, "gitomic init")?;
    }

    let cfg = Config::load()?;
    fs::create_dir_all(&sdir)?;

    match proc::daemonize()? {
        Fork::Parent => {
            let started = wait_for_watcher(&git_dir, Duration::from_secs(3));
            let base = git::rev_parse(&root, BASE_REF)?;
            let verb = if resuming { "resumed" } else { "started" };
            println!("gitomic: session {verb} for {}", root.display());
            println!("  base:  {}", short(&base));
            if started {
                if let Some(pid) = live_watcher(&git_dir) {
                    println!("  watcher pid: {pid}");
                }
            } else {
                println!(
                    "  warning: watcher did not report readiness; check {}",
                    proc::logfile(&sdir).display()
                );
            }
            println!("  log:   {}", proc::logfile(&sdir).display());
            Ok(())
        }
        Fork::Child => {
            // Detached grandchild. Redirect diagnostics, announce liveness via the pidfile, then watch until a
            // shutdown signal. Cleanup removes the pidfile so the session reads as watcher-stopped afterwards.
            proc::redirect_stdio(&proc::logfile(&sdir))?;
            proc::install_signal_handlers();
            if proc::write_pid(&sdir).is_ok() {
                let _ = watch::run(&root, &git_dir, &cfg);
            }
            proc::clear_pid(&sdir);
            std::process::exit(0);
        }
    }
}

// Report session state without modifying it.
pub fn status(cwd: &Path) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let sdir = state_dir(&git_dir);

    if !git::rev_exists(&root, BASE_REF)? {
        println!("gitomic: no active session in {}", root.display());
        return Ok(());
    }

    let base = git::rev_parse(&root, BASE_REF)?;
    let pending = git::count(&root, &format!("{BASE_REF}..HEAD"))?;
    println!("gitomic: session active in {}", root.display());
    println!("  base:              {}", short(&base));
    println!("  pending commits:   {pending}");
    match live_watcher(&git_dir) {
        Some(pid) => println!("  watcher:           running (pid {pid})"),
        None => println!("  watcher:           stopped"),
    }
    println!("  log:               {}", proc::logfile(&sdir).display());
    Ok(())
}

// Stop the watcher while preserving the base marker and the recorded atomic commits, so the session can be
// finalized or resumed later.
pub fn stop(cwd: &Path) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;

    let stopped = terminate_watcher(&git_dir, Duration::from_secs(5))?;
    if !stopped {
        println!("gitomic: no watcher running in {}", root.display());
    } else {
        println!("gitomic: watcher stopped in {}", root.display());
    }
    if git::rev_exists(&root, BASE_REF)? {
        let pending = git::count(&root, &format!("{BASE_REF}..HEAD"))?;
        println!("  {pending} atomic commit(s) preserved; run 'gitomic finish' to finalize or 'gitomic init' to resume");
    }
    Ok(())
}

// Discard the session: stop the watcher, reset the branch back to the base marker (dropping all atomic
// commits and their working-tree state), and remove the marker. Destructive, so it requires an explicit
// --force to proceed; without it the effect is described but not performed.
pub fn abort(cwd: &Path, force: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;

    if !git::rev_exists(&root, BASE_REF)? {
        println!("gitomic: no active session in {}", root.display());
        return Ok(());
    }
    let base = git::rev_parse(&root, BASE_REF)?;
    let pending = git::count(&root, &format!("{BASE_REF}..HEAD"))?;

    if !force {
        println!(
            "gitomic: abort would reset {} to {} and discard {pending} atomic commit(s).",
            root.display(),
            short(&base)
        );
        println!(
            "  working-tree changes since the base would be lost. Re-run with --force to proceed."
        );
        return Ok(());
    }

    terminate_watcher(&git_dir, Duration::from_secs(5))?;
    git::run(&root, &["reset", "--hard", &base])?;
    git::delete_ref(&root, BASE_REF)?;
    proc::clear_pid(&state_dir(&git_dir));
    println!(
        "gitomic: session aborted; {} reset to {}",
        root.display(),
        short(&base)
    );
    Ok(())
}

// End the session: stop the watcher, then stamp one message across every atomic commit recorded since the
// base. `message` short-circuits the editor; otherwise the configured editor is launched with a template.
pub fn finish(cwd: &Path, message: Option<String>, numbering_override: Option<bool>) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let sdir = state_dir(&git_dir);
    let cfg = Config::load()?;

    if !git::rev_exists(&root, BASE_REF)? {
        println!("gitomic: no active session in {}", root.display());
        return Ok(());
    }

    terminate_watcher(&git_dir, Duration::from_secs(10))?;

    let branch = git::current_branch(&root)?;
    let base = git::rev_parse(&root, BASE_REF)?;
    let head = git::rev_parse(&root, "HEAD")?;
    let commits = git::rev_list_reverse(&root, &format!("{BASE_REF}..HEAD"))?;

    if commits.is_empty() {
        git::delete_ref(&root, BASE_REF)?;
        proc::clear_pid(&sdir);
        println!("gitomic: no atomic commits recorded this session; session cleared");
        return Ok(());
    }

    let message = match message {
        Some(m) => strip_message(&m),
        None => obtain_message(&root, &sdir, &commits)?,
    };
    if message.is_empty() {
        println!("gitomic: empty commit message; finalize aborted, session preserved");
        return Ok(());
    }

    let numbering = numbering_override.unwrap_or(cfg.finalize_numbering);
    let new_head = replay(&root, &base, &commits, &message, numbering)?;
    git::update_ref_cas(
        &root,
        &format!("refs/heads/{branch}"),
        &new_head,
        &head,
        "gitomic finalize",
    )?;
    git::delete_ref(&root, BASE_REF)?;
    proc::clear_pid(&sdir);

    println!(
        "gitomic: finalized {} atomic commit(s) on {branch}",
        commits.len()
    );
    println!("  {} -> {}", short(&base), short(&new_head));
    Ok(())
}

// Rebuild the batch onto the base, giving each commit the finalized message while preserving its original tree
// and authorship. Returns the object id of the new batch head.
fn replay(
    root: &Path,
    base: &str,
    commits: &[String],
    message: &str,
    numbering: bool,
) -> Res<String> {
    let n = commits.len();
    let mut parent = base.to_string();
    for (i, commit) in commits.iter().enumerate() {
        let tree = git::rev_parse(root, &format!("{commit}^{{tree}}"))?;
        let (an, ae, ad) = git::author_of(root, commit)?;
        let msg = if numbering {
            numbered(message, i + 1, n)
        } else {
            message.to_string()
        };
        parent = git::commit_tree(root, &tree, &parent, &msg, &an, &ae, &ad)?;
    }
    Ok(parent)
}

// Launch the editor on a template and return the cleaned message. The template lists the commits to be
// finalized as comment lines for orientation; comment lines and surrounding blank lines are removed on read.
fn obtain_message(root: &Path, sdir: &Path, commits: &[String]) -> Res<String> {
    fs::create_dir_all(sdir)?;
    let path = sdir.join("FINALIZE_MSG");
    fs::write(&path, template(commits))?;

    let editor = git::editor(root)?;
    // git launches its editor through the shell so that an editor value carrying arguments is honoured; the
    // same convention is reproduced, with the template path passed as the positional parameter.
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("gitomic")
        .arg(&path)
        .status()?;
    if !status.success() {
        return Err("editor exited non-zero; finalize aborted".into());
    }
    Ok(strip_message(&fs::read_to_string(&path)?))
}

// Build the editor template: a leading blank line for the message, followed by guidance and the commit list as
// comment lines.
fn template(commits: &[String]) -> String {
    let mut t = String::from("\n");
    t.push_str(
        "# Enter one commit message to apply to every atomic commit recorded this session.\n",
    );
    t.push_str("# Lines starting with '#' are ignored. An empty message aborts finalize and preserves the session.\n");
    t.push_str("#\n");
    t.push_str(&format!(
        "# {} commit(s) to finalize (oldest first):\n",
        commits.len()
    ));
    for c in commits {
        t.push_str(&format!("#   {}\n", short(c)));
    }
    t
}

// Stop a running watcher and wait for it to exit within `deadline`. Returns whether a watcher was found and
// stopped. A stale pidfile (process already gone) is cleared and reported as "not running".
fn terminate_watcher(git_dir: &Path, deadline: Duration) -> Res<bool> {
    let sdir = state_dir(git_dir);
    let pid = match proc::read_pid(&sdir) {
        Some(p) => p,
        None => return Ok(false),
    };
    if !proc::alive(pid) {
        proc::clear_pid(&sdir);
        return Ok(false);
    }

    proc::request_stop(pid);
    let start = Instant::now();
    while start.elapsed() < deadline {
        if !proc::alive(pid) {
            proc::clear_pid(&sdir);
            return Ok(true);
        }
        sleep(Duration::from_millis(50));
    }
    // The watcher flushes on SIGTERM; failure to exit within the deadline indicates a wedged process and is
    // surfaced rather than left ambiguous.
    Err(format!(
        "watcher (pid {pid}) did not exit within {} ms",
        deadline.as_millis()
    )
    .into())
}

// Poll for a freshly forked watcher to publish its pidfile and become live, up to `deadline`.
fn wait_for_watcher(git_dir: &Path, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if live_watcher(git_dir).is_some() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

// Abbreviate an object id for display, matching git's conventional short length.
fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

// Remove comment lines and trim surrounding whitespace, mirroring git's `--cleanup=strip`: lines whose first
// non-whitespace character is '#' are dropped, then leading and trailing blank lines are removed.
fn strip_message(raw: &str) -> String {
    let kept: Vec<&str> = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    kept.join("\n").trim().to_string()
}

// Append a " [i/N]" ordinal to a message so identical batch messages remain distinguishable in the log.
fn numbered(message: &str, index: usize, total: usize) -> String {
    format!("{message} [{index}/{total}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_comments_and_trims() {
        let raw = "\n# a comment\nreal message\n  # indented comment\nsecond line\n\n";
        assert_eq!(strip_message(raw), "real message\nsecond line");
    }

    #[test]
    fn strip_of_only_comments_is_empty() {
        assert_eq!(strip_message("# only\n#comments\n\n"), "");
    }

    #[test]
    fn strip_preserves_hash_inside_line() {
        assert_eq!(strip_message("fix issue #42\n"), "fix issue #42");
    }

    #[test]
    fn numbering_format() {
        assert_eq!(numbered("refactor auth", 3, 20), "refactor auth [3/20]");
    }

    #[test]
    fn short_truncates() {
        assert_eq!(short("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
    }
}
