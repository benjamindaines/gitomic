// Command implementations. Each entry point resolves the target repository from the working directory, then
// operates through the git wrappers and process-control helpers.
//
// A session records onto a private branch rather than onto the branch the operator began on (issue #10). `init`
// forks a session branch `<origin>-<short base>` from HEAD, checks it out, and the watcher's atomic commits land
// there; `finish` integrates that batch back onto the origin branch and removes the session branch. Recording on
// a separate ref means anything that moves the origin branch out-of-band during the session — a `git pull`, a
// push from another client, a direct-to-remote commit later fetched — cannot be misattributed as session work,
// because the origin branch and the session branch never share a ref. `finish` finalizes onto the origin branch's
// current tip: unchanged since the session began, the batch replays straight on (identical to a plain commit);
// advanced by a fast-forward, the batch is replayed on top of the new tip; diverged such that the batch does not
// apply, the finalized work is left on the session branch for a manual merge or pull request while the origin
// branch is left exactly as the out-of-band move left it — data is preserved either way rather than one line of
// history silently rewriting the other.
//
// Session state is three artefacts per session branch: the ref refs/gitomic/base/<session-branch> marking the
// fork point, a pidfile under <git-dir>/gitomic/<session-branch> identifying that session's live watcher (issue
// #4), and an ORIGIN file beside the pidfile naming the branch the session was forked from. Their presence or
// absence fully describes the session, so recovery after an unclean exit is a matter of inspecting them rather
// than reconstructing hidden state. A session with a base ref but no ORIGIN file is a legacy in-place session
// (created before issue #10); the commands below still finalize such a session on the branch itself. `status`
// reports every open session and, for each, the origin branch the operator sees rather than the private branch,
// noting whether that origin branch has moved since the session began.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::git::state_dir;
use crate::proc::{self, Fork};
use crate::{git, watch, Res};

// The pid of a live watcher for `branch`, or None when no watcher is running for it. A pidfile whose process
// has died is treated as absent, so a crash leaves no lingering "running" illusion.
pub(crate) fn live_watcher(git_dir: &Path, branch: &str) -> Option<i32> {
    let pid = proc::read_pid(&state_dir(git_dir, branch))?;
    if proc::alive(pid) {
        Some(pid)
    } else {
        None
    }
}

// Name of the private branch a session records on: the origin branch plus the short base commit, so the branch
// the operator believes they are on is never the branch recording actually happens on (issue #10). The base sha
// is the fork point — the commit at which the session diverges from the origin branch — so the name identifies
// exactly where an eventual manual reconciliation would begin.
fn session_branch_name(origin: &str, base: &str) -> String {
    format!("{origin}-{}", short(base))
}

// File under a session branch's state directory recording the origin branch it was forked from, so finish, abort,
// and status can integrate back onto and report against the branch the operator started on rather than the
// private session branch. Its absence marks a legacy in-place session (one created before issue #10), which the
// same commands still handle on the branch itself.
const ORIGIN_MARKER: &str = "ORIGIN";

fn write_session_origin(git_dir: &Path, session_branch: &str, origin: &str) -> Res<()> {
    let sdir = state_dir(git_dir, session_branch);
    fs::create_dir_all(&sdir)?;
    fs::write(sdir.join(ORIGIN_MARKER), format!("{origin}\n"))?;
    Ok(())
}

