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

const BASE_REF: &str = git::BASE_REF;

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
pub fn init(cwd: &Path, foreground: bool, verbose: bool) -> Res<()> {
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

    if foreground {
        // Foreground session: the watcher runs in the calling process with diagnostics on the terminal rather
        // than detaching and redirecting them to the log. This is a diagnostic mode: the operator sees the
        // exact moment the recursive watch is established (the "watching ..." line), every event as it arrives
        // under verbose tracing, and each commit cycle, while reproducing a scenario by hand. The pidfile is
        // still written so status/finish/stop/abort invoked from another terminal observe and can signal this
        // watcher; it is cleared on exit. Termination is by SIGINT (Ctrl-C) or a SIGTERM from `finish`/`stop`,
        // both of which the installed handler turns into a clean loop exit; the session base and recorded
        // atomic commits are preserved for a subsequent `finish` or `init` exactly as in the detached case.
        let base = git::rev_parse(&root, BASE_REF)?;
        let verb = if resuming { "resumed" } else { "started" };
        println!("gitomic: foreground session {verb} for {}", root.display());
        println!("  base:  {}", short(&base));
        println!("  diagnostics stream to this terminal (not the log file) until Ctrl-C.");
        if verbose {
            println!(
                "  verbose: every file-system event is traced before the git-internal filter."
            );
        }
        println!("  Ctrl-C stops the watcher; the session and its commits are preserved.");
        proc::install_signal_handlers();
        proc::write_pid(&sdir)?;
        let res = watch::run(&root, &git_dir, &cfg, verbose);
        proc::clear_pid(&sdir);
        res?;
        return Ok(());
    }

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
                let _ = watch::run(&root, &git_dir, &cfg, verbose);
            }
            proc::clear_pid(&sdir);
            std::process::exit(0);
        }
    }
}

// Run a wrapped command to completion in the work tree, then capture its full effect as one atomic commit
// staging untracked files as well (`git add -A`), irrespective of the configured stage mode. This is the
// escape hatch for `stage = tracked`: a patch or generator that creates new files, run under a tracked-only
// session, would otherwise leave those files unstaged and produce no commit. Under the default `stage =
// observed` a live watcher already captures such files directly (it observes the create), so exec is only
// needed there to bind a command's whole effect into a single commit with no watcher running. Wrapping the
// command declares its result as intended history, so the created files are recorded. Requires an active
// session so the commit joins a batch that `finish` finalizes. The wrapped command's non-zero exit aborts the
// capture: a patch that does not apply must not be followed by a commit of a partial tree.
pub fn exec(cwd: &Path, argv: &[String], shell: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;

    if argv.is_empty() {
        return Err("exec: no command given".into());
    }
    if !git::rev_exists(&root, BASE_REF)? {
        return Err("exec: no active session; run 'gitomic init' first so the captured change joins a batch".into());
    }

    let label = command_label(argv, shell);
    let status = if shell {
        // Shell mode mirrors `su -c`: the arguments are joined into one string interpreted by sh, so globs,
        // pipelines, and redirections are honoured.
        Command::new("sh")
            .arg("-c")
            .arg(argv.join(" "))
            .current_dir(&root)
            .status()?
    } else {
        // Direct mode executes argv without a shell, so no quoting or injection concerns arise for the common
        // `gitomic exec git apply file.patch` form.
        Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&root)
            .status()?
    };
    if !status.success() {
        let how = status
            .code()
            .map(|c| format!("exited {c}"))
            .unwrap_or_else(|| "terminated by signal".to_string());
        return Err(format!("exec: '{label}' {how}; no capture performed").into());
    }

    match watch::flush_all(&root, &git_dir) {
        watch::Flush::Committed(sha) if !sha.is_empty() => {
            println!("gitomic: captured '{label}' as {sha}")
        }
        watch::Flush::Committed(_) => println!("gitomic: captured '{label}'"),
        watch::Flush::Nothing => println!("gitomic: '{label}' produced no change to capture"),
        watch::Flush::Skipped(why) => println!("gitomic: capture skipped: {why}"),
        watch::Flush::Failed(msg) => return Err(format!("exec: capture failed: {msg}").into()),
    }
    Ok(())
}

// Render the wrapped command for diagnostics: the joined argv in both modes, which reads the same whether the
// operator quoted a shell string or passed bare arguments.
fn command_label(argv: &[String], _shell: bool) -> String {
    argv.join(" ")
}

