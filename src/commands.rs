// Command implementations. Each entry point resolves the target repository from the working directory, then
// operates through the git wrappers and process-control helpers.
//
// A session records onto the branch the operator is standing on. `init` plants a base marker,
// refs/gitomic/base/<branch>, at HEAD and detaches a watcher bound to that branch; the watcher's atomic commits
// land on the branch itself, and `finish` restamps one message across base..<branch> in place. Nothing is
// checked out at any point, so no operation of gitomic's moves HEAD or the work tree during a session.
//
// This is a deliberate reversal of issue #10, which recorded onto a private branch forked from HEAD and checked
// that branch out. That design made recording conditional on HEAD staying on the private branch, and issue #4's
// guard — a watcher stands down on any cycle where the checked-out branch is not its own — then silently muted
// the watcher as soon as anything returned HEAD to the operator's branch (issue #22). The two fixes composed
// into a recording failure that reported itself as healthy. Recording in place removes the condition rather
// than defending it: there is no second branch for the work tree to be stranded away from.
//
// What #10 was protecting against is retained as detection rather than as ref separation, since an out-of-band
// commit landing inside base..<branch> — a pull, a push from another client, a manual commit — would otherwise
// be restamped with the session message and lose its own. Two guards cover it:
//
//   - The watcher stands down when the pending batch holds a commit that is not one of its own placeholders
//     (see watch::foreign_in_batch), rather than extending a batch it cannot safely finalize.
//   - `finish` refuses to rewrite any commit a remote-tracking ref already contains, since restamping produces
//     new object ids and publishing them would require a force push.
//
// Neither guard needs a branch to park work on. A foreign commit is unpublished by definition once the second
// guard has passed, so the batch is replayed in place with that commit keeping its own message: history stays
// linear, the work is not duplicated, and nothing is misattributed. A published commit, or a merge, stops
// `finish` with the branch untouched and the session intact, because the choice between force-pushing and
// re-planting the base belongs to the operator.
//
// Session state is two artefacts per branch: the base marker above, and a pidfile under
// <git-dir>/gitomic/<branch> identifying that branch's live watcher (issue #4). Their presence or absence fully
// describes the session, so recovery after an unclean exit is a matter of inspecting them.
//
// Two earlier session shapes are still readable so that a session open across an upgrade is not stranded, and
// neither is created any more. A base marker with an ORIGIN file beside it is an issue #10 private-branch
// session: `finish`, `abort`, `diff`, and `status` handle it on its own branch and integrate back onto the
// recorded origin. A marker at the unscoped refs/gitomic/base path is a pre-#4 session, which `init` refuses
// and `status` reports.

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

// File beside the pidfile of an issue #10 private-branch session, naming the branch that session was forked
// from. It is no longer written — a session records on the operator's own branch, so there is no second name to
// record — and is read only to recognise such a session left open across an upgrade and finalize it back onto
// the branch it came from.
const ORIGIN_MARKER: &str = "ORIGIN";

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

// The private branch, if any, on which an issue #10 session for `origin` is still open. Only a session created
// by that version carries an ORIGIN file, so this matches nothing for a session recorded in place and is used
// solely to recognise a pre-upgrade session and direct the operator to finalize it on its own branch.
fn private_branch_session_for(root: &Path, git_dir: &Path, origin: &str) -> Res<Option<String>> {
    for sb in git::branches_with_session(root)? {
        if read_session_origin(git_dir, &sb).as_deref() == Some(origin) {
            return Ok(Some(sb));
        }
    }
    Ok(None)
}

// Marker line identifying a pre-push hook as gitomic's own, so the hook is never removed or overwritten when
// the operator has one of their own installed.
const HOOK_MARKER: &str = "# gitomic-push-guard";