fn read_session_origin(git_dir: &Path, session_branch: &str) -> Option<String> {
    let s = fs::read_to_string(state_dir(git_dir, session_branch).join(ORIGIN_MARKER)).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn clear_session_origin(git_dir: &Path, session_branch: &str) {
    let _ = fs::remove_file(state_dir(git_dir, session_branch).join(ORIGIN_MARKER));
}

// The session branch, if any, whose recorded origin is `origin`. Lets init/finish/abort/build_safe notice a
// session that is open on its private branch while the operator has the origin branch itself checked out, rather
// than starting a second session or reporting none. A legacy in-place session carries no ORIGIN file, so it is
// never matched here and is found only when its own branch is checked out.
fn find_session_for_origin(root: &Path, git_dir: &Path, origin: &str) -> Res<Option<String>> {
    for sb in git::session_branches(root)? {
        if read_session_origin(git_dir, &sb).as_deref() == Some(origin) {
            return Ok(Some(sb));
        }
    }
    Ok(None)
}

// Begin or resume a session. A fresh session forks a private branch `<origin>-<short base>` from HEAD, checks it
// out, plants the base marker, and records the origin branch, then detaches a background watcher bound to the
// session branch. Re-invoked while already on a session branch, it resumes that session (a no-op with a notice if
// its watcher is already live; a restart if the watcher had been stopped). Re-invoked on an origin branch whose
// session is open on its private branch, it directs the operator to that branch rather than opening a second
// session.
pub fn init(cwd: &Path, foreground: bool, verbose: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let current = git::current_branch(&root)?; // rejects a detached HEAD before any state is written
    reject_legacy_session(&root)?;

    // Resolve which branch this session records on. Already on a session branch (its base marker exists): resume
    // it. On an origin branch with a session open elsewhere: point there. Otherwise: fork a new session branch.
    let (branch, resuming) = if git::rev_exists(&root, &git::base_ref(&current))? {
        (current.clone(), true)
    } else if let Some(existing) = find_session_for_origin(&root, &git_dir, &current)? {
        println!(
            "gitomic: a session for '{current}' is already open on '{existing}'.\n  \
             Resume it with 'git checkout {existing}' then 'gitomic init', or finish/abort it first."
        );
        return Ok(());
    } else {
        let base = git::rev_parse(&root, "HEAD")?;
        let sb = session_branch_name(&current, &base);
        if git::branch_exists(&root, &sb)? {
            return Err(format!(
                "cannot start session: branch '{sb}' already exists; delete or rename it, then retry"
            )
            .into());
        }
        git::create_and_checkout(&root, &sb, &base)?;
        git::update_ref(&root, &git::base_ref(&sb), &base, "gitomic init")?;
        write_session_origin(&git_dir, &sb, &current)?;
        (sb, false)
    };

    let sdir = state_dir(&git_dir, &branch);
    let base_ref = git::base_ref(&branch);
    // The name presented to the operator: the origin branch for a session-branch session, the branch itself for a
    // legacy in-place one. Also the name recorded in the cross-repo registry so `active` reports the origin.
    let shown = display_origin(&git_dir, &branch);

    if let Some(pid) = live_watcher(&git_dir, &branch) {
        println!(
            "gitomic: watcher already running (pid {pid}) for {} [{shown}]",
            root.display()
        );
        return Ok(());
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
        let base = git::rev_parse(&root, &base_ref)?;
        let verb = if resuming { "resumed" } else { "started" };
        println!(
            "gitomic: foreground session {verb} for {} [{shown}]",
            root.display()
        );
        println!("  base:  {}", short(&base));
        if shown != branch {
            println!("  recording on private branch {branch}");
        }
        println!("  diagnostics stream to this terminal (not the log file) until Ctrl-C.");
        if verbose {
            println!(
                "  verbose: every file-system event is traced before the git-internal filter."
            );
        }
        println!("  Ctrl-C stops the watcher; the session and its commits are preserved.");
        proc::install_signal_handlers();
        proc::write_pid(&sdir)?;
        proc::announce_active(&root, &shown);
        let res = watch::run(&root, &git_dir, &branch, &cfg, verbose);
        proc::retire_active(&root, &shown);
        proc::clear_pid(&sdir);
        res?;
        return Ok(());
    }

    match proc::daemonize()? {
        Fork::Parent => {
            let started = wait_for_watcher(&git_dir, &branch, Duration::from_secs(3));
            let base = git::rev_parse(&root, &base_ref)?;
            let verb = if resuming { "resumed" } else { "started" };
            println!("gitomic: session {verb} for {} [{shown}]", root.display());
            println!("  base:  {}", short(&base));
            if shown != branch {
                println!("  recording on private branch {branch}");
            }
            if started {
                if let Some(pid) = live_watcher(&git_dir, &branch) {
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
                proc::announce_active(&root, &shown);
                let _ = watch::run(&root, &git_dir, &branch, &cfg, verbose);
                proc::retire_active(&root, &shown);
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
    let branch = git::current_branch(&root)?;

    if argv.is_empty() {
        return Err("exec: no command given".into());
    }
    if !git::rev_exists(&root, &git::base_ref(&branch))? {
        return Err(format!(
            "exec: no active session on '{branch}'; run 'gitomic init' first so the captured change joins a batch"
        )
        .into());
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

// Whether the checked-out branch is free of a gitomic session, i.e. safe to run a build without that branch's
// watcher sweeping build outputs into the session, or a later finish/resume capturing them. A session is
// defined by that branch's base marker and is independent of whether the watcher process is currently live: a
// session that was stopped but not finished still holds the tree in a recording state, so it is reported as
// not build-safe. Scoped to the checked-out branch rather than the whole repository: another branch's open
// session cannot touch the current work tree, since its watcher stands down whenever that branch is not the
// one checked out (see watch::run). Detached HEAD is reported safe unconditionally, since `init` refuses to
// start a session without a named branch, so none can exist there. Intended as a scriptable gate; the caller
// maps the boolean to a process exit code.
pub fn build_safe(cwd: &Path) -> Res<bool> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    match git::current_branch(&root) {
        Ok(branch) => {
            // Not safe if the checked-out branch is itself a session branch, nor if a session for it is open on a
            // private branch — the operator is on the origin branch but its watcher is still bound to the session
            // branch and will resume the moment it is checked back out, sweeping in whatever a build wrote to the
            // shared work tree meanwhile. Both cases share one work tree, so both make a build unsafe.
            if git::rev_exists(&root, &git::base_ref(&branch))? {
                return Ok(false);
            }
            Ok(find_session_for_origin(&root, &git_dir, &branch)?.is_none())
        }
        Err(_) => Ok(true),
    }
}

// The name to present to the operator for a session branch: its origin when recorded, else the branch itself.
fn display_origin(git_dir: &Path, session_branch: &str) -> String {
    read_session_origin(git_dir, session_branch).unwrap_or_else(|| session_branch.to_string())
}

// Report every branch holding an open session, without modifying anything. Unlike the other commands, status
// is not scoped to the checked-out branch: with one watcher per branch (issue #4), a session can sit open and
// untouched on a branch that is not currently checked out, and that is exactly the case an operator most needs
// surfaced rather than hidden behind whichever branch they happen to be on right now.
pub fn status(cwd: &Path) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let current = git::current_branch(&root).ok(); // None on detached HEAD; other branches' sessions still list
    let branches = git::session_branches(&root)?;
    let legacy = git::rev_exists(&root, git::LEGACY_BASE_REF)?;

    if branches.is_empty() {
        println!("gitomic: no active session in {}", root.display());
    } else {
        let plural = if branches.len() == 1 { "" } else { "s" };
        println!(
            "gitomic: {} session{plural} open in {}",
            branches.len(),
            root.display()
        );
        for branch in &branches {
            let base_ref = git::base_ref(branch);
            let base = git::rev_parse(&root, &base_ref)?;
            let origin = read_session_origin(&git_dir, branch); // None => legacy in-place session
            let shown = origin.clone().unwrap_or_else(|| branch.clone());
            // The session branch's own tip, not HEAD: a session's commits land there only while it is checked out,
            // but this report must be meaningful for a session that currently is not.
            let pending = git::count(&root, &format!("{base_ref}..refs/heads/{branch}"))?;
            // Marked current when the session branch itself is checked out, or when the operator is on the origin
            // branch this session records for.
            let is_current = current.as_deref() == Some(branch.as_str())
                || (origin.is_some() && current.as_deref() == origin.as_deref());
            let marker = if is_current { " (current)" } else { "" };
            println!("  {shown}{marker}");
            println!("    base:            {}", short(&base));
            if let Some(ref origin_branch) = origin {
                println!("    session branch:  {branch} (local)");
                // Whether the origin branch has moved out-of-band since the session began — the desync this scoping
                // is meant to surface rather than silently fold into the pending count.
                if let Ok(origin_tip) = git::rev_parse(&root, &format!("refs/heads/{origin_branch}")) {
                    if origin_tip != base {
                        let how = if git::is_ancestor(&root, &base, &origin_tip).unwrap_or(false) {
                            "advanced; finish replays the batch on top, or keeps it on the session branch if it conflicts"
                        } else {
                            "diverged; finish keeps the batch on the session branch for a manual merge"
                        };
                        println!(
                            "    origin moved:    '{origin_branch}' now at {} — {how}",
                            short(&origin_tip)
                        );
                    }
                }
            }
            println!("    pending commits: {pending}");
            match live_watcher(&git_dir, branch) {
                Some(pid) => println!("    watcher:         running (pid {pid})"),
                None => println!("    watcher:         stopped"),
            }
            println!(
                "    log:             {}",
                proc::logfile(&state_dir(&git_dir, branch)).display()
            );
        }
    }

    if legacy {
        let base = git::rev_parse(&root, git::LEGACY_BASE_REF)?;
        println!(
            "  legacy session marker present (pre-#4 fix, base {}):",
            short(&base)
        );
        println!(
            "    not used by this version; inspect with 'git log {}..HEAD' against whichever",
            git::LEGACY_BASE_REF
        );
        println!(
            "    branch it was recorded against, then remove with 'git update-ref -d {}'",
            git::LEGACY_BASE_REF
        );
        println!("    once any pending work is recovered.");
    }
    Ok(())
}

// Report every live gitomic watcher on the machine, across every repository, without needing to be run from
// inside any one of them (see main.rs's dispatch: this is the one command that does not resolve a repository
// from the working directory first). Backs a shell-profile hook run on new-terminal open: prints nothing when
// nothing is running, so it stays quiet on the common case instead of announcing "all clear" every time.
pub fn active() -> Res<()> {
    for s in proc::active_sessions() {
        println!("{}  [{}]  pid {}", s.repo, s.branch, s.pid);
    }
    Ok(())
}

// Show the consolidated diff of the checked-out branch's pending batch (base..HEAD) — the accumulated effect
// of every atomic commit recorded this session so far, not the working tree (plain `git diff` already shows
// that). Runs with inherited stdio so the operator's pager and color configuration apply exactly as they
// would for `git diff` invoked directly, rather than gitomic capturing and re-printing the output itself.
pub fn diff(cwd: &Path, stat: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let branch = git::current_branch(&root)?;
    let base_ref = git::base_ref(&branch);

    if !git::rev_exists(&root, &base_ref)? {
        return Err(format!(
            "diff: no active session for '{branch}' in {}",
            root.display()
        )
        .into());
    }

    let range = format!("{base_ref}..HEAD");
    let mut args: Vec<&str> = vec!["diff"];
    if stat {
        args.push("--stat");
    }
    args.push(&range);
    let status = git::spawn_inherit(&root, &args)?;
    if !status.success() {
        return Err("diff: git diff exited non-zero".into());
    }
    Ok(())
}

// Stop the watcher while preserving the base marker and the recorded atomic commits, so the session can be
// finalized or resumed later.
pub fn stop(cwd: &Path) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let branch = git::current_branch(&root)?;
    let base_ref = git::base_ref(&branch);
    let cfg = Config::load()?;

    let stopped = terminate_watcher(&git_dir, &branch)?;
    if !stopped {
        println!(
            "gitomic: no watcher running for '{branch}' in {}",
            root.display()
        );
    } else {
        println!(
            "gitomic: watcher stopped for '{branch}' in {}",
            root.display()
        );
    }
    // Foreground capture of any change observed after the watcher's last debounce cycle, run only once the
    // watcher is confirmed gone so the two never contend for the index. Skipped when no session is active,
    // since there is no base against which the recorded commit would be finalized.
    if git::rev_exists(&root, &base_ref)? {
        report_flush(&root, &git_dir, &branch, &cfg);
        let pending = git::count(&root, &format!("{base_ref}..HEAD"))?;
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
    let branch = git::current_branch(&root)?;
    let base_ref = git::base_ref(&branch);

    if !git::rev_exists(&root, &base_ref)? {
        if let Some(sb) = find_session_for_origin(&root, &git_dir, &branch)? {
            println!("gitomic: the session for '{branch}' is on '{sb}'; run 'git checkout {sb}' then 'gitomic abort'.");
        } else {
            println!(
                "gitomic: no active session for '{branch}' in {}",
                root.display()
            );
        }
        return Ok(());
    }
    let origin = read_session_origin(&git_dir, &branch); // None => legacy in-place session
    let base = git::rev_parse(&root, &base_ref)?;
    let pending = git::count(&root, &format!("{base_ref}..HEAD"))?;

    if !force {
        match &origin {
            Some(b) => println!(
                "gitomic: abort would discard {pending} atomic commit(s) on session branch '{branch}', \
                 return to '{b}', and remove '{branch}'."
            ),
            None => println!(
                "gitomic: abort would move {} back to {} and discard {pending} atomic commit(s) on '{branch}'.",
                root.display(),
                short(&base)
            ),
        }
        println!(
            "  working-tree files are preserved; content from discarded commits reverts to unstaged/untracked. \
             Re-run with --force to proceed."
        );
        return Ok(());
    }

    terminate_watcher(&git_dir, &branch)?;
    let sdir = state_dir(&git_dir, &branch);
    // A mixed reset moves the branch and index to the base while leaving every working-tree file in place, so no
    // file on disk is deleted by the abort. A hard reset would remove files that exist only in the discarded
    // commits. No final capture is performed: the session is being discarded, so committing the last changes only
    // to reset past them would be pointless.
    git::run(&root, &["reset", "--mixed", &base])?;
    match origin {
        Some(b) => {
            // Return to the origin branch and drop the private session branch. The reset above left the discarded
            // content on disk as unstaged/untracked, and the switch carries it across; a switch that would be
            // blocked by that content is reported rather than forced, leaving the branch removable by hand.
            if let Err(e) = git::checkout(&root, &b) {
                git::delete_ref(&root, &base_ref)?;
                proc::clear_pid(&sdir);
                clear_session_origin(&git_dir, &branch);
                println!(
                    "gitomic: session commits discarded on '{branch}', but switching to '{b}' failed ({e})."
                );
                println!("  resolve the working tree, then 'git checkout {b}' and 'git branch -D {branch}'.");
                return Ok(());
            }
            git::delete_branch(&root, &branch)?;
            git::delete_ref(&root, &base_ref)?;
            proc::clear_pid(&sdir);
            clear_session_origin(&git_dir, &branch);
            println!("gitomic: session aborted; back on '{b}', session branch '{branch}' removed");
        }
        None => {
            git::delete_ref(&root, &base_ref)?;
            proc::clear_pid(&sdir);
            println!(
                "gitomic: session aborted; {} reset to {} on '{branch}'",
                root.display(),
                short(&base)
            );
        }
    }
    Ok(())
}

// Remove individual unpublished commits from the checked-out branch (issue #12). Each selector is a
// commit hash, abbreviated or full, as printed by the finish template or `git log`. The candidates
// are the commits that no remote-tracking ref contains or, while a session is open, the session's
// pending batch (base..HEAD). A published commit is refused: rewriting it would diverge from what a
// remote already holds.
//
// The rewrite is computed entirely in the object database. The commits following the first dropped
// one are re-applied, oldest first, onto the surviving parent with their messages and authorship
// intact. A later commit that does not apply cleanly without a dropped change (for example, one
// that edits a file the dropped commit created) aborts the whole operation before anything is
// modified; the error names that commit so it can be dropped as well. Only when every replay
// succeeds is the branch moved, with `reset --keep`: files the dropped commits introduced or
// altered are removed or restored on disk, uncommitted edits elsewhere are retained, and an
// uncommitted edit to a file that must be rewritten aborts the move with nothing changed. The
// dropped commit objects stay in the object database until garbage collection, and their full ids
// are printed so a mistaken drop is recoverable with `git cherry-pick`.
//
// An open session's watcher is stopped and its final capture flushed first, so the tip being
// rewritten is stable, then restarted afterwards, so the session continues. `dry_run` reports the
// outcome, including any conflict, without modifying the branch or the watcher.
pub fn drop_commits(cwd: &Path, selectors: &[String], dry_run: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let branch = git::current_branch(&root)?;

    if selectors.is_empty() {
        return Err("drop: expected at least one commit hash".into());
    }
    if git::operation_in_progress(&git_dir) {
        return Err("drop: a merge, rebase, cherry-pick, revert, or bisect is in progress".into());
    }

    // Rejecting a bad selector before the watcher is disturbed keeps a typo from interrupting a
    // session.
    resolve_drop_targets(&root, &branch, selectors)?;

    let was_live = !dry_run && live_watcher(&git_dir, &branch).is_some();
    if was_live {
        let cfg = Config::load()?;
        terminate_watcher(&git_dir, &branch)?;
        report_flush(&root, &git_dir, &branch, &cfg);
    }

    let result = drop_locked(&root, &branch, selectors, dry_run);

    if was_live {
        if let Err(e) = init(cwd, false, false) {
            eprintln!("gitomic: drop: watcher could not be restarted: {e}");
        }
    }
    result
}

// Candidate commits for a drop, oldest-first with their parents, and the wording that describes the
// candidate set in diagnostics. An open session confines candidates to its pending batch, since the
// base marker must stay an ancestor of the branch tip; otherwise every commit not yet contained in
// a remote-tracking ref qualifies.
type Chain = Vec<(String, Vec<String>)>;
type Candidates = (Chain, &'static str);

fn drop_candidates(root: &Path, branch: &str) -> Res<Candidates> {
    let base_ref = git::base_ref(branch);
    if git::rev_exists(root, &base_ref)? {
        let range = format!("{base_ref}..HEAD");
        Ok((
            git::commits_with_parents(root, Some(&range))?,
            "in this session's pending batch",
        ))
    } else {
        Ok((
            git::commits_with_parents(root, None)?,
            "unpublished (not contained in any remote-tracking branch)",
        ))
    }
}

// Resolve each selector to a full commit id and confirm it is a candidate. Returns the candidate
// list and the distinct selected ids in history order, so the caller reports and replays them
// consistently regardless of the order or repetition on the command line.
fn resolve_drop_targets(
    root: &Path,
    branch: &str,
    selectors: &[String],
) -> Res<(Chain, Vec<String>)> {
    let (candidates, scope) = drop_candidates(root, branch)?;
    let mut chosen: Vec<String> = Vec::new();
    for sel in selectors {
        let full = git::rev_parse(root, &format!("{sel}^{{commit}}"))
            .map_err(|_| format!("drop: '{sel}' does not name a single commit"))?;
        if !candidates.iter().any(|(c, _)| *c == full) {
            return Err(format!(
                "drop: {} is not {scope} on '{branch}'; only such commits may be dropped",
                short(&full)
            )
            .into());
        }
        if !chosen.contains(&full) {
            chosen.push(full);
        }
    }
    let ordered = candidates
        .iter()
        .map(|(c, _)| c.clone())
        .filter(|c| chosen.contains(c))
        .collect();
    Ok((candidates, ordered))
}

// The outcome of planning a drop: what the branch tip would become, computed without modifying the
// branch, the index, or the work tree. Only new commit objects are written, and an unreferenced
// object is inert until garbage collection.
pub struct DropPlan {
    old_head: String,
    new_head: String,
    targets: Vec<String>,
    replayed: usize,
}

// Resolve the selectors and replay every later commit onto the surviving parent, in the object
// database only. Fails, with nothing modified, on an ineligible selector, on a merge or root commit
// in the affected range, or on a later commit that does not apply without a dropped change.
fn plan_drop(root: &Path, branch: &str, selectors: &[String]) -> Res<DropPlan> {
    let (candidates, targets) = resolve_drop_targets(root, branch, selectors)?;
    let first = candidates
        .iter()
        .position(|(c, _)| targets.contains(c))
        .ok_or("drop: no matching commit")?;

    // Replay is defined for a single chain of single-parent commits. A merge or root commit at or
    // after the first dropped commit has no unique parent to re-apply onto.
    for (c, parents) in &candidates[first..] {
        if parents.len() != 1 {
            let kind = if parents.is_empty() { "root" } else { "merge" };
            return Err(format!(
                "drop: {} is a {kind} commit; only linear history can be rewritten",
                short(c)
            )
            .into());
        }
    }

    let old_head = git::rev_parse(root, "HEAD")?;
    let mut parent = candidates[first].1[0].clone();
    let mut replayed = 0usize;
    for (commit, _) in &candidates[first + 1..] {
        if targets.contains(commit) {
            continue;
        }
        match git::cherry_pick_tree(root, &parent, commit)? {
            git::Pick::Tree(tree) => {
                let message = git::commit_message(root, commit)?;
                let (name, email, date) = git::author_of(root, commit)?;
                parent = git::commit_tree(root, &tree, &parent, &message, &name, &email, &date)?;
                replayed += 1;
            }
            git::Pick::Conflict => {
                return Err(format!(
                    "drop: {} does not apply without the change being dropped; nothing was \
                     modified. Drop it too to proceed.",
                    short(commit)
                )
                .into());
            }
        }
    }
    Ok(DropPlan {
        old_head,
        new_head: parent,
        targets,
        replayed,
    })
}

// Verify, without modifying anything, that the given commits can be dropped together. Backs the
// interactive picker, which reports a failure to the operator before committing to the operation.
// Returns the number of later commits that would be re-applied.
pub fn check_drop(cwd: &Path, selectors: &[String]) -> Res<usize> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let branch = git::current_branch(&root)?;
    if git::operation_in_progress(&git_dir) {
        return Err("drop: a merge, rebase, cherry-pick, revert, or bisect is in progress".into());
    }
    Ok(plan_drop(&root, &branch, selectors)?.replayed)
}

// The commits `drop` would accept on the checked-out branch, newest first (the order `git log`
// prints), as (full id, subject) pairs. An empty subject stands for a placeholder atomic commit.
pub fn drop_choices(cwd: &Path) -> Res<Vec<(String, String)>> {
    let root = git::work_tree(cwd)?;
    let branch = git::current_branch(&root)?;
    let (candidates, _) = drop_candidates(&root, &branch)?;
    let mut out = Vec::with_capacity(candidates.len());
    for (commit, _) in candidates.iter().rev() {
        let message = git::commit_message(&root, commit)?;
        let subject = message.lines().next().unwrap_or("").trim().to_string();
        out.push((commit.clone(), subject));
    }
    Ok(out)
}

// The rewrite proper, run with the watcher already stopped. Selectors are resolved again here
// because the final capture may have amended the newest atomic commit, changing its id.
fn drop_locked(root: &Path, branch: &str, selectors: &[String], dry_run: bool) -> Res<()> {
    let plan = plan_drop(root, branch, selectors).map_err(|e| {
        format!("{e} (a capture of pending edits may have amended the newest commit; re-check it)")
    })?;
    let DropPlan {
        old_head,
        new_head: parent,
        targets,
        replayed,
    } = plan;

    if dry_run {
        println!("gitomic: dry run; nothing was modified");
    } else {
        // The watcher is stopped, so HEAD can only differ if something outside gitomic moved it
        // meanwhile.
        if git::rev_parse(root, "HEAD")? != old_head {
            return Err(
                "drop: HEAD moved while the rewrite was prepared; nothing was modified".into(),
            );
        }
        git::reset_keep(root, &parent, "gitomic drop").map_err(|e| {
            format!(
                "drop: cannot update the work tree ({e}); commit or stash uncommitted edits to the \
                 files involved and retry. Nothing was modified."
            )
        })?;
    }

    let verb = if dry_run { "would drop" } else { "dropped" };
    for commit in &targets {
        let message = git::commit_message(root, commit)?;
        let subject = message.lines().next().unwrap_or("").trim();
        let subject = if subject.is_empty() {
            "(no message)"
        } else {
            subject
        };
        println!("gitomic: {verb} {} {subject}", short(commit));
    }
    println!(
        "  {replayed} later commit(s) re-applied; {} -> {}",
        short(&old_head),
        short(&parent)
    );
    if !dry_run {
        println!("  recover a dropped commit with: git cherry-pick <full id>");
        for commit in &targets {
            println!("    {commit}");
        }
    }
    Ok(())
}

// End the session: stop the watcher, then stamp one message across every atomic commit recorded since the
// base. `message` short-circuits the editor; otherwise the configured editor is launched with a template.
pub fn finish(cwd: &Path, message: Option<String>, numbering_override: Option<bool>) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let branch = git::current_branch(&root)?;
    let sdir = state_dir(&git_dir, &branch);
    let base_ref = git::base_ref(&branch);
    let cfg = Config::load()?;

    if !git::rev_exists(&root, &base_ref)? {
        if let Some(sb) = find_session_for_origin(&root, &git_dir, &branch)? {
            println!("gitomic: the session for '{branch}' is on '{sb}'; run 'git checkout {sb}' then 'gitomic finish'.");
        } else {
            println!(
                "gitomic: no active session for '{branch}' in {}",
                root.display()
            );
        }
        return Ok(());
    }

    let origin = read_session_origin(&git_dir, &branch); // None => legacy in-place session

    terminate_watcher(&git_dir, &branch)?;
    // Foreground capture before the batch is enumerated, so a change made after the watcher's last cycle is
    // included in the finalized batch rather than lost. Visible to the operator, unlike the former in-watcher
    // shutdown flush.
    report_flush(&root, &git_dir, &branch, &cfg);

    let base = git::rev_parse(&root, &base_ref)?;
    let head = git::rev_parse(&root, "HEAD")?;
    let commits = git::rev_list_reverse(&root, &format!("{base_ref}..HEAD"))?;

    if commits.is_empty() {
        git::delete_ref(&root, &base_ref)?;
        proc::clear_pid(&sdir);
        match origin {
            Some(origin_branch) => {
                clear_session_origin(&git_dir, &branch);
                // No work recorded: return to the origin branch and drop the empty session branch. A switch that
                // is blocked by uncommitted content is reported rather than forced.
                if let Err(e) = git::checkout(&root, &origin_branch) {
                    println!("gitomic: no atomic commits recorded; session cleared, but switching to '{origin_branch}' failed ({e}).");
                    println!("  'git checkout {origin_branch}' when ready, then 'git branch -D {branch}'.");
                } else {
                    git::delete_branch(&root, &branch)?;
                    println!("gitomic: no atomic commits recorded this session; session cleared, back on '{origin_branch}'");
                }
            }
            None => println!("gitomic: no atomic commits recorded this session; session cleared"),
        }
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

    match origin {
        None => {
            // Legacy in-place session (no ORIGIN marker): replay onto the base and advance this branch in place,
            // exactly as before the session-branch model.
            let new_head = replay(&root, &base, &commits, &message, numbering)?;
            git::update_ref_cas(
                &root,
                &format!("refs/heads/{branch}"),
                &new_head,
                &head,
                "gitomic finalize",
            )?;
            git::delete_ref(&root, &base_ref)?;
            proc::clear_pid(&sdir);
            println!(
                "gitomic: finalized {} atomic commit(s) on {branch}",
                commits.len()
            );
            println!("  {} -> {}", short(&base), short(&new_head));
        }
        Some(origin_branch) => finish_session_branch(
            &root,
            &git_dir,
            &branch,
            &origin_branch,
            &base,
            &head,
            &commits,
            &message,
            numbering,
            &sdir,
        )?,
    }
    Ok(())
}

// Integrate a session branch's finalized batch back onto its origin branch (issue #10). The origin branch's
// current tip decides the path: unmoved since the session began, the batch replays straight on and the origin
// branch advances to it; moved but still able to carry the batch, the batch is rebased on top; moved such that the
// batch does not apply cleanly, the finalized work is left on the session branch for a manual merge or pull
// request and the origin branch is untouched. The first two paths return to the origin branch and remove the
// session branch; the third leaves the operator on the session branch.
#[allow(clippy::too_many_arguments)]
fn finish_session_branch(
    root: &Path,
    git_dir: &Path,
    session_branch: &str,
    origin_branch: &str,
    base: &str,
    head: &str,
    commits: &[String],
    message: &str,
    numbering: bool,
    sdir: &Path,
) -> Res<()> {
    let base_ref = git::base_ref(session_branch);
    let origin_ref = format!("refs/heads/{origin_branch}");
    let origin_tip = git::rev_parse(root, &origin_ref)?;

    // The origin branch has not moved: the batch's own trees are valid on top of the base, so replay reuses them
    // and the result is identical to a plain commit onto the origin branch.
    if origin_tip == *base {
        let new_head = replay(root, base, commits, message, numbering)?;
        git::update_ref_cas(root, &origin_ref, &new_head, &origin_tip, "gitomic finalize")?;
        finalize_teardown(root, git_dir, session_branch, origin_branch, &base_ref, sdir);
        println!(
            "gitomic: finalized {} atomic commit(s) on {origin_branch}",
            commits.len()
        );
        println!("  {} -> {}", short(base), short(&new_head));
        return Ok(());
    }

    // The origin branch moved out-of-band during the session. Rebasing each commit's change onto the new tip via a
    // three-way merge (in the object database only) both integrates the movement and detects a conflict without
    // touching the work tree.
    match rebase_batch(root, &origin_tip, commits, message, numbering)? {
        Rebase::Done(new_head) => {
            git::update_ref_cas(root, &origin_ref, &new_head, &origin_tip, "gitomic finalize")?;
            finalize_teardown(root, git_dir, session_branch, origin_branch, &base_ref, sdir);
            println!(
                "gitomic: '{origin_branch}' advanced to {} during the session; integrated {} atomic commit(s) on top",
                short(&origin_tip),
                commits.len()
            );
            println!("  {} -> {}", short(&origin_tip), short(&new_head));
        }
        Rebase::Conflict(conflicted) => {
            // The batch cannot be replayed cleanly onto the moved origin tip. Finalize the messages onto the
            // session branch so it carries a clean, integrable history, then hand it over: the origin branch is
            // left exactly as the out-of-band move left it, and the session work survives on its own branch for a
            // manual merge or a pull request. Nothing is rewritten on the origin branch.
            let finalized = replay(root, base, commits, message, numbering)?;
            git::update_ref_cas(
                root,
                &format!("refs/heads/{session_branch}"),
                &finalized,
                head,
                "gitomic finalize",
            )?;
            git::delete_ref(root, &base_ref)?;
            proc::clear_pid(sdir);
            clear_session_origin(git_dir, session_branch);
            println!(
                "gitomic: '{origin_branch}' advanced to {} and the session's changes conflict with it (at {}).",
                short(&origin_tip),
                short(&conflicted)
            );
            println!(
                "  '{origin_branch}' is unchanged. The finalized session work ({} commit(s)) is on branch '{session_branch}'.",
                commits.len()
            );
            println!("  Reconcile it when ready, for example:");
            println!("    git checkout {session_branch} && git merge {origin_branch}");
            println!(
                "  or open a pull request from '{session_branch}' into '{origin_branch}', then delete '{session_branch}' once merged."
            );
        }
    }
    Ok(())
}

// Return to the origin branch and remove the now-integrated session branch, clearing the session's state. The
// work tree already holds the finalized content, so where the origin branch was unchanged the switch moves refs
// without touching files; where it advanced, the switch brings the integrated result onto disk. A switch blocked
// by uncommitted work-tree content is reported rather than forced, and the session artefacts are left in place so
// nothing is lost and the operator can complete the move by hand.
fn finalize_teardown(
    root: &Path,
    git_dir: &Path,
    session_branch: &str,
    origin_branch: &str,
    base_ref: &str,
    sdir: &Path,
) {
    if let Err(e) = git::checkout(root, origin_branch) {
        println!("  note: '{origin_branch}' was updated, but switching to it failed ({e}).");
        println!("        'git checkout {origin_branch}' when ready, then 'git branch -D {session_branch}'.");
        return;
    }
    let _ = git::delete_branch(root, session_branch);
    let _ = git::delete_ref(root, base_ref);
    proc::clear_pid(sdir);
    clear_session_origin(git_dir, session_branch);
}

// Outcome of replaying a finalized batch onto a moved origin tip.
enum Rebase {
    // Every commit applied cleanly; carries the new batch head on top of the moved tip.
    Done(String),
    // A commit did not apply without conflict; carries that commit's id.
    Conflict(String),
}

// Replay each atomic commit's own change onto `onto`, oldest first, giving each the finalized message while
// preserving its authorship. The three-way merge runs entirely in the object database (see git::cherry_pick_tree),
// so the work tree and index are never touched and a conflict is detected without leaving a half-applied state.
fn rebase_batch(
    root: &Path,
    onto: &str,
    commits: &[String],
    message: &str,
    numbering: bool,
) -> Res<Rebase> {
    let n = commits.len();
    let mut parent = onto.to_string();
    for (i, commit) in commits.iter().enumerate() {
        match git::cherry_pick_tree(root, &parent, commit)? {
            git::Pick::Tree(tree) => {
                let (an, ae, ad) = git::author_of(root, commit)?;
                let msg = if numbering {
                    numbered(message, i + 1, n)
                } else {
                    message.to_string()
                };
                parent = git::commit_tree(root, &tree, &parent, &msg, &an, &ae, &ad)?;
            }
            git::Pick::Conflict => return Ok(Rebase::Conflict(commit.clone())),
        }
    }
    Ok(Rebase::Done(parent))
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
    t.push_str("#\n");
    t.push_str("# To remove a commit from this list, leave the message empty, run\n");
    t.push_str("# 'gitomic drop <hash>', then run finish again.\n");
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
pub(crate) fn terminate_watcher(git_dir: &Path, branch: &str) -> Res<bool> {
    let sdir = state_dir(git_dir, branch);
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
pub(crate) fn report_flush(root: &Path, git_dir: &Path, branch: &str, cfg: &Config) {
    let observed = watch::take_pending_observed(&state_dir(git_dir, branch));
    match watch::flush_once(root, git_dir, &git::base_ref(branch), cfg, &observed) {
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
fn wait_for_watcher(git_dir: &Path, branch: &str, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if live_watcher(git_dir, branch).is_some() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

// Guard against starting a per-branch session while a pre-#4 unscoped session marker is still present. The two
// ref schemes cannot coexist on disk (see git::LEGACY_BASE_REF), so proceeding would otherwise fail on a raw
// ref-conflict error from `update-ref` with no indication of what it means or how to resolve it.
fn reject_legacy_session(root: &Path) -> Res<()> {
    if !git::rev_exists(root, git::LEGACY_BASE_REF)? {
        return Ok(());
    }
    let base = git::rev_parse(root, git::LEGACY_BASE_REF)?;
    let pending = git::count(root, &format!("{}..HEAD", git::LEGACY_BASE_REF))?;
    Err(format!(
        "a pre-branch-scoped session marker exists ({}, base {}, {pending} pending commit(s) against the \
         branch it was recorded on). Recover any pending work — 'git log {}..HEAD' shows the commits — then \
         remove the marker with 'git update-ref -d {}' before starting a new session.",
        git::LEGACY_BASE_REF,
        short(&base),
        git::LEGACY_BASE_REF,
        git::LEGACY_BASE_REF
    )
    .into())
}

// Abbreviate an object id for display, matching git's conventional short length.
pub(crate) fn short(sha: &str) -> String {
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

// Tests for `drop` run against throwaway repositories in the system temporary directory, one per
// test, so no test depends on another's state or on the developer's own git configuration.
#[cfg(test)]
mod drop_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    // A fresh repository on branch `main` with a local identity, removed on drop.
    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Repo {
            let n = NEXT.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("gitomic-drop-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let repo = Repo(dir);
            repo.git(&["init", "-q", "-b", "main"]);
            repo.git(&["config", "user.name", "Test"]);
            repo.git(&["config", "user.email", "test@example.invalid"]);
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            git::run(&self.0, args).unwrap()
        }

        // Write `content` to `name` and record it as one commit with `msg`.
        fn commit_file(&self, name: &str, content: &str, msg: &str) -> String {
            fs::write(self.0.join(name), content).unwrap();
            self.git(&["add", name]);
            self.git(&["commit", "-q", "-m", msg]);
            self.git(&["rev-parse", "HEAD"])
        }

        fn subjects(&self) -> Vec<String> {
            self.git(&["log", "--format=%s", "--reverse"])
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn drop(&self, hashes: &[&str], dry_run: bool) -> Res<()> {
            let sel: Vec<String> = hashes.iter().map(|h| h.to_string()).collect();
            drop_commits(&self.0, &sel, dry_run)
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn drops_a_middle_commit_and_its_files() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        let two = r.commit_file("b.txt", "b\n", "two");
        r.commit_file("c.txt", "c\n", "three");

        r.drop(&[&two[..8]], false).unwrap();

        assert_eq!(r.subjects(), ["one", "three"]);
        assert!(r.0.join("a.txt").exists());
        assert!(!r.0.join("b.txt").exists());
        assert!(r.0.join("c.txt").exists());
        assert!(!r.git(&["rev-list", "HEAD"]).contains(&two));
    }

    #[test]
    fn preserves_authorship_and_empty_messages_of_replayed_commits() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        let two = r.commit_file("b.txt", "b\n", "two");
        fs::write(r.0.join("c.txt"), "c\n").unwrap();
        r.git(&["add", "c.txt"]);
        r.git(&[
            "commit",
            "-q",
            "--allow-empty-message",
            "-m",
            "",
            "--author",
            "Other <other@example.invalid>",
        ]);

        r.drop(&[&two], false).unwrap();

        assert_eq!(
            r.git(&["log", "-1", "--format=%an <%ae>"]),
            "Other <other@example.invalid>"
        );
        assert_eq!(r.git(&["log", "-1", "--format=%s"]), "");
    }

    #[test]
    fn refuses_when_a_later_commit_depends_on_the_dropped_one() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        let two = r.commit_file("b.txt", "b1\n", "two");
        r.commit_file("b.txt", "b2\n", "three");
        let head = r.git(&["rev-parse", "HEAD"]);

        let err = r.drop(&[&two], false).unwrap_err().to_string();

        assert!(err.contains("does not apply"), "{err}");
        assert_eq!(r.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(fs::read_to_string(r.0.join("b.txt")).unwrap(), "b2\n");
    }

    #[test]
    fn dropping_the_dependent_commits_together_succeeds() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        let two = r.commit_file("b.txt", "b1\n", "two");
        let three = r.commit_file("b.txt", "b2\n", "three");

        r.drop(&[&three, &two], false).unwrap();

        assert_eq!(r.subjects(), ["one"]);
        assert!(!r.0.join("b.txt").exists());
    }

    #[test]
    fn refuses_a_commit_already_in_a_remote_tracking_branch() {
        let r = Repo::new();
        let one = r.commit_file("a.txt", "a\n", "one");
        r.commit_file("b.txt", "b\n", "two");
        r.git(&["update-ref", "refs/remotes/origin/main", &one]);

        let err = r.drop(&[&one], false).unwrap_err().to_string();

        assert!(err.contains("not unpublished"), "{err}");
        assert_eq!(r.subjects(), ["one", "two"]);
    }

    #[test]
    fn refuses_a_root_commit() {
        let r = Repo::new();
        let one = r.commit_file("a.txt", "a\n", "one");
        r.commit_file("b.txt", "b\n", "two");

        let err = r.drop(&[&one], false).unwrap_err().to_string();

        assert!(err.contains("root commit"), "{err}");
        assert_eq!(r.subjects(), ["one", "two"]);
    }

    #[test]
    fn keeps_uncommitted_edits_to_unrelated_files() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        let two = r.commit_file("b.txt", "b\n", "two");
        r.commit_file("c.txt", "c\n", "three");
        fs::write(r.0.join("a.txt"), "edited\n").unwrap();

        r.drop(&[&two], false).unwrap();

        assert_eq!(fs::read_to_string(r.0.join("a.txt")).unwrap(), "edited\n");
        assert_eq!(r.subjects(), ["one", "three"]);
    }

    #[test]
    fn refuses_when_an_uncommitted_edit_blocks_the_work_tree_update() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        let two = r.commit_file("b.txt", "b\n", "two");
        fs::write(r.0.join("b.txt"), "local edit\n").unwrap();
        let head = r.git(&["rev-parse", "HEAD"]);

        let err = r.drop(&[&two], false).unwrap_err().to_string();

        assert!(err.contains("Nothing was modified"), "{err}");
        assert_eq!(r.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(r.0.join("b.txt")).unwrap(),
            "local edit\n"
        );
    }

    #[test]
    fn dry_run_modifies_nothing() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        let two = r.commit_file("b.txt", "b\n", "two");
        r.commit_file("c.txt", "c\n", "three");
        let head = r.git(&["rev-parse", "HEAD"]);

        r.drop(&[&two], true).unwrap();

        assert_eq!(r.git(&["rev-parse", "HEAD"]), head);
        assert!(r.0.join("b.txt").exists());
    }

    #[test]
    fn rejects_an_unknown_hash() {
        let r = Repo::new();
        r.commit_file("a.txt", "a\n", "one");
        r.commit_file("b.txt", "b\n", "two");

        let err = r.drop(&["deadbeef"], false).unwrap_err().to_string();

        assert!(err.contains("does not name a single commit"), "{err}");
    }

    #[test]
    fn session_scope_excludes_commits_before_the_base() {
        let r = Repo::new();
        let one = r.commit_file("a.txt", "a\n", "one");
        r.commit_file("b.txt", "b\n", "two");
        let head = r.git(&["rev-parse", "HEAD"]);
        r.git(&["update-ref", &git::base_ref("main"), &head]);
        let three = r.commit_file("c.txt", "c\n", "three");

        let err = r.drop(&[&one], false).unwrap_err().to_string();
        assert!(err.contains("pending batch"), "{err}");

        r.drop(&[&three], false).unwrap();
        assert_eq!(r.subjects(), ["one", "two"]);
        assert_eq!(r.git(&["rev-parse", &git::base_ref("main")]), head);
    }
}

// Tests for the session-branch integration path of `finish` (issue #10). Each test builds a repository, forks a
// session branch by hand (mirroring what `init` plants: the base ref plus the ORIGIN marker), records atomic
// commits on it, and then drives `finish` directly, so the daemonized watcher is never involved.
#[cfg(test)]
mod session_branch_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Repo {
            let n = NEXT.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("gitomic-sb-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let r = Repo(dir);
            r.git(&["init", "-q", "-b", "main"]);
            r.git(&["config", "user.name", "Test"]);
            r.git(&["config", "user.email", "test@example.invalid"]);
            r
        }

        fn git(&self, args: &[&str]) -> String {
            git::run(&self.0, args).unwrap()
        }

        fn git_dir(&self) -> PathBuf {
            self.0.join(".git")
        }

        // Commit a file with an explicit message (used for real commits on the origin branch).
        fn commit_file(&self, name: &str, content: &str, msg: &str) -> String {
            fs::write(self.0.join(name), content).unwrap();
            self.git(&["add", name]);
            self.git(&["commit", "-q", "-m", msg]);
            self.git(&["rev-parse", "HEAD"])
        }

        // Commit a file with an empty message, matching what the watcher records as an atomic commit.
        fn atomic(&self, name: &str, content: &str) -> String {
            fs::write(self.0.join(name), content).unwrap();
            self.git(&["add", name]);
            self.git(&["commit", "-q", "--allow-empty-message", "-m", ""]);
            self.git(&["rev-parse", "HEAD"])
        }

        // Plant the session artefacts `init` would leave: the base ref for the session branch and the ORIGIN
        // marker recording the origin branch. The session branch itself is created by the caller.
        fn plant_session(&self, session_branch: &str, origin: &str, base: &str) {
            self.git(&["update-ref", &git::base_ref(session_branch), base]);
            write_session_origin(&self.git_dir(), session_branch, origin).unwrap();
        }

        fn current_branch(&self) -> String {
            self.git(&["symbolic-ref", "--short", "HEAD"])
        }

        fn subjects_of(&self, refname: &str) -> Vec<String> {
            self.git(&["log", "--format=%s", "--reverse", refname])
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn finish(&self, msg: &str) -> Res<()> {
            finish(&self.0, Some(msg.to_string()), Some(false))
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // The origin branch has not moved: the session's two atomic commits land on it as a straight, message-bearing
    // batch, the operator ends on the origin branch, and the session branch and its artefacts are gone.
    #[test]
    fn finish_integrates_onto_unmoved_origin_and_returns() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        let sb = session_branch_name("main", &base);
        r.git(&["checkout", "-q", "-b", &sb, &base]);
        r.atomic("s.txt", "one\n");
        r.atomic("s.txt", "one\ntwo\n");
        r.plant_session(&sb, "main", &base);

        r.finish("finalized work").unwrap();

        assert_eq!(r.current_branch(), "main");
        assert_eq!(
            r.subjects_of("main"),
            ["root", "finalized work", "finalized work"]
        );
        assert!(!git::branch_exists(&r.0, &sb).unwrap());
        assert!(!git::rev_exists(&r.0, &git::base_ref(&sb)).unwrap());
        assert!(read_session_origin(&r.git_dir(), &sb).is_none());
        assert_eq!(fs::read_to_string(r.0.join("s.txt")).unwrap(), "one\ntwo\n");
    }

    // The origin branch advanced with a non-conflicting commit during the session: the batch is rebased on top of
    // the new tip, preserving one commit per atomic step, and the operator ends on the origin branch.
    #[test]
    fn finish_rebases_onto_advanced_origin_without_conflict() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        let sb = session_branch_name("main", &base);
        r.git(&["checkout", "-q", "-b", &sb, &base]);
        r.atomic("s.txt", "session\n");
        // Origin advances with an unrelated file, out of band from the session.
        r.git(&["checkout", "-q", "main"]);
        r.commit_file("o.txt", "origin\n", "origin advance");
        r.git(&["checkout", "-q", &sb]);
        r.plant_session(&sb, "main", &base);

        r.finish("session change").unwrap();

        assert_eq!(r.current_branch(), "main");
        assert_eq!(
            r.subjects_of("main"),
            ["root", "origin advance", "session change"]
        );
        // Both the out-of-band file and the session's file are present on the integrated branch.
        assert_eq!(fs::read_to_string(r.0.join("o.txt")).unwrap(), "origin\n");
        assert_eq!(fs::read_to_string(r.0.join("s.txt")).unwrap(), "session\n");
        assert!(!git::branch_exists(&r.0, &sb).unwrap());
    }

    // The origin branch advanced with a commit that conflicts with the session's change: nothing is rewritten on
    // the origin branch, the finalized work is left on the session branch, and the operator stays there.
    #[test]
    fn finish_leaves_work_on_session_branch_on_conflict() {
        let r = Repo::new();
        let base = r.commit_file("shared.txt", "base\n", "root");
        let sb = session_branch_name("main", &base);
        r.git(&["checkout", "-q", "-b", &sb, &base]);
        r.atomic("shared.txt", "session edit\n");
        // Origin advances by editing the very same file to a different value.
        r.git(&["checkout", "-q", "main"]);
        let origin_tip = r.commit_file("shared.txt", "origin edit\n", "origin advance");
        r.git(&["checkout", "-q", &sb]);
        r.plant_session(&sb, "main", &base);

        r.finish("session change").unwrap();

        // Origin branch is exactly where the out-of-band commit left it — untouched by finish.
        assert_eq!(r.git(&["rev-parse", "main"]), origin_tip);
        assert_eq!(r.subjects_of("main"), ["root", "origin advance"]);
        // The operator remains on the session branch, which now carries the finalized message.
        assert_eq!(r.current_branch(), sb);
        assert_eq!(r.subjects_of(&sb), ["root", "session change"]);
        // The session is closed out: base ref and ORIGIN marker removed, so gitomic no longer manages the branch.
        assert!(!git::rev_exists(&r.0, &git::base_ref(&sb)).unwrap());
        assert!(read_session_origin(&r.git_dir(), &sb).is_none());
    }

    // A session with no recorded atomic commits clears cleanly and returns to the origin branch.
    #[test]
    fn finish_with_no_commits_returns_to_origin() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        let sb = session_branch_name("main", &base);
        r.git(&["checkout", "-q", "-b", &sb, &base]);
        r.plant_session(&sb, "main", &base);

        r.finish("unused").unwrap();

        assert_eq!(r.current_branch(), "main");
        assert!(!git::branch_exists(&r.0, &sb).unwrap());
        assert!(read_session_origin(&r.git_dir(), &sb).is_none());
    }

    // abort on a session branch discards the atomic commits, returns to the origin branch, removes the session
    // branch, and preserves the work-tree content as uncommitted.
    #[test]
    fn abort_returns_to_origin_and_preserves_files() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        let sb = session_branch_name("main", &base);
        r.git(&["checkout", "-q", "-b", &sb, &base]);
        r.atomic("s.txt", "work\n");
        r.plant_session(&sb, "main", &base);

        abort(&r.0, true).unwrap();

        assert_eq!(r.current_branch(), "main");
        assert!(!git::branch_exists(&r.0, &sb).unwrap());
        assert!(!git::rev_exists(&r.0, &git::base_ref(&sb)).unwrap());
        // The file the atomic commit recorded survives on disk, now untracked rather than committed.
        assert_eq!(fs::read_to_string(r.0.join("s.txt")).unwrap(), "work\n");
        assert_eq!(r.subjects_of("main"), ["root"]);
    }

    #[test]
    fn session_branch_name_appends_short_base() {
        let name = session_branch_name("feature/x", "0123456789abcdef0123");
        assert_eq!(name, "feature/x-0123456789ab");
    }
}
