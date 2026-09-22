# Receipt — issue #11: `active` is no-op or worse

## Context

`gitomic active` reported no running watchers even with sessions live, and the maintainer's
own diagnosis (registry file "never appears locked" and gets "nuked" on session teardown)
pointed at `src/proc.rs`'s cross-repository session registry (`with_registry` / `announce_active`
/ `retire_active` / `active_sessions`, added for issue #6).

## Root cause

`with_registry` (src/proc.rs) opened the registry file with:

```rust
OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&path)
```

`O_TRUNC` is applied by the kernel at `open()` time, before the `flock(LOCK_EX)` call that
follows and before the closure `f` runs. Every call into `with_registry` — a START append, a
STOP append, or the plain read that `active_sessions` performs — therefore discarded the
file's existing contents immediately on open, regardless of locking.

Effects observed:
- `gitomic active` opens the registry (wiping it), then reads the now-empty file and reports
  nothing running: the no-op behavior in the issue title.
- A watcher's own `announce_active` START line is wiped by the *next* process that merely opens
  the registry (e.g. another `active` invocation, or another watcher starting/stopping) — worse
  than a no-op, since it also destroys state a live watcher had already recorded.
- The maintainer's diagnosis ("never appears locked... gets nuked") was directionally correct:
  the file's destruction was real, though the mechanism was the unconditional truncate-on-open
  rather than the flock itself failing.

Confirmed in isolation with a standalone repro (`with_registry`'s exact `OpenOptions` pattern,
independent of `Config::dir()`): a first call writes a START line and it's readable back; a
second, read-only call sees an empty file, and the file is empty on disk afterward.

## Changes

`src/proc.rs`, `with_registry`: replaced `.truncate(true)` with an explicit `.truncate(false)`.
Existing registry content now survives every open; the two call sites that need to shrink the
file already do so deliberately via `file.set_len(0)` once they hold the lock
(`retire_active` when no session remains active anywhere, `active_sessions` when self-healing
against unclean-kill orphans) — no other logic needed to change.

`.truncate(false)` rather than simply omitting the call: `cargo clippy` flags
`create(true)` with undefined truncate behavior (`clippy::suspicious_open_options`); making the
intent explicit satisfies that lint and documents the decision for the next reader.

## Intended outcome

- `gitomic active` reports every currently-live watcher across the machine, matching what
  `announce_active`/`retire_active` have recorded.
- A registry read no longer has a destructive side effect.
- Registry entries persist across repeated opens (starts, stops, and reads interleaved from
  multiple processes) until explicitly retired or self-healed against a dead pid.

## Verification

- Standalone repro (outside the crate) reproduced the exact failure mode against the
  pre-fix `OpenOptions` sequence: a write followed by a read-only open lost the write.
- Added a regression test, `proc::registry_tests::with_registry_read_does_not_erase_prior_writes`,
  which announces a session, then performs a read-only `active_sessions()` call and asserts both
  the in-memory result and the on-disk file still contain the entry, then retires it and confirms
  it clears. Reverting only the `.truncate(false)` change (restoring `.truncate(true)`) makes this
  new test fail (`left: 0, right: 1` on the session count) and leaves the other 24 pre-existing
  tests passing, confirming the test isolates this bug rather than depending on the rest of the
  fix.
- Full suite: `cargo test` — 25 passed, 0 failed (24 pre-existing + 1 new).
- `cargo build` and `cargo clippy --all-targets` — no warnings from this change (one pre-existing,
  unrelated `clippy::collapsible_match` warning at `parse_registry`'s `"START"` arm was left
  untouched, out of scope for this issue).
- Built against the real crate dependencies (`libc`, `notify`) reconstructed from the repository's
  `Cargo.toml`, since the kernel source tree wasn't otherwise available locally; the git transport
  to the origin host is not reachable from this environment (see the note atop `repotool.sh`), so
  the fix could not be pushed as a normal `git push` — it is committed via `repotool.sh
  commit-files` against the Forgejo contents API instead.
