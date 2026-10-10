// gitomic — a session-scoped helper for recording atomic commits.
//
// Working within a git repository, `gitomic init` forks a private session branch from the current HEAD and a
// background watcher that records each settled change onto it as its own commit bearing an empty placeholder
// message. `gitomic finish` stops the watcher, stamps a single message across every commit in the batch, and
// integrates the batch back onto the branch the operator began on, so a session of many recoverable steps
// collapses to one authored intent without losing per-step history. Recording on a private branch keeps an
// out-of-band move of the origin branch (a pull, another client, a direct-to-remote commit) from being
// mistaken for session work or overwriting it (issue #10). All commits are local; publishing remains an
// explicit, separate `git push`.
//
// The repository is inferred from the working directory, so any command may be run from anywhere in the tree.

use std::path::PathBuf;
use std::process::ExitCode;

mod cherry;
mod cherry_ui;
mod commands;
mod config;
mod conflict;
mod git;
mod history;
mod patch;
mod pick;
mod proc;
mod pull;
mod resolve;
mod restore;
mod restore_ui;
#[cfg(test)]
mod testrepo;
mod watch;
mod work;

// Application-wide fallible result. A boxed trait object keeps the error surface dependency-free while still
// carrying git's own diagnostics upward to the top-level reporter.
pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gitomic: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &[String]) -> Res<()> {
    let cwd = std::env::current_dir()?;
    let command = args.first().map(String::as_str).unwrap_or("");

    match command {
        "init" | "start" => {
            let opts = InitOpts::parse(&args[1..])?;
            commands::init(&cwd, opts.foreground, opts.verbose)
        }
        "finish" | "commit" => {
            let opts = FinishOpts::parse(&args[1..])?;
            commands::finish(&cwd, opts.message, opts.numbering)
        }
        "status" => commands::status(&cwd),
        "active" => commands::active(),
        "switch" | "s" => commands::switch(args.get(1).map(String::as_str)),
        "diff" => {
            let stat = args[1..].iter().any(|a| a == "--stat");
            commands::diff(&cwd, stat)
        }
        "build-safe" => {
            // Scriptable session gate. Exit code is the primary signal: 0 when no session is open (safe to
            // build), 1 when a session is open (not safe), 2 on error (e.g. not inside a repository), so a
            // script can distinguish "session active" from "gitomic could not answer". The words true/false
            // are printed for capture in a variable unless -q/--quiet is given. This arm sets the process exit
            // code directly rather than routing through the Ok/Err reporter, since "session open" is a normal
            // negative answer, not an error to be printed.
            let quiet = args[1..].iter().any(|a| a == "-q" || a == "--quiet");
            match commands::build_safe(&cwd) {
                Ok(safe) => {
                    if !quiet {
                        println!("{}", if safe { "true" } else { "false" });
                    }
                    std::process::exit(if safe { 0 } else { 1 });
                }
                Err(e) => {
                    eprintln!("gitomic: {e}");
                    std::process::exit(2);
                }
            }
        }
        "exec" | "run" => {
            let rest = &args[1..];
            // `-c` as the first token selects shell mode (the remaining tokens form one command string, su
            // -style); otherwise the tokens are an argv executed directly without a shell.
            let (shell, argv) = match rest.first().map(String::as_str) {
                Some("-c") => (true, rest[1..].to_vec()),
                _ => (false, rest.to_vec()),
            };
            commands::exec(&cwd, &argv, shell)
        }
        "drop" | "rm" => {
            let opts = DropOpts::parse(&args[1..])?;
            if opts.hashes.is_empty() {
                // No hash given: choose interactively, then drop through the ordinary path.
                match pick::run(&cwd)? {
                    Some(hashes) => commands::drop_commits(&cwd, &hashes, opts.dry_run),
                    None => {
                        println!("gitomic: nothing dropped");
                        Ok(())
                    }
                }
            } else {
                commands::drop_commits(&cwd, &opts.hashes, opts.dry_run)
            }
        }
        "cherry-pick" | "pick" => cherry::run(&cwd, parse_cherry(&args[1..])?),
        "restore" => restore::run(&cwd, parse_restore(&args[1..])?),
        "history" => history::run(&cwd, parse_history(&args[1..])?),
        "resolve" => {
            let opts = ResolveOpts::parse(&args[1..])?;
            resolve::run(&cwd, opts.decide, opts.finish)
        }
        "pull" => {
            let opts = PullOpts::parse(&args[1..])?;
            pull::run(&cwd, opts.action, !opts.no_fetch)
        }
        "stop" => commands::stop(&cwd),
        "unstick" => commands::unstick(
            &cwd,
            args[1..]
                .iter()
                .any(|a| a == "--force" || a == "-f" || a == "--yes" || a == "-y"),
        ),
        "abort" => commands::abort(
            &cwd,
            args[1..]
                .iter()
                .any(|a| a == "--force" || a == "-f" || a == "--yes" || a == "-y"),
        ),
        "-h" | "--help" | "help" | "" => {
            print_usage();
            Ok(())
        }
        "-V" | "--version" => {
            println!("gitomic {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!("unknown command '{other}' (try 'gitomic help')").into()),
    }
}

// Parsed options for the finish command.
struct FinishOpts {
    message: Option<String>,
    numbering: Option<bool>,
}

impl FinishOpts {
    // Parse finish arguments. `-m/--message <text>` supplies the message inline and skips the editor;
    // `-n/--numbering` and `--no-numbering` override the configured numbering behaviour. A bare positional
    // word "commit" is accepted as syntactic sugar for `gitomic finish commit` and otherwise ignored.
    fn parse(rest: &[String]) -> Res<FinishOpts> {
        let mut message = None;
        let mut numbering = None;
        let mut it = rest.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-m" | "--message" => {
                    let value = it.next().ok_or("expected text after -m/--message")?;
                    message = Some(value.clone());
                }
                "-n" | "--numbering" => numbering = Some(true),
                "--no-numbering" => numbering = Some(false),
                "commit" => {}
                other => return Err(format!("unexpected argument '{other}' for finish").into()),
            }
        }
        Ok(FinishOpts { message, numbering })
    }
}

