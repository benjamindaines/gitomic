# Receipt — issue #4: branch-scoped sessions

## Root cause (confirmed, matches issue text)

Session state was a single, repository-wide artefact: one ref (`refs/gitomic/base`, `src/git.rs`) and one
pidfile (`<git-dir>/gitomic/watch.pid`, `src/proc.rs`). Both the watcher's commit cycle and `finish`
(`src/commands.rs`) resolved "the current branch" from `git symbolic-ref HEAD` at the moment they ran, with no
record of which branch was checked out when the session began. A mid-session `checkout`/`switch` was ordinary,
undetected, and silently redirected all further recording (and `finish`'s replay) onto the newly checked-out
branch.

## Fix

Session state is now keyed by branch, everywhere:

- `git::base_ref(branch)` replaces the single `BASE_REF` constant, returning `refs/gitomic/base/<branch>`.
- `git::state_dir(git_dir, branch)` replaces the single per-repository state directory, returning
  `<git-dir>/gitomic/<branch>` (pidfile, log, finalize template all nest under it). Previously duplicated
  between `commands.rs` and `watch.rs` (the latter's copy was explicitly commented as mirroring the former);
  consolidated into one definition in `git.rs` so the two cannot drift.
- `watch::run` now takes the branch it is bound to. On every debounce-elapsed cycle and on shutdown, it compares
  the checked-out branch against its own via `git::current_branch` and stands down — skips the commit, and on
  shutdown drops rather than persists observed-but-uncommitted paths — whenever they differ. This is the actual
  fix: the watcher is unaffected by which branch happens to be checked out except to gate whether it acts at
  all. Nothing prevents the branch switch itself (git already permits it); the watcher just stops treating
  "whatever HEAD is now" as authoritative for its own session.
- `init`/`finish`/`stop`/`abort`/`exec`/`build-safe` all resolve their target branch from the checked-out HEAD
  (as they implicitly always did) and scope every ref/state-dir lookup to it. `build-safe` on a detached HEAD
  reports safe unconditionally, since `init` already refuses to start a session without a named branch.
- `status` is the one command that is *not* scoped to the checked-out branch: it now lists every branch with an
  open session (base, pending-commit count against that branch's own tip, watcher liveness, log path), marking
  whichever is currently checked out. Per your follow-up comment on the issue.
- A legacy, unscoped `refs/gitomic/base` (planted by a pre-fix binary) cannot coexist on disk with the new
  per-branch refs (git can't make "base" both a ref and a directory of refs). `init` now detects it up front and
  refuses with an explicit message and recovery steps, rather than letting `update-ref` fail with an opaque
  ref-conflict error; `status` reports it too if present, so it isn't silently forgotten after an upgrade.

## Scope note: "concurrently" per your comment

A single work tree can only have one branch checked out at a time, so true filesystem-level concurrency (two
branches' trees on disk at once) would need `git worktree`, which this does not add. What's implemented instead:
multiple watcher processes — one per branch, each with its own PID — can be alive at the same time, and each
independently tracks whether *its* branch is the one currently checked out. Only the matching one ever commits;
the others idle until you switch back, at which point the original one resumes on its own with no re-init. This
satisfies "separate but equal, unique PIDs" and covers the reported incident; worktree-based parallelism is a
larger, separate piece of work if you want actual simultaneous editing across branches.

## Verification performed

Sandboxed Linux environment, `rustc`/`cargo`/`rustfmt`/`clippy` 1.75 (matching the toolchain your CI verifier
already targets):

- `cargo build --release` — clean, zero warnings.
- `cargo fmt --all --check` — clean (after one `cargo fmt --all` pass over the new code).
- `cargo clippy --release --all-targets -- -W clippy::all` — zero lints.
- `cargo test` — all 14 pre-existing tests still pass unmodified.
- End-to-end scratch-repo run reproducing the reported incident's shape: `init` on `feature-a`, edit, commit
  captured; `checkout main` **without finishing**; edited `main` — the `feature-a` watcher logged the branch
  mismatch and skipped, `main`'s history stayed untouched. `init` on `main` too — both watchers alive
  concurrently with distinct PIDs; `status` listed both sessions independently. Switched back to `feature-a` —
  its watcher resumed unprompted and correctly coalesced the new edit into the existing atomic commit (existing
  same-file coalescing still works per-branch). `finish` while on `main` (no session there) correctly refused
  rather than touching `feature-a`'s session; `finish` after switching back to `feature-a` finalized cleanly.

## Lines changed

- `src/git.rs`: `BASE_REF` constant removed; added `base_ref()`, `LEGACY_BASE_REF`, `session_branches()`,
  `state_dir()`.
- `src/commands.rs`: every command that resolves session state now threads a `branch: &str` through
  (`live_watcher`, `terminate_watcher`, `report_flush`, `wait_for_watcher`); `init` gained a legacy-marker guard;
  `status` rewritten to enumerate all branches; `build_safe` handles detached HEAD explicitly.
- `src/watch.rs`: `run()` takes `branch: &str`; branch-mismatch check added at the debounce-elapsed point and at
  shutdown; `flush_once`/`flush_staged`/`same_paths_as_last_commit` take `base_ref: &str` instead of the removed
  global constant; local `state_dir` mirror removed in favour of `git::state_dir`.
- `src/main.rs`: help text for `init` and `status` updated to describe per-branch scoping.

Pushed to branch `issue-4-branch-scoped-sessions` rather than `main`, for `rust-build-verify.yml` to check
independently before this lands.
