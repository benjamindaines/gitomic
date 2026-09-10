# Receipt — gitomic issue #2: `exec` command wrapper

Scope: adds a command wrapper that records the full effect of a wrapped command as one
atomic commit, staging untracked files regardless of include_untracked. Direct fix for the
reproduced cause (a creation patch under include_untracked=false stages nothing via
`git add -u`). Builds on the foreground/verbose change; same three files.

## src/watch.rs
- flush_once() body split. The precondition checks, staging, empty-diff test, and commit are
  now in a private flush_staged(root, git_dir, add_arg) taking the staging breadth as an
  argument. flush_once() becomes a thin selector: "-A" when cfg.include_untracked else "-u".
  Rationale: single source of truth for the capture logic; the breadth is the only variable.
- Added pub(crate) flush_all(root, git_dir): calls flush_staged with "-A" unconditionally.
  Used by exec so a command's newly created files are staged even under a tracked-only watcher
  policy. The watcher itself does not call flush_all; it continues to honour the config.
- Stale flush_once doc comment replaced to describe the selector role.

## src/commands.rs
- Added pub fn exec(cwd, argv, shell):
    * Requires an active session (BASE_REF exists); errors otherwise so the capture has a batch
      to join.
    * Runs the wrapped command in the work tree. shell=true joins argv into one string run via
      `sh -c` (su-style: globs/pipelines honoured); shell=false executes argv directly with no
      shell (no quoting/injection surface for `gitomic exec git apply file.patch`).
    * Non-zero exit aborts before any capture: a patch that does not apply must not be followed
      by a commit of a partial tree. Exit code or signal is reported.
    * On success, watch::flush_all records the full effect as one atomic commit; the outcome is
      printed (captured <sha> / produced no change / skipped / failed).
- Added command_label() to render the wrapped command in diagnostics.

## src/main.rs
- dispatch(): added "exec" | "run" arm. `-c` as the first token selects shell mode (remaining
  tokens form one command string); otherwise the tokens are a direct argv.
- print_usage(): documented `exec [-c] <cmd...>` under COMMANDS.

## Intended outcome
`gitomic exec git apply file.patch` (or `gitomic exec -c "git apply *.patch"`) applies the
patch and commits its complete result — including files the patch creates — into the current
session batch, independent of include_untracked. This is the general workaround for any
generator/patch step whose output includes new paths.

## Interaction with a live watcher
exec is independent of the watcher and may run alongside it. With include_untracked=false and
the watcher live, the watcher may commit the tracked subset (add -u) and exec then commits the
untracked remainder (add -A): the change is fully captured, possibly across two atomic commits,
which finalize collapses under one message. No change is lost. For a single clean atomic commit
per command, run exec without the watcher (init, stop, then exec), or rely on exec alone.

## Verification (local, rustc 1.75)
- cargo build --release: clean.
- include_untracked=false, creation patch via `exec git apply`: pending 0 -> 1 (previously 0).
- `exec -c "git apply <mod.patch>"`: pending -> 2.
- re-apply of an already-applied patch: exit 1, "no capture performed", pending unchanged.
- finish: batch finalized to one message.
rustfmt/clippy unavailable in sandbox; rust-build-verify CI is authoritative. Lines wrap <=120.