// Parsed options for the drop command.
struct DropOpts {
    hashes: Vec<String>,
    dry_run: bool,
}

impl DropOpts {
    // Parse drop arguments. Every positional word is a commit hash (abbreviated or full);
    // `-n/--dry-run` reports what would be dropped, and whether every later commit still applies,
    // without changing anything. With no hash at all, the interactive picker is used.
    fn parse(rest: &[String]) -> Res<DropOpts> {
        let mut hashes = Vec::new();
        let mut dry_run = false;
        for arg in rest {
            match arg.as_str() {
                "-n" | "--dry-run" => dry_run = true,
                flag if flag.starts_with('-') => {
                    return Err(format!("unexpected option '{flag}' for drop").into())
                }
                hash => hashes.push(hash.to_string()),
            }
        }
        Ok(DropOpts { hashes, dry_run })
    }
}

// Parse cherry-pick arguments. Every positional word is a commit (abbreviated or full), replayed in
// the order given; with none, the interactive screen opens. `--from <branch>` names the branch that
// screen lists first; `-n/--dry-run` reports the patch without writing or applying it;
// `-p/--patch-only` writes the patch file and stops. `-s/--stash` takes single files out of a stash
// instead of commits from a branch (see `cherry::run`).
fn parse_cherry(rest: &[String]) -> Res<cherry::Opts> {
    let mut opts = cherry::Opts {
        only: None,
        from: None,
        hashes: Vec::new(),
        dry_run: false,
        patch_only: false,
        stash: false,
    };
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-p" | "--patch-only" => opts.patch_only = true,
            "-s" | "--stash" => opts.stash = true,
            "--only" => {
                let value = it.next().ok_or("expected a path after --only")?;
                opts.only = Some(value.clone());
            }
            "--from" => {
                let value = it.next().ok_or("expected a branch after --from")?;
                opts.from = Some(value.clone());
            }
            flag if flag.starts_with("--from=") => {
                opts.from = Some(flag["--from=".len()..].to_string());
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unexpected option '{flag}' for cherry-pick").into())
            }
            hash => opts.hashes.push(hash.to_string()),
        }
    }
    Ok(opts)
}

