// gitomic — a session-scoped helper for recording atomic commits.
//
// Working within a git repository, `gitomic init` marks the current HEAD as a session base and forks a
// background watcher that records each settled change as its own commit bearing an empty placeholder message.
// `gitomic finish` stops the watcher and stamps a single message across every commit in the batch, so a
// session of many recoverable steps collapses to one authored intent without losing per-step history. All
// commits are local; publishing remains an explicit, separate `git push`.
//
// The repository is inferred from the working directory, so any command may be run from anywhere in the tree.

use std::path::PathBuf;
use std::process::ExitCode;

mod commands;
mod config;
mod git;
mod pick;
mod proc;
mod watch;

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
        "stop" => commands::stop(&cwd),
        "abort" => commands::abort(&cwd, args[1..].iter().any(|a| a == "--force" || a == "-f")),
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

fn print_usage() {
    println!(
        "gitomic {} — record atomic commits as a repository changes, then stamp one message across the batch

USAGE:
  gitomic <command> [options]     run inside a git repository (any subdirectory)

COMMANDS:
                       NOTE: gitomic does not currently verify that the commit marked at the beginning of
                       a session is still valid! Until that is added, do not move HEAD outside of gitomic.
                       Example: don't pull while gitomic is running or merge or anything like that.

  init [options]       Mark HEAD as the session base for the checked-out branch and fork a watcher bound to
                       that branch. If a base already exists for it without a running watcher, resume that
                       session. Sessions are independent per branch: switching branches leaves this one's
                       watcher running but idle until it is checked out again, and 'init' on the new branch
                       starts (or resumes) that branch's own session. Aliases: start.
  finish [options]     Stop the watcher and apply one message to every atomic commit in the session,
                       then clear the session. Aliases: commit.
  status               Show every branch with an open session (base, pending atomic-commit count, watcher
                       state, log path), marking whichever is currently checked out. A branch with no open
                       session is omitted.
  active               List every live gitomic watcher on this machine, across every repository — does not
                       need to be run from inside a repository. Prints nothing when nothing is running, so
                       it is quiet by default; meant to be called from a shell profile on new-terminal open.
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
  stop                 Stop the watcher but keep the base and recorded commits for later finish/resume.
  abort [--force]      Discard the session: reset the branch to the base and drop the atomic commits.
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

  Add the following to your .bashrc file to be reminded of running sessions (add a call for it as well
  where you would like it to run.

    gitomicSessions() { 
        local c_red c_blu c_grn c_rst c_ylw c_bold 
        c_red=$'\033[31m'; c_grn=$'\033[32m'; c_ylw=$'\033[33m' 
        c_blu=$'\033[34m'; c_rst=$'\033[0m'; c_bold=$'\033[1m' 
        local active=$(gitomic active) 
        if [ "$active" ]; then 
                printf "\n%s%s%s\n%s\n\n" \ 
                        "$c_ylw$c_bold" "Active gitomic sessions:" "$c_rst" \ 
                        "$active" 
        fi 
} ",
        env!("CARGO_PKG_VERSION")
    );
}
