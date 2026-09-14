# Receipt — issue #6, `diff` and `active` (feat2 + feat3)

## What changed

### src/git.rs
- Added `spawn_inherit(dir, args) -> Res<ExitStatus>`: runs git with inherited stdio instead of capturing
  output. Every other wrapper in the file captures stdout for gitomic's own use; this one exists because
  `diff`'s output is meant for a human terminal, and capturing it would lose the caller's pager/color config.

### src/commands.rs
- Added `pub fn diff(cwd, stat: bool) -> Res<()>`: resolves branch + base_ref, errors if no session is open
  (same message shape as `stop`/`finish`), then calls `git::spawn_inherit` with `diff [--stat] <base>..HEAD`.
- Added `pub fn active() -> Res<()>`: the one command with no repository resolution at all — prints
  `proc::active_sessions()`, one line per live watcher, nothing when empty.
- `init()`, both the foreground and detached-daemon branches: added `proc::announce_active(&root, &branch)`
  immediately after each branch's successful `write_pid`, and `proc::retire_active(&root, &branch)`
  immediately after `watch::run` returns, before the existing `clear_pid`. No other command needed a paired
  call — `stop`/`finish`/`abort` signal the watcher and wait for *it* to exit; the watcher retires itself on
  its own exit path (the same code just added to `init`), regardless of who asked it to stop.

### src/config.rs
- Split `Config::path()` into `Config::dir()` (the `$XDG_CONFIG_HOME/gitomic` directory) plus `path()` now
  built from it. Done so `proc.rs` can put the registry beside the config file without duplicating the
  XDG-vs-`$HOME` fallback logic.

### src/proc.rs
- New section: `ActiveSession`, `registry_path`, `with_registry` (opens/creates the file, takes a whole-file
  `flock(LOCK_EX)` for the call's duration, releases on drop), `parse_registry` (replays START/STOP lines into
  the still-open set), `announce_active`, `retire_active`, `active_sessions`. Three new unit tests for
  `parse_registry` (paired start/stop, malformed-line tolerance, idempotent re-START).

### src/main.rs
- Dispatch arms for `active` and `diff [--stat]`; usage text for both.

## Why these choices

- **Whole-file flock, not fine-grained locking.** Contention is once per watcher start/stop, never per commit
  (watch.rs's debounce-cycle commits don't touch the registry at all). A single coarse lock held for a short
  read-plus-maybe-rewrite is simpler than anything finer and cheap at this call frequency.
- **Self-truncate on empty, not a rewrite-in-place prune.** Ben's stated preference was append-only with the
  last session to close clearing the log. Truncating to zero bytes only when the live set is provably empty
  keeps that: the common path never rewrites existing lines, only appends and — rarely — zeroes the whole
  file.
- **`active` also self-heals, not just `retire_active`.** An unclean kill (`kill -9`, OOM, crash) never reaches
  `retire_active`, so it can leave an orphaned START with no matching STOP. Rather than require a subsequent
  clean start/stop elsewhere to clear it, `active_sessions()` drops dead pids from what it reports *and*
  truncates the file itself if that leaves nothing live — so a single `active` call after a crash is enough to
  bring the registry back to empty, not just to report correctly around the stale entry.
- **The watcher retires itself, not `terminate_watcher`.** `terminate_watcher` (used by `stop`/`finish`/
  `abort`) only has `git_dir` and `branch`, not the work-tree `root` the registry line needs, and more to the
  point: the watcher process is the one whose own exit path already runs `clear_pid`. Piggybacking
  `retire_active` on that same exit path means every way a watcher can go down (Ctrl-C, `stop`, `finish`,
  `abort`, a signal from another terminal) retires it exactly once, without `terminate_watcher` needing to
  know the registry exists at all.

## Verified

- `cargo fmt --all --check`, `cargo clippy --all-targets -- -W clippy::all`, `cargo test` (17/17) all clean
  against the sandbox's pinned 1.75.0 toolchain.
- Manual end-to-end, two scratch repos under an isolated `$HOME`:
  - `active` before any session: silent (correct).
  - `init` in repo 1, `active` from repo 2's directory: reports repo 1, branch, pid — confirms the registry
    is genuinely cross-repository, not cwd-scoped.
  - `init` in repo 2 as well: `active` reports both; registry file has two unmatched START lines.
  - Edits + `diff --stat` and `diff` (via `GIT_PAGER=cat` for the non-interactive test run) against repo 1's
    pending batch: correct stat and patch output, scoped to `base..HEAD` only.
  - `finish` in repo 1: registry gains repo 1's STOP line, not yet truncated (repo 2 still live). `stop` in
    repo 2: registry truncates to 0 bytes; `active` prints nothing.
  - `kill -9` on a live watcher (simulating an unclean exit, no `retire_active` call reached): registry shows
    the orphaned START; `active` correctly omits it from its printed output and truncates the file back to
    empty in the same call, even though the pid was still passing `kill(pid, 0)` at that point (see note
    below) — confirming self-heal works from the read side, not just via a clean retire.

## One thing worth knowing, not fixed here

`kill -9`-ing the sandboxed watcher left it answering `kill(pid, 0) == 0` (i.e. `proc::alive()` reports it as
alive) for some time afterward — the pid had become a zombie, reaped slowly or not at all by this container's
init. `active_sessions()`'s liveness check still reported it as live in that window, matching the *existing*
`live_watcher()` behavior for the per-repo pidfile (same `alive()` primitive, same characteristic, not new
here). It resolved itself shortly after — likely once the zombie was reaped — and a subsequent command
(`abort --force`) correctly signalled and waited it out regardless. Distinguishing a zombie from a genuinely
live process (e.g. reading `/proc/<pid>/stat` state) would fix this precisely, but it's a pre-existing gap in
`alive()` itself, not something introduced by this change, and out of scope for issue #6.