// Body of the pre-push hook installed under `push_guard`. It refuses the push when the branch being pushed has
// an open session whose pending batch still holds placeholder commits, which are the commits `finish` has not
// yet given a message. A batch with no placeholders is not blocked, so a session left open over an already
// finalized batch does not wedge pushing. `git push --no-verify` bypasses this, as it does every pre-push hook.
const HOOK_BODY: &str = r#"#!/bin/sh
# gitomic-push-guard
# Refuses to push a branch whose gitomic session still holds unfinalized placeholder commits.
# Remove this hook, or set push_guard = no in gitomic.cfg, to disable it.
while read -r local_ref _local_sha _remote_ref _remote_sha; do
    case "$local_ref" in
        refs/heads/*) branch=${local_ref#refs/heads/} ;;
        *) continue ;;
    esac
    base="refs/gitomic/base/$branch"
    git rev-parse --verify --quiet "$base" >/dev/null || continue
    placeholders=$(git log --format=%s "$base..$local_ref" | grep -c '^$')
    [ "$placeholders" -gt 0 ] || continue
    echo "gitomic: refusing to push '$branch': $placeholders unfinalized commit(s) in the open session." >&2
    echo "  Run 'gitomic finish' first, or 'git push --no-verify' to override." >&2
    exit 1
done
exit 0
"#;

// Install the pre-push guard, unless a foreign pre-push hook is already present. An existing hook is left
// untouched and reported: silently replacing an operator's own hook would be worse than not guarding. Returns
// what happened so `init` can say so once rather than on every resume.
fn install_push_guard(git_dir: &Path) -> Res<Option<String>> {
    let path = git::pre_push_hook_path(git_dir);
    if let Ok(existing) = fs::read_to_string(&path) {
        if existing.contains(HOOK_MARKER) {
            return Ok(None); // already ours, nothing to say
        }
        return Ok(Some(format!(
            "push_guard is enabled but {} already exists and is not gitomic's; leaving it alone",
            path.display()
        )));
    }
    fs::create_dir_all(path.parent().ok_or("hooks directory has no parent")?)?;
    fs::write(&path, HOOK_BODY)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(Some(format!("push guard installed at {}", path.display())))
}

// Remove the pre-push guard, but only when it is the one gitomic wrote. Called when a session is closed so the
// repository is left as it was found; a hook the operator installed themselves is never touched.
fn remove_push_guard(git_dir: &Path) {
    let path = git::pre_push_hook_path(git_dir);
    if let Ok(body) = fs::read_to_string(&path) {
        if body.contains(HOOK_MARKER) {
            let _ = fs::remove_file(&path);
        }
    }
}

// Begin or resume a session on the branch the operator is standing on. A fresh session plants the base marker
// at HEAD and detaches a background watcher bound to that branch; nothing is checked out and HEAD does not move.
// Re-invoked while a session is already open on the branch, it resumes it — a no-op with a notice if the watcher
// is already live, a restart if it had been stopped — which is also how a session survives the watcher being
// killed. A pre-upgrade private-branch session for this branch is recognised and reported rather than joined or
// duplicated.
pub fn init(cwd: &Path, foreground: bool, verbose: bool) -> Res<()> {
    let root = git::work_tree(cwd)?;
    let git_dir = git::git_dir(cwd)?;
    let branch = git::current_branch(&root)?; // rejects a detached HEAD before any state is written
    reject_legacy_session(&root)?;

    // The base marker alone decides whether a session is already open here, so resuming carries no further
    // state. Planting it is the whole of starting one: no branch is created and the work tree is untouched.
    let resuming = git::rev_exists(&root, &git::base_ref(&branch))?;
    if !resuming {
        if let Some(sb) = private_branch_session_for(&root, &git_dir, &branch)? {
            println!(
                "gitomic: a session for '{branch}' is open on the private branch '{sb}', recorded by an \
                 earlier version."
            );
            println!(
                "  Finalize it there first: 'git checkout {sb}' then 'gitomic finish' (or 'gitomic abort')."
            );
            println!("  A new session started afterwards records in place on '{branch}'.");
            return Ok(());
        }
        let base = git::rev_parse(&root, "HEAD")?;
        git::update_ref(&root, &git::base_ref(&branch), &base, "gitomic init")?;
    }

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
    if cfg.push_guard {
        match install_push_guard(&git_dir) {
            Ok(Some(note)) => println!("  {note}"),
            Ok(None) => {}
            Err(e) => println!("  warning: could not install the push guard ({e})"),
        }
    }

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
            "exec: no active session on '{branch}'; run 'gitomic init' first so the captured \
            change joins a batch"
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
            Ok(private_branch_session_for(&root, &git_dir, &branch)?.is_none())
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
    let branches = git::branches_with_session(&root)?;
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
            // The branch's own tip, not HEAD: a session records only while its branch is the checked-out one, and
            // this report must stay meaningful for a session on a branch that currently is not.
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
                if let Ok(origin_tip) =
                    git::rev_parse(&root, &format!("refs/heads/{origin_branch}"))
                {
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
            // A commit that reached the batch from outside the session. Surfaced here because it changes what
            // `finish` will do: an unpublished one is replayed keeping its own message, while a merge or a
            // published one stops the finalize with the branch untouched.
            if let Some((id, subject)) =
                watch::foreign_in_batch(&root, &base_ref, &format!("refs/heads/{branch}"))
            {
                let what = if subject.is_empty() {
                    "no message; a merge or an empty-message commit".to_string()
                } else {
                    format!("\"{subject}\"")
                };
                println!("    not recorded by gitomic: {} ({what})", short(&id));
                println!("      finish keeps its own message; a merge or an already-pushed commit stops finish");
            }
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

// List every live watcher with a 1-based index prepended (no selector), or resolve one such index to its
// repository path and branch (a selector). A separate binary cannot change its parent shell's working
// directory or its checked-out branch, so the index form exists for the `gitomicSwitch`/`gs` shell
// function in the help text to consume — `cd` to the path, then `git checkout` the branch, covering both
// a switch to a different repository and a switch to a sibling session on the same repository's other
// branch (more than one watcher can be live in one repository at once, each on its own branch, per issue
// #4's per-branch session state); the listing form is `active`'s output with numbers added, not a second
// source of truth. The index is this session's position in `proc::active_sessions`'s own ordering, which
// is already activation order with anything no longer alive dropped (see the registry notes in proc.rs),
// so it never depends on the working directory `switch` happens to be run from, and it renumbers on its
// own the moment an earlier session closes and its entry drops out of that list — nothing here tracks or
// reassigns numbers explicitly. With a selector, the only stdout line is `<repo>\t<branch>`; every other
// message goes to stderr, so a diagnostic can never be captured into a `cd`/`checkout` target.
// Aliases: s.
pub fn switch(selector: Option<&str>) -> Res<()> {
    let sessions = proc::active_sessions();
    let selector = match selector {
        Some(s) => s,
        None => {
            for (i, s) in sessions.iter().enumerate() {
                println!("{})  {}  [{}]  pid {}", i + 1, s.repo, s.branch, s.pid);
            }
            return Ok(());
        }
    };
    if sessions.is_empty() {
        return Err("switch: no active gitomic sessions".into());
    }
    let n: usize = selector.parse().map_err(|_| {
        format!("switch: '{selector}' is not a session number (see 'gitomic switch' for the list)")
    })?;
    if n == 0 || n > sessions.len() {
        return Err(format!(
            "switch: no session {n} (valid range is 1..{}; see 'gitomic switch' for the list)",
            sessions.len()
        )
        .into());
    }
    let target = &sessions[n - 1];
    println!("{}\t{}", target.repo, target.branch);
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
        println!(
            "  {pending} atomic commit(s) preserved; run 'gitomic finish' to finalize \
            or 'gitomic init' to resume"
        );
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
        if let Some(sb) = private_branch_session_for(&root, &git_dir, &branch)? {
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
             Re-run with --yes to proceed."
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
            remove_push_guard(&git_dir);
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
        if let Some(sb) = private_branch_session_for(&root, &git_dir, &branch)? {
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
            finish_in_place(
                &root, &branch, &base, &head, &base_ref, &commits, &message, numbering, &sdir,
            )?;
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

// Finalize the batch on the branch it was recorded on. Every placeholder in base..HEAD receives the session
// message; a commit the session did not record keeps its own, so an out-of-band commit that landed in the range
// mid-session is carried through the rewrite rather than restamped. Authorship is preserved for all of them by
// `replay`, so only messages and object ids change.
//
// Two conditions make the rewrite unsafe and stop it with the session intact rather than proceeding:
//
//   - A commit in the range that a remote-tracking ref already contains. Restamping produces new object ids, so
//     publishing the result would require a force push; that is the operator's decision to make, not gitomic's.
//     `drop` refuses a published commit on the same grounds.
//   - A merge commit in the range. `replay` re-parents each commit onto a single parent, which cannot express a
//     merge, so the merge's second parent would be dropped silently.
//
// Both are reported with the offending commits named and the branch left exactly as it stands, so no work is
// lost and no history is rewritten without the operator choosing it.
#[allow(clippy::too_many_arguments)]
fn finish_in_place(
    root: &Path,
    branch: &str,
    base: &str,
    head: &str,
    base_ref: &str,
    commits: &[String],
    message: &str,
    numbering: bool,
    sdir: &Path,
) -> Res<()> {
    let range = format!("{base_ref}..HEAD");

    let published = git::published_in_range(root, &range)?;
    if !published.is_empty() {
        println!(
            "gitomic: {} of the {} pending commit(s) are already contained in a remote-tracking branch:",
            published.len(),
            commits.len()
        );
        for c in &published {
            println!("    {}", short(c));
        }
        println!(
            "  Finalizing rewrites every commit in the batch, which gives them new object ids, so publishing \
             the result would need a force push."
        );
        println!(
            "  The session is preserved and '{branch}' is unchanged. Either force-push after finalizing, or \
             move the published commits out of the batch first by re-planting the base:"
        );
        println!(
            "    git update-ref {base_ref} {}",
            short(published.last().unwrap_or(&String::new()))
        );
        return Ok(());
    }

    // Each commit paired with the message it will carry: the session message for a placeholder, its own for a
    // commit that arrived from elsewhere. Numbering counts only the restamped commits, so a preserved commit
    // does not consume an ordinal or inflate the total.
    let summaries = git::commit_summaries(root, &range)?;
    if let Some((id, _, _)) = summaries.iter().find(|(_, parents, _)| parents.len() > 1) {
        println!(
            "gitomic: the pending batch contains a merge commit ({}).",
            short(id)
        );
        println!(
            "  Finalizing replays each commit onto a single parent, which cannot carry a merge, so the rewrite \
             would discard its second parent."
        );
        println!(
            "  The session is preserved and '{branch}' is unchanged. Resolve the merge out of the batch — \
             'gitomic diff' shows what is pending — then finish."
        );
        return Ok(());
    }

    let restamped = summaries.iter().filter(|(_, _, s)| s.is_empty()).count();
    let mut ordinal = 0;
    let mut plan: Vec<(String, String)> = Vec::with_capacity(summaries.len());
    for (id, _, subject) in &summaries {
        if subject.is_empty() {
            ordinal += 1;
            let msg = if numbering && restamped > 1 {
                numbered(message, ordinal, restamped)
            } else {
                message.to_string()
            };
            plan.push((id.clone(), msg));
        } else {
            // A commit the session did not record: its message is carried through unchanged, so the rewrite
            // re-parents it without claiming it as session work.
            plan.push((id.clone(), git::commit_message(root, id)?));
        }
    }

    let new_head = replay(root, base, &plan)?;
    git::update_ref_cas(
        root,
        &format!("refs/heads/{branch}"),
        &new_head,
        head,
        "gitomic finalize",
    )?;
    git::delete_ref(root, base_ref)?;
    proc::clear_pid(sdir);
    remove_push_guard(&git::git_dir(root)?);
    let preserved = summaries.len() - restamped;
    println!("gitomic: finalized {restamped} atomic commit(s) on {branch}");
    if preserved > 0 {
        println!(
            "  {preserved} commit(s) the session did not record kept their own message and were replayed in place"
        );
    }
    println!("  {} -> {}", short(base), short(&new_head));
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
        let new_head = replay(root, base, &uniform_plan(commits, message, numbering))?;
        git::update_ref_cas(
            root,
            &origin_ref,
            &new_head,
            &origin_tip,
            "gitomic finalize",
        )?;
        finalize_teardown(
            root,
            git_dir,
            session_branch,
            origin_branch,
            &base_ref,
            sdir,
        );
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
            git::update_ref_cas(
                root,
                &origin_ref,
                &new_head,
                &origin_tip,
                "gitomic finalize",
            )?;
            finalize_teardown(
                root,
                git_dir,
                session_branch,
                origin_branch,
                &base_ref,
                sdir,
            );
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
            let finalized = replay(root, base, &uniform_plan(commits, message, numbering))?;
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

// Rebuild the batch onto the base, giving each commit the message paired with it while preserving its original
// tree and authorship. Each element is (commit, message), so the caller decides per commit whether it is being
// restamped with the session message or carrying its own. Returns the object id of the new batch head.
fn replay(root: &Path, base: &str, plan: &[(String, String)]) -> Res<String> {
    let mut parent = base.to_string();
    for (commit, msg) in plan {
        let tree = git::rev_parse(root, &format!("{commit}^{{tree}}"))?;
        let (an, ae, ad) = git::author_of(root, commit)?;
        parent = git::commit_tree(root, &tree, &parent, msg, &an, &ae, &ad)?;
    }
    Ok(parent)
}

// Pair every commit with the same message, for the private-branch paths that restamp a whole batch uniformly.
fn uniform_plan(commits: &[String], message: &str, numbering: bool) -> Vec<(String, String)> {
    let n = commits.len();
    commits
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let msg = if numbering {
                numbered(message, i + 1, n)
            } else {
                message.to_string()
            };
            (c.clone(), msg)
        })
        .collect()
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
    t.push_str(
        "# Lines starting with '#' are ignored. An empty message aborts \
        finalize and preserves the session.\n",
    );
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
        "a pre-branch-scoped session marker exists ({}, base {}, {pending} pending commit(s) \
         against the branch it was recorded on). Recover any pending work — 'git log {}..HEAD' \
         shows the commits — then remove the marker with 'git update-ref -d {}' before starting \
         a new session.",
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

        // Plant the artefacts an issue #10 `init` left behind: the base ref for the private branch and the
        // ORIGIN marker naming the branch it was forked from. Production no longer writes either, so the shape
        // is constructed here, in the module that still exercises finalizing it.
        fn plant_session(&self, session_branch: &str, origin: &str, base: &str) {
            self.git(&["update-ref", &git::base_ref(session_branch), base]);
            let sdir = state_dir(&self.git_dir(), session_branch);
            fs::create_dir_all(&sdir).unwrap();
            fs::write(sdir.join(ORIGIN_MARKER), format!("{origin}\n")).unwrap();
        }

        // The private-branch name that version derived from the origin branch and the fork point.
        fn session_branch_name(&self, origin: &str, base: &str) -> String {
            format!("{origin}-{}", short(base))
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
        let sb = r.session_branch_name("main", &base);
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
        let sb = r.session_branch_name("main", &base);
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
        let sb = r.session_branch_name("main", &base);
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
        let sb = r.session_branch_name("main", &base);
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
        let sb = r.session_branch_name("main", &base);
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
        let r = Repo::new();
        let name = r.session_branch_name("feature/x", "0123456789abcdef0123");
        assert_eq!(name, "feature/x-0123456789ab");
    }
}

// Tests for finalizing a session recorded in place on the operator's branch (issue #22). Each test builds a
// repository, plants the base marker `init` would leave, records atomic commits as the watcher would, and drives
// `finish` directly, so the daemonized watcher is never involved — the same boundary the session-branch tests
// above observe. The watcher's own behaviour across a branch switch is exercised by running the binary against a
// scratch repository; see the receipt for that transcript.
#[cfg(test)]
mod in_place_session_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Repo {
            let n = NEXT.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("gitomic-ip-{}-{n}", std::process::id()));
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

        // A commit carrying a message, standing for work that did not come from the watcher.
        fn commit_file(&self, name: &str, content: &str, msg: &str) -> String {
            fs::write(self.0.join(name), content).unwrap();
            self.git(&["add", name]);
            self.git(&["commit", "-q", "-m", msg]);
            self.git(&["rev-parse", "HEAD"])
        }

        // A commit with an empty message, matching what the watcher records.
        fn atomic(&self, name: &str, content: &str) -> String {
            fs::write(self.0.join(name), content).unwrap();
            self.git(&["add", name]);
            self.git(&["commit", "-q", "--allow-empty-message", "-m", ""]);
            self.git(&["rev-parse", "HEAD"])
        }

        // The single artefact an in-place `init` plants: the base marker for the current branch. No branch is
        // created and nothing is checked out, which is the whole of the change this module covers.
        fn plant_session(&self, branch: &str, base: &str) {
            self.git(&["update-ref", &git::base_ref(branch), base]);
        }

        // Mark `sha` as present on a remote by writing the remote-tracking ref directly, which is what
        // `rev-list --remotes` reads. Avoids needing a second repository to push to.
        fn publish(&self, sha: &str) {
            self.git(&["update-ref", "refs/remotes/origin/main", sha]);
        }

        fn subjects(&self) -> Vec<String> {
            self.git(&["log", "--format=%s", "--reverse", "HEAD"])
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn branches(&self) -> Vec<String> {
            self.git(&["branch", "--format=%(refname:short)"])
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

    // The ordinary case: every commit in the batch is a placeholder, so all of them take the session message,
    // the branch advances in place, no branch is created, and the session artefacts are cleared.
    #[test]
    fn finish_restamps_the_batch_in_place_without_creating_a_branch() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        r.plant_session("main", &base);
        r.atomic("a.txt", "a\n");
        r.atomic("b.txt", "b\n");

        r.finish("session work").unwrap();

        assert_eq!(r.subjects(), ["root", "session work", "session work"]);
        assert_eq!(r.branches(), ["main"], "no branch is created at any point");
        assert_eq!(r.git(&["symbolic-ref", "--short", "HEAD"]), "main");
        assert!(!git::rev_exists(&r.0, &git::base_ref("main")).unwrap());
    }

    // A commit that landed in the batch from outside the session keeps its own message and its position, while
    // the placeholders around it are restamped. This is the case that made recording on a private branch seem
    // necessary: before the fix, `replay` gave every commit in the range the session message.
    #[test]
    fn a_commit_the_session_did_not_record_keeps_its_own_message() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        r.plant_session("main", &base);
        r.atomic("a.txt", "a\n");
        r.commit_file("up.txt", "up\n", "upstream: unrelated fix");
        r.atomic("b.txt", "b\n");

        r.finish("session work").unwrap();

        assert_eq!(
            r.subjects(),
            [
                "root",
                "session work",
                "upstream: unrelated fix",
                "session work"
            ]
        );
        assert!(!git::rev_exists(&r.0, &git::base_ref("main")).unwrap());
    }

    // Authorship of a preserved commit survives the replay, so a commit carried through the rewrite is not
    // reattributed to whoever ran finish.
    #[test]
    fn a_preserved_commit_keeps_its_author() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        r.plant_session("main", &base);
        r.atomic("a.txt", "a\n");
        fs::write(r.0.join("up.txt"), "up\n").unwrap();
        r.git(&["add", "up.txt"]);
        r.git(&[
            "-c",
            "user.name=Someone Else",
            "-c",
            "user.email=else@example.invalid",
            "commit",
            "-q",
            "-m",
            "theirs",
        ]);

        r.finish("session work").unwrap();

        let authors = r.git(&["log", "--format=%an", "--reverse", "HEAD"]);
        assert!(
            authors.lines().any(|a| a == "Someone Else"),
            "preserved commit keeps its author: {authors}"
        );
    }

    // A commit a remote already holds cannot be rewritten without a force push, so finish declines and changes
    // nothing: the branch stays where it is and the session stays open for the operator to decide.
    #[test]
    fn finish_refuses_to_rewrite_a_published_commit_and_preserves_the_session() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        r.plant_session("main", &base);
        let pushed = r.atomic("a.txt", "a\n");
        r.publish(&pushed);
        r.atomic("b.txt", "b\n");
        let head = r.git(&["rev-parse", "HEAD"]);

        r.finish("session work").unwrap();

        assert_eq!(r.git(&["rev-parse", "HEAD"]), head, "branch is untouched");
        assert!(
            git::rev_exists(&r.0, &git::base_ref("main")).unwrap(),
            "session is preserved"
        );
    }

    // A merge in the batch cannot be expressed by a single-parent replay, so finish declines rather than
    // silently dropping the merge's second parent.
    #[test]
    fn finish_refuses_a_merge_in_the_batch_and_preserves_the_session() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        r.git(&["checkout", "-q", "-b", "side"]);
        r.commit_file("s.txt", "s\n", "side work");
        r.git(&["checkout", "-q", "main"]);
        r.plant_session("main", &base);
        r.atomic("a.txt", "a\n");
        r.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        let head = r.git(&["rev-parse", "HEAD"]);

        r.finish("session work").unwrap();

        assert_eq!(r.git(&["rev-parse", "HEAD"]), head, "branch is untouched");
        assert!(git::rev_exists(&r.0, &git::base_ref("main")).unwrap());
    }

    // An empty batch clears the session without touching the branch and without leaving a branch behind.
    #[test]
    fn finish_with_nothing_recorded_clears_the_session_in_place() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        r.plant_session("main", &base);

        r.finish("session work").unwrap();

        assert_eq!(r.git(&["rev-parse", "HEAD"]), base);
        assert_eq!(r.branches(), ["main"]);
        assert!(!git::rev_exists(&r.0, &git::base_ref("main")).unwrap());
    }

    // The batch audit distinguishes the watcher's own placeholders from anything else in the range.
    #[test]
    fn the_batch_audit_finds_only_commits_the_session_did_not_record() {
        let r = Repo::new();
        let base = r.commit_file("root.txt", "root\n", "root");
        r.plant_session("main", &base);
        r.atomic("a.txt", "a\n");
        assert!(watch::foreign_in_batch(&r.0, &git::base_ref("main"), "HEAD").is_none());

        r.commit_file("up.txt", "up\n", "upstream fix");
        let found = watch::foreign_in_batch(&r.0, &git::base_ref("main"), "HEAD");
        assert_eq!(found.map(|(_, s)| s), Some("upstream fix".to_string()));
    }
}