// Parse history arguments: exactly one path (relative to the current directory); `-n/--dry-run` and
// `-p/--patch-only` apply to the restore of a version chosen on the screen, as for `restore`.
fn parse_history(rest: &[String]) -> Res<history::Opts> {
    let mut opts = history::Opts {
        path: String::new(),
        dry_run: false,
        patch_only: false,
    };
    let mut named = false;
    for arg in rest {
        match arg.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-p" | "--patch-only" => opts.patch_only = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unexpected option '{flag}' for history").into())
            }
            path if !named => {
                opts.path = path.to_string();
                named = true;
            }
            _ => return Err("history takes one file".into()),
        }
    }
    if !named {
        return Err("history: name a file".into());
    }
    Ok(opts)
}

// Parse restore arguments. Every positional word is a path (relative to the current directory);
// `--from <rev>` names the branch, tag or commit the files are taken from; `-n/--dry-run` reports
// the patch without writing or applying it; `-p/--patch-only` writes the patch file and stops.
// `-s/--stash` takes the files from a stash; `-M/--modified` offers the tracked files whose edits are
// not staged, to be returned to their staged state. With no path, the interactive screen opens.
fn parse_restore(rest: &[String]) -> Res<restore::Opts> {
    let mut modified = false;
    let mut opts = restore::Opts {
        from: None,
        paths: Vec::new(),
        dry_run: false,
        patch_only: false,
        stash: false,
        merge: false,
    };
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-p" | "--patch-only" => opts.patch_only = true,
            "-s" | "--stash" => opts.stash = true,
            "-m" | "--merge" => opts.merge = true,
            "-M" | "--modified" => modified = true,
            "--from" => {
                let value = it.next().ok_or("expected a branch after --from")?;
                opts.from = Some(value.clone());
            }
            flag if flag.starts_with("--from=") => {
                opts.from = Some(flag["--from=".len()..].to_string());
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unexpected option '{flag}' for restore").into())
            }
            path => opts.paths.push(path.to_string()),
        }
    }
    if modified {
        if opts.stash || opts.from.is_some() {
            return Err("restore: --modified cannot be combined with --stash or --from".into());
        }
        opts.from = Some(restore::MODIFIED.to_string());
    }
    Ok(opts)
}

// Parsed options for the pull command.
struct PullOpts {
    action: pull::Action,
    no_fetch: bool,
}

impl PullOpts {
    // Parse pull arguments. With no outcome named the report is printed and nothing is changed, which is the
    // point of the command: the choice is made from the report rather than from a configuration setting fixed
    // in advance. `--no-fetch` reports against the remote-tracking refs as they stand, for a repository with
    // no network reachable.
    fn parse(rest: &[String]) -> Res<PullOpts> {
        let mut action = pull::Action::Report;
        let mut no_fetch = false;
        for arg in rest {
            match arg.as_str() {
                "--rebase" => action = pull::Action::Rebase,
                "--merge" => action = pull::Action::Merge,
                "--take-remote" => action = pull::Action::TakeRemote,
                "--no-fetch" => no_fetch = true,
                other => return Err(format!("unexpected argument '{other}' for pull").into()),
            }
        }
        Ok(PullOpts { action, no_fetch })
    }
}

// Parsed options for the resolve command.
struct ResolveOpts {
    decide: resolve::Decide,
    finish: bool,
}

