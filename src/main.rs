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
        "init" | "start" => commands::init(&cwd),
        "finish" | "commit" => {
            let opts = FinishOpts::parse(&args[1..])?;
            commands::finish(&cwd, opts.message, opts.numbering)
        }
        "status" => commands::status(&cwd),
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
  init                 Mark HEAD as the session base and fork a background watcher. If a base already
                       exists without a running watcher, resume that session. Aliases: start.
  finish [options]     Stop the watcher and apply one message to every atomic commit in the session,
                       then clear the session. Aliases: commit.
  status               Show the session base, pending atomic-commit count, watcher state, and log path.
  stop                 Stop the watcher but keep the base and recorded commits for later finish/resume.
  abort [--force]      Discard the session: reset the branch to the base and drop the atomic commits.
  help, --version

FINISH OPTIONS:
  -m, --message <text>   Use <text> as the message and skip the editor.
  -n, --numbering        Append ' [i/N]' to each commit message this run.
      --no-numbering     Do not append ordinals this run (override the config default).

NOTES:
  All commits are local; run 'git push' yourself to publish. Atomic commits and the finalize rewrite
  bypass git hooks. Config: ${{XDG_CONFIG_HOME:-~/.config}}/gitomic/gitomic.cfg.",
        env!("CARGO_PKG_VERSION")
    );
}