// Whether the repository is free of a gitomic session, i.e. safe to run a build without the watcher sweeping
// build outputs into the session, or a later finish/resume capturing them. A session is defined by the base
// marker refs/gitomic/base and is independent of whether the watcher process is currently live: a session that
// was stopped but not finished still holds the tree in a recording state, so it is reported as not build-safe.
// Returns true when no base marker exists. Intended as a scriptable gate; the caller maps the boolean to a
// process exit code.
pub fn build_safe(cwd: &Path) -> Res<bool> {
    let root = git::work_tree(cwd)?;
    Ok(!git::rev_exists(&root, BASE_REF)?)
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
    let cfg = Config::load()?;

    let stopped = terminate_watcher(&git_dir)?;
    if !stopped {
        println!("gitomic: no watcher running in {}", root.display());
    } else {
        println!("gitomic: watcher stopped in {}", root.display());
    }
    // Foreground capture of any change observed after the watcher's last debounce cycle, run only once the
    // watcher is confirmed gone so the two never contend for the index. Skipped when no session is active,
    // since there is no base against which the recorded commit would be finalized.
    if git::rev_exists(&root, BASE_REF)? {
        report_flush(&root, &git_dir, &cfg);
        let pending = git::count(&root, &format!("{BASE_REF}..HEAD"))?;
        println!("  {pending} atomic commit(s) preserved; run 'gitomic finish' to finalize or 'gitomic init' to resume");
    }
    Ok(())
}

// Discard the session: stop the watcher, move the branch back to the base marker (dropping all atomic
// commits), and remove the marker. The working tree is preserved: `reset --mixed` rewinds the branch and
// index to the base without touching files on disk, so content the watcher swept into atomic commits from a
// previously untracked state survives the abort as untracked files rather than being deleted. A prior
// `reset --hard` here removed such files, because files present only in the discarded commits (absent at the
// base) are deleted by a hard reset; that data-loss path is the reason a mixed reset is used. Still requires
// an explicit --force, since dropping recorded commits is not reversible through gitomic itself; without it
// the effect is described but not performed.
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
            "gitomic: abort would move {} back to {} and discard {pending} atomic commit(s).",
            root.display(),
            short(&base)
        );
        println!(
            "  working-tree files are preserved; content from discarded commits reverts to unstaged/untracked. \
             Re-run with --force to proceed."
        );
        return Ok(());
    }

    terminate_watcher(&git_dir)?;
    // A mixed reset moves the branch and index to the base while leaving every working-tree file in place, so
    // no file on disk is deleted by the abort. A hard reset would remove files that exist only in the
    // discarded commits. No final capture is performed: the session is being discarded, so committing the
    // last changes only to reset past them would be pointless.
    git::run(&root, &["reset", "--mixed", &base])?;
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

    terminate_watcher(&git_dir)?;
    // Foreground capture before the batch is enumerated, so a change made after the watcher's last cycle is
    // included in the finalized batch rather than lost. Visible to the operator, unlike the former in-watcher
    // shutdown flush.
    report_flush(&root, &git_dir, &cfg);

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

// Stop a running watcher and wait for it to exit. Returns whether a watcher was found and stopped. A stale
// pidfile (process already gone) is cleared and reported as "not running". There is no shutdown deadline: the
// watcher exits within one poll interval unless it is mid-commit on a large tree, in which case it exits once
// that single git operation completes. The function waits for that to happen and reports periodically so the
// wait is never silent, rather than giving up after an arbitrary interval and returning control while the
// watcher is still live. Confirming the watcher has exited before returning is what lets a caller safely run a
// subsequent reset without racing a live watcher over the index. A genuinely wedged watcher is interrupted by
// the operator (Ctrl-C) rather than by a timer.
fn terminate_watcher(git_dir: &Path) -> Res<bool> {
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
    println!("gitomic: stopping watcher (pid {pid})");
    let start = Instant::now();
    let mut next_notice = Duration::from_secs(2);
    while proc::alive(pid) {
        if start.elapsed() >= next_notice {
            // A watcher caught mid-commit on a large tree completes that git operation before observing the
            // signal; report the ongoing wait rather than leaving the operator at a silent prompt.
            println!(
                "  still waiting for the watcher to finish an in-flight commit ({}s elapsed)",
                start.elapsed().as_secs()
            );
            next_notice += Duration::from_secs(2);
        }
        sleep(Duration::from_millis(100));
    }
    proc::clear_pid(&sdir);
    println!(
        "  watcher exited after {:.1}s",
        start.elapsed().as_secs_f64()
    );
    Ok(true)
}

// Perform one foreground capture and report its outcome to the operator's terminal. Used by `stop` and
// `finish` after the watcher is confirmed gone, so the final capture and its result are visible rather than
// buried in the watcher's redirected log. Under observed staging the paths the just-stopped watcher saw but
// had not yet committed are recovered from the state directory and used for this capture, so a change made
// after the watcher's last debounce cycle — including a rename, whose new half tracked-only staging would drop
// — is still captured whole. For tracked/all modes the set is consumed and ignored.
fn report_flush(root: &Path, git_dir: &Path, cfg: &Config) {
    let observed = watch::take_pending_observed(&state_dir(git_dir));
    match watch::flush_once(root, git_dir, cfg, &observed) {
        watch::Flush::Committed(sha) if !sha.is_empty() => {
            println!("  captured final change as {sha}")
        }
        watch::Flush::Committed(_) => println!("  captured final change"),
        watch::Flush::Nothing => println!("  no pending changes to capture"),
        watch::Flush::Skipped(why) => println!("  final capture skipped: {why}"),
        watch::Flush::Failed(msg) => println!("  final capture failed: {msg}"),
    }
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