impl ResolveOpts {
    // Parse resolve arguments. `--ours`/`--theirs` answer every conflict one way without opening the screen
    // and are mutually exclusive; `--continue` completes the operation once every path is staged. Completing
    // is not the default: the next step of a rebase may conflict again, and stopping with the result staged
    // leaves that decision with the operator.
    fn parse(rest: &[String]) -> Res<ResolveOpts> {
        let mut decide = resolve::Decide::Interactive;
        let mut finish = false;
        for arg in rest {
            match arg.as_str() {
                "--ours" => decide = resolve::Decide::Ours,
                "--theirs" => decide = resolve::Decide::Theirs,
                "--continue" => finish = true,
                other => return Err(format!("unexpected argument '{other}' for resolve").into()),
            }
        }
        Ok(ResolveOpts { decide, finish })
    }
}

// Parsed options for the init command.
struct InitOpts {
    foreground: bool,
    verbose: bool,
}

impl InitOpts {
    // Parse init arguments. `-f/--foreground` runs the watcher in the calling process with diagnostics on the
    // terminal instead of detaching it into the background; `-v/--verbose` adds a per-event trace line.
    // Foreground implies verbose, so the operator sees the event stream by default while reproducing a
    // scenario; `-v` alone fattens the detached watcher's log without keeping the process in the foreground.
    fn parse(rest: &[String]) -> Res<InitOpts> {
        let mut foreground = false;
        let mut verbose = false;
        for arg in rest {
            match arg.as_str() {
                "-f" | "--foreground" => foreground = true,
                "-v" | "--verbose" => verbose = true,
                other => return Err(format!("unexpected argument '{other}' for init").into()),
            }
        }
        Ok(InitOpts {
            foreground,
            verbose: verbose || foreground,
        })
    }
}

// Absolute path helper retained for potential future subcommands that accept an explicit repository path;
// currently every command resolves the repository from the working directory.
#[allow(dead_code)]
fn as_path(s: &str) -> PathBuf {
    PathBuf::from(s)
}

