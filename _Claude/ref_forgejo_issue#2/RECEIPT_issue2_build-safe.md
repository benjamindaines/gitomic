# Receipt — gitomic issue #2: `build-safe` session gate

Scope: adds a scriptable predicate reporting whether the repository is free of a gitomic
session, for gating a build script. Two files touched (commands.rs, main.rs); watch.rs
unchanged from the exec delivery.

## src/commands.rs
- Added pub fn build_safe(cwd) -> Res<bool>: returns true when refs/gitomic/base is absent.
  A session is defined by that base marker and is independent of watcher liveness: a stopped-
  but-unfinished session still holds the tree in a recording state and is reported not
  build-safe. The function only queries; the exit-code mapping is the caller's.

## src/main.rs
- dispatch(): added "build-safe" arm. Parses -q/--quiet, calls build_safe, and sets the
  process exit code directly:
    * exit 0, prints "true"  -> no session open (safe to build)
    * exit 1, prints "false" -> session open (not safe)
    * exit 2, message on stderr -> error (e.g. not inside a repository)
  Rationale: "session open" is a normal negative answer, not an error, so it must not route
  through the Err reporter (which prints "gitomic: ..." and cannot yield a distinct code).
  Distinct code 2 lets a script separate "session active" from "could not answer". -q
  suppresses the word for exit-code-only use. The arm diverges via process::exit, which
  coerces to the arm's Res<()> type without a signature change to dispatch.
- print_usage(): documented `build-safe [-q]` under COMMANDS.

## Intended outcome
A build script halts while a session is open:
    gitomic build-safe -q || { echo "gitomic session active"; exit 1; }
or
    if gitomic build-safe -q; then make ...; fi
Gating the build this way removes the need to run the watcher with include_untracked
disabled to keep build artifacts out of the session: with the build blocked during a
session, untracked capture can be left enabled (which is what fixes the patch case).

## Verification (local, rustc 1.75)
- no session:                 stdout "true",  exit 0
- session open, watcher live: stdout "false", exit 1
- session open, watcher stopped (post `stop`): stdout "false", exit 1; -q -> exit 1, no stdout
- after `finish`:             stdout "true",  exit 0
- outside a git repository:   exit 2, diagnostic on stderr
- `if gitomic build-safe -q; then ...` halts the build while a session is active.
rustfmt/clippy unavailable in sandbox; rust-build-verify CI is authoritative. Lines wrap <=120.
