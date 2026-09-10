# gitomic — initial handoff

Scaffolding of `gitomic` v0.1.0 from the README spec. This note records the questions that had to be resolved
before implementation, why each mattered, and the decision taken. It is a handoff for the next session, not a
per-edit receipt; organise or discard as convenient.

## Verification performed before push

Built and exercised in a sandboxed Linux environment with `cargo 1.75` and `git 2.43`:

- `cargo fmt --all --check` — clean.
- `cargo build --release --all-targets` — clean, zero warnings.
- `cargo test` — 12/12 pass (config parser, message cleanup, numbering, short-sha).
- End-to-end run in a scratch repo: init forks the watcher, three debounced edits produced three atomic
  commits, `finish` stamped one message across all three with author and author-date preserved and the work
  tree left clean. Edge paths verified: detached-HEAD refusal, empty session, stop→resume, abort dry-run vs
  `--force`, and empty-message-preserves-session.

`clippy` was not available in the sandbox; the code was written to its standard and the one needless-allocation
it would have flagged was removed. This push is partly a test of whether the Forgejo CI toolchain closes that
gap.

## Clarifications resolved, and the reasoning

### 1. Commits are local only; publishing stays a manual `git push`

Needed because the spec described recording commits but did not state whether gitomic touches a remote. This is
the load-bearing safety question: the finalize step rewrites commit messages, and rewriting history is
dangerous on published commits but harmless on unpublished ones. Chosen local-only, because it keeps the
finalize-rewrite safe by construction and because gitomic was to hold no credentials — leaving all remote
interaction on the `git` side satisfies both.

### 2. The batch keeps N commits with one shared message — it does not squash

Needed because the spec asked for "one message duplicated across all the commits" while also requiring that
"each event should be recoverable." Those are consistent only if the batch remains N separate commits rather
than being collapsed into one. Chosen: keep the commits distinct, all carrying the finalized message; each stays
recoverable by hash and reflog. A `--numbering` option (and `finalize_numbering` config) appends ` [i/N]` when
distinguishing otherwise-identical log lines is wanted; it is off by default.

### 3. No always-on daemon; a manually started, session-scoped watcher that still forks to background

Needed because the spec's always-running daemon would record commits during periods the maintainer did not
intend to track. Chosen: `gitomic init` starts a session and forks a background watcher tied to the current
repository; `gitomic finish` (or `stop`) reaps it. This keeps control over *when* tracking happens while
preserving the ergonomic of not needing to babysit a foreground process. The known cost — forgetting to `init`
before working — is documented, not solved; a convenience for that case is deferred. The multi-directory
`watch_dir` config key from the original daemon idea is accepted and ignored, reserved for that revisit.

### 4. Batch boundary is a marker ref, not "commits ahead of upstream"

Needed because a local-only tool cannot assume an upstream exists or reflects the session. Chosen: `init`
plants `refs/gitomic/base` at HEAD, and the batch is `base..HEAD`. Self-contained, upstream-independent, and
inspectable with plain git.

### 5. Shell out to the `git` binary rather than link a library

Needed to decide how gitomic talks to git. Chosen: shell out, so git configuration, `.gitignore` semantics, and
object storage are inherited unchanged with no reimplementation. There are no user or privilege transitions
here to complicate process spawning, so the simplest mechanism wins.

### 6. Rust

Needed as a halt-point given prior build-toolchain friction. Chosen: Rust, matching the build verifier already
present in the repository — a single binary suited to `~/.local/bin`, no runtime beyond the `git` binary.

## Two behavioural decisions worth knowing

- **Hooks are bypassed.** Placeholder commits use `--no-verify` and the finalize rewrite uses `commit-tree`,
  which runs no hooks. Sane for a watcher firing on every debounce, but a `commit-msg`/`pre-commit` hook will
  not run during a session. Opt-in hook support is a candidate follow-up.
- **A long continuous edit defers the commit.** The debounce window resets on each event, so a burst with no
  pause longer than `debounce_ms` will not commit until the typing stops. This matches "commit when you pause";
  a maximum-defer cap is a candidate follow-up if unbounded deferral proves annoying.

## Deferred / candidate follow-ups

- Always-on multi-repository daemon (and wiring `watch_dir`), with an "attach to work already in progress" path.
- Optional hook execution at finalize.
- Maximum-defer cap on the debounce.