// Shell function offered to users for their .bashrc. Held as a raw string so that braces, backslashes and quotes
// reach the terminal verbatim; substituted into the help text as a format argument, which is not re-parsed.
const BASHRC_SNIPPET: &str = r#"    gitomicSessions() {
        local c_red c_blu c_grn c_rst c_ylw c_bold
        c_red=$'\033[31m'; c_grn=$'\033[32m'; c_ylw=$'\033[33m'
        c_blu=$'\033[34m'; c_rst=$'\033[0m'; c_bold=$'\033[1m'
        local active
        active=$(gitomic active)
        if [ "$active" ]; then
            printf "\n%s%s%s\n%s\n\n" \
                "$c_ylw$c_bold" "Active gitomic sessions:" "$c_rst" \
                "$active"
        fi
    }

    gitomicSwitch() {
        # No <n>: let the real binary write its numbered listing straight to the terminal, rather than
        # capturing it into a variable meant to hold a single cd/checkout target. `command gitomic`,
        # not a bare `gitomic`, throughout: once the `gitomic()` function below is also loaded, a bare
        # `gitomic` here would call that function instead of the binary, recursing into this same code.
        if [ $# -eq 0 ]; then
            command gitomic switch
            return
        fi
        local line target branch
        line=$(command gitomic switch "$@") || return 1
        IFS=$'\t' read -r target branch <<< "$line"
        cd -- "$target" || return 1
        # Two sessions can be live on the same repository at once, each on its own branch (issue #4's
        # per-branch session state), so getting to the right one is a checkout as well as a cd; a repo
        # already on that branch makes this a harmless no-op ("Already on '<branch>'").
        git checkout "$branch"
    }
    alias gs=gitomicSwitch

    # Shadows the `gitomic` binary with a same-named shell function, so `gitomic switch <n>` and
    # `gitomic s <n>` — typed exactly as such, no separate alias needed — actually land you in the
    # target repository and branch, not just print where it is. A compiled binary can never change its
    # parent shell's directory on its own (the reason gitomicSwitch/gs exists above); shadowing its own
    # name is what closes that gap without giving up the plain `gitomic <command>` interface for
    # everything else. Only `switch <n>` / `s <n>` (an argument present) is intercepted; `switch`/`s`
    # with no argument, and every other command, falls through to `command gitomic "$@"` — the real
    # binary — completely unchanged, including its exit code and interactive stdio (init -f, cherry-pick's
    # interactive screen, etc. all still work normally through this).
    gitomic() {
        case "$1" in
            switch|s)
                if [ $# -ge 2 ]; then
                    shift
                    gitomicSwitch "$@"
                    return
                fi
                ;;
        esac
        command gitomic "$@"
    }"#;

// Print the command reference, through a pager when stdout is a terminal.
fn print_usage() {
    proc::page(&usage_text());
}

// The command reference as a string.
fn usage_text() -> String {
    format!(
        "gitomic {} — record atomic commits as a repository changes, then stamp one message across the batch

USAGE:
  gitomic <command> [options]     run inside a git repository (any subdirectory)

COMMANDS:
                       Moving the origin branch out of band during a session (a pull, a push from another
                       client, a direct-to-remote commit later fetched) is safe: recording happens on a
                       private session branch, so an out-of-band move is never miscounted as session work,
                       and finish integrates the two lines rather than letting one overwrite the other.

  init [options]       Fork a private session branch '<branch>-<short base>' from HEAD, check it out, and
                       fork a watcher that records each settled change onto it. Reported and treated as the
                       branch you were on; the private branch is where commits actually land. If a session
                       is already open, resume it. Sessions are independent per branch. Aliases: start.
  finish [options]     Stop the watcher, apply one message to every atomic commit in the session, and
                       integrate the batch back onto the origin branch: a straight commit when the origin
                       branch has not moved, a replay on top when it advanced, or — if the work conflicts
                       with an out-of-band change — left on the session branch for a manual merge or pull
                       request while the origin branch is left untouched. Aliases: commit.
  status               Show every open session (origin branch, base, pending atomic-commit count, watcher
                       state, log path), the private session branch, and whether the origin branch has
                       moved since the session began. A branch with no open session is omitted.
  active               List every live gitomic watcher on this machine, across every repository — does not
                       need to be run from inside a repository. Prints nothing when nothing is running, so
                       it is quiet by default; meant to be called from a shell profile on new-terminal open.
  switch [<n>]         Without <n>, list every live gitomic watcher on this machine like 'active', with a
                       1-based number prepended; the numbering follows activation order, not the working
                       directory, and renumbers on its own as sessions close. With <n>, print that
                       session's '<repo path>\t<branch>' on stdout and nothing else — two sessions can be
                       live on the same repository at once, each on its own branch, so the branch is part
                       of what a selection resolves to, not just the repository. This binary, run
                       directly, can never change its parent shell's directory or checked-out branch, so
                       on its own it only ever prints that line; add the shell snippet below to make
                       'gitomic switch <n>' / 'gitomic s <n>' (and 'gs <n>') actually take you there.
                       Aliases: s.
  diff [--stat]        Show the consolidated diff of the checked-out branch's pending batch (base..HEAD) —
                       the atomic commits recorded so far this session, not the working tree. --stat prints
                       a summary instead of the full patch. Requires an active session.
  build-safe [-q]      Scriptable session gate for a build script. Prints 'true' when no session is open
                       (exit 0), 'false' when one is (exit 1); exit 2 on error. -q/--quiet suppresses the
                       word and returns the exit code only, e.g. 'gitomic build-safe -q || exit 1'.
  exec [-c] <cmd...>   Run <cmd> in the work tree, then capture its full effect as one atomic commit,
                       staging untracked files as well regardless of the configured stage mode. Requires an
                       active session. Mainly for `stage = tracked`; under `stage = observed` a live watcher
                       captures created files directly. `-c` runs a shell string (su-style); otherwise <cmd>
                       is an argv run without a shell. Aliases: run.
  drop [-n] [<hash>...] Delete individual unpublished commits from the checked-out branch: those in
                       the open session's pending batch or, with no session, any commit not
                       contained in a remote-tracking branch. Later commits are re-applied without
                       them, and the files the dropped commits changed are removed or restored on
                       disk; uncommitted edits to other files are kept. Refuses, changing nothing,
                       when a later commit depends on a dropped change or an uncommitted edit
                       blocks the update. -n/--dry-run only reports. Recover a drop with
                       'git cherry-pick <full id>' (printed). With no hash, opens an interactive
                       picker: commits on the left, the highlighted diff on the right, space to
                       mark, Enter to drop the marked commits after a y/n confirmation. Aliases: rm.
  cherry-pick [options] [<hash>...]
                       Bring commits from another branch onto the checked-out one by way of a
                       patch file, without rewriting history. The chosen commits are replayed onto
                       the current HEAD and the result is applied to the work tree; with a session
                       open it is recorded as one atomic commit (a live watcher is paused
                       meanwhile), otherwise it is left uncommitted. With no hash, opens an
                       interactive screen: commits on the left, what each would change on the
                       right, space to mark, R to follow one file (marks the commit and every older
                       commit that changes the same file, each applied for that file only), Tab to
                       choose the branch, Enter to prepare. A conflict opens a decision screen: a
                       keeps the tree copy, b takes the picked commit's, c keeps both, per conflict
                       hunk; X restores the whole file from the newest R-marked commit instead.
                       Options: --from <branch>, --only <path> (apply each named commit for that
                       file only), -n/--dry-run (report only), -p/--patch-only (write the patch
                       file, apply nothing). With hashes, conflicts are refused. S in the screen,
                       or -s/--stash, switches to single files out of the stashes (see restore);
                       the stash keeps them. Aliases: pick.
  restore [options] [<path>...]
                       Bring files from other branches into the work tree exactly as they are
                       there (the tip of the branch), overwriting the local copy. Paths are
                       relative to the current directory. --from <rev> names the branch, tag or
                       commit; without it the local branches must agree about each file. With a
                       session open, pending changes to tracked files are recorded first and the
                       restore is one atomic commit; otherwise it is left uncommitted and refused
                       when a local edit is in the way. With no path, opens an interactive
                       screen: files of every local branch on the left (Tab chooses the branches,
                       / filters by name or glob such as *.img, B shows only binary files), the
                       change each would make on the right, space to mark and move down, v to
                       choose between differing versions, P to view and delete the patch files
                       kept in .git/gitomic-picks, Enter to restore. Each file is its own patch; a
                       failure does not stop the others. Also reachable with F from the cherry-pick
                       screen. The stashes are listed after the branches (Tab) and offer only the
                       files the stash itself changed, untracked ones included; taking a file
                       leaves the stash as it was. PgUp/PgDn page the lists, h/l scroll an overlay.
                       Options: -n/--dry-run, -p/--patch-only, -s/--stash (start with the stashes
                       selected; with paths, take them from stash@{{0}}, or from the stash named
                       by --from, as stash@{{N}} or just N), -m/--merge (with paths: instead of
                       overwriting the local copy, go through the differences one change at a
                       time, a keeping the local lines, b taking the source's, c both, u undoing
                       a decision; nothing is written until every change is decided, and the
                       stash is left as it was),
                       -M/--modified (the tracked files that are modified or deleted, returned
                       to their staged state like 'git restore <path>'; the '(modified)' entry
                       of the Tab overlay is the same, and it excludes the branches and stashes.
                       The changes are discarded in place:
                       nothing is committed, no session is needed, and they cannot be brought
                       back; -n lists what would go, -p is refused).
  history [options] <path>
                       The commits of the checked-out branch that changed one file, newest first,
                       each with the change it made to that file (v switches the right pane to what
                       restoring that version would change on HEAD). Enter restores the highlighted
                       version after a y/n, through the same pipeline as restore: with a session
                       open it is one atomic commit, otherwise it is left uncommitted. Works for a
                       file that matches HEAD and for one that has been deleted (the version before
                       its deletion is listed); renames are not followed. Without a terminal it
                       prints the list (short id, date, subject), and 'gitomic restore --from <id>
                       <path>' takes any of those ids. Options: -n/--dry-run, -p/--patch-only.
  pull [options]       Report how the checked-out branch and its upstream differ, then integrate on request.
                       The report leads with the fact git withholds until a strategy has already been
                       chosen: how many of your commits are already upstream under a different hash (the
                       residue of a force-push or a restored branch), and how many are genuinely new.
                       With no option it reports and changes nothing. Options: --rebase (replay your new
                       commits onto the upstream), --merge (join both lines), --take-remote (discard yours;
                       a backup/<branch>-<time> branch is made first), --no-fetch (report against the
                       remote-tracking refs as they stand). A conflict stops with a pointer to
                       'gitomic resolve'. No pull.rebase configuration is read or written.
  resolve [options]    Decide the conflicts of an interrupted merge, rebase, cherry-pick, or revert and
                       stage the result. With no option, opens the decision screen: unmerged paths on the
                       left, the selected conflict on the right, a to keep our side, b to take theirs, c to
                       keep both, u to undo, Enter to stage once every conflict is decided. A binary file,
                       an add/add, or a modify/delete carries one whole-file decision instead of hunks.
                       Nothing is written until every conflict is decided. Options: --ours / --theirs
                       (answer every conflict one way, no screen), --continue (complete the operation
                       once staged, rather than leaving it for 'git commit' / 'git <op> --continue').
  stop                 Stop the watcher but keep the base and recorded commits for later finish/resume.
  unstick [--yes]      Abandon an interrupted merge, rebase, cherry-pick, revert, or bisect — the state that
                       makes drop and cherry-pick refuse to run — by way of git's own abort for whichever
                       one is detected, returning the repository to the state before it started. Without
                       --yes, names the operation and the exact git command and does nothing. A gitomic
                       session on the branch is preserved; a live watcher is stopped across the abort and
                       restarted. Alias: --force, -f, -y
  abort [--yes]        Discard the session: reset the branch to the base and drop the atomic commits.
                       Alias: --force, -f, -y
  help, --version

INIT OPTIONS:
  -f, --foreground       Run the watcher in this process with diagnostics on the terminal instead of
                         detaching it; Ctrl-C stops it and preserves the session. Implies --verbose.
  -v, --verbose          Trace every file-system event (kind, paths, and whether it armed the debounce
                         timer or was ignored as git-internal). Usable with the detached watcher too.

FINISH OPTIONS:
  -m, --message <text>   Use <text> as the message and skip the editor.
  -n, --numbering        Append ' [i/N]' to each commit message this run.
      --no-numbering     Do not append ordinals this run (override the config default).

NOTES:
  All commits are local; run 'git push' yourself to publish. Atomic commits and the finalize rewrite
  bypass git hooks. Config: ${{XDG_CONFIG_HOME:-~/.config}}/gitomic/gitomic.cfg.

  Add the following to your .bashrc file, and call gitomicSessions where it should run (for example, on
  its own line after the definition), to be reminded of running sessions on new-terminal open, and to gain
  what the binary alone cannot provide, since a subprocess can never change its parent shell's directory
  or checked-out branch: 'gitomic switch <n>' and 'gitomic s <n>', typed exactly like that (the last
  function below shadows the 'gitomic' binary itself with a same-named shell function, forwarding
  everything except 'switch <n>'/'s <n>' straight through to it unchanged), or 'gs <n>' as a shorter
  equivalent, actually cd into that session's repository and check out its branch — also covering a
  switch between two sessions live on the same repository at once, each on its own branch. With no <n>,
  'gitomic switch'/'gitomic s'/'gs' all still just list, exactly as the binary alone would:

{snippet}
",
        env!("CARGO_PKG_VERSION"),
        snippet = BASHRC_SNIPPET
    )
}
