# Receipt — issue #8: editor swap files fragmenting sessions

## Root cause (confirmed against the linked compare, bef2ed5..c62d38 in BenOS-tools)

Kate creates `.install.sh.kate-swp` the instant its buffer is dirtied and removes it the instant `:w` writes
`install.sh` itself. Those land in two different debounce cycles with two different staged-path sets — `{swp
added}`, then `{swp removed, install.sh modified}` — which never repeat consecutively. `same_paths_as_last_commit`
(`src/watch.rs`) requires an exact match to fold a capture into the prior commit, so the sets never coalesce and
every cycle becomes its own commit: 25 of the 29 commits in the referenced range are swap-file noise around a
single continuous edit of one file.

## Fix

Per the agreed approach in the issue #8 discussion (comment 124): a configurable ignore-pattern list, applied at
the watcher's event source rather than at staging time.

- `config.rs`: new `Config::ignore_patterns: Vec<String>`, seeded from a new `DEFAULT_IGNORE_PATTERNS` constant
  covering vim (`*.swp`/`*.swo`/`*.swx`/`*.un~`), Kate (`*.kate-swp` — matches with or without a leading dot,
  since the matcher's `*` is not dot-excluding), Emacs (`#*#` auto-save, `.#*` lock symlink), and the generic
  `*~` backup suffix several editors share. A new `ignore_patterns = a, b, c` config key extends this list
  (never replaces it), so a project-specific addition doesn't require re-listing the defaults.
  - `config::glob_match(pattern, name)`: a small hand-rolled matcher supporting only the `*` wildcard, which is
    all this vocabulary needs (prefix/suffix shapes, no character classes) — kept the dependency budget at
    `libc` + `notify`, no glob crate added.
- `watch.rs`: the notify event closure now filters on `is_ignored_path` (basename against `ignore_patterns`)
  alongside the existing `is_within` git-dir filter, both applied before a path can enter `external`/arm the
  debounce timer. This is the actual fix — filtering at the event source rather than in `stage_observed` means
  the swap file's churn never counts as activity at all, so debounce cycles form around the real file's actual
  edits and `coalesce_same_file` sees the same path set on every one of them.
  - `trace_event` (verbose mode) now tags each path individually as `armed`, `ignored (git-internal)`, or
    `ignored (editor swap/lock/backup pattern)`, replacing the old boolean armed/git-internal split, so verbose
    tracing still distinguishes all three outcomes rather than collapsing the new case into "armed".

Only `StageMode::Observed` consults this list (it's the only mode that reads the observed-path set at all);
`Tracked`/`All` stage via `git add -u`/`-A` directly and are unaffected, as before.

## Verification performed

Sandboxed Linux environment, `rustc`/`cargo`/`rustfmt`/`clippy` 1.75 via `apt` (matching the toolchain
`rust-build-verify.yml` targets):

- `cargo build --release --all-targets` — clean, zero warnings.
- `cargo fmt --all --check` — clean (after one `cargo fmt --all` pass over the new code).
- `cargo clippy --all-targets --all-features -- -D warnings` — zero lints.
- `cargo test --all-features` — 24 tests pass (13 pre-existing plus 11 new: glob-match edge cases, ignore-list
  parsing/extension/blank-entry handling in `config.rs`; basename-vs-full-path and git-dir-prefix behaviour in
  `watch.rs`).
- End-to-end scratch-repo reproduction of the reported incident's shape: `gitomic init`, then four
  dirty/save cycles each touching `.install.sh.kate-swp` before the real write to `install.sh`, each pair
  separated by more than the debounce window. Result: one atomic commit covering all four edits, no
  `kate-swp` path anywhere in the resulting history (`git log --all --stat`). A second run with `init -v`
  confirmed the swap file's create/close/remove events are tagged `ignored (editor swap/lock/backup pattern)`
  in the watch log while the real file's writes are tagged `armed`.

## Lines changed

- `src/config.rs`: added `DEFAULT_IGNORE_PATTERNS`, `Config::ignore_patterns`, the `ignore_patterns` config key
  in `parse()`, and `glob_match()`; six new tests.
- `src/watch.rs`: event closure now filters `is_ignored_path` alongside `is_within`; added `is_ignored_path()`;
  rewrote `trace_event()` to tag each path individually across three outcomes instead of one boolean; new
  `#[cfg(test)]` module (watch.rs had none previously) with four tests.
- `gitomic.cfg.example`: documented the new `ignore_patterns` key and its default list.

Lines wrap at 120 characters (this project's `rustfmt` default), matching the rest of the codebase.

Pushed to branch `issue-8-editor-swap-ignore` (branched off `main` after merging PR #7 per the maintainer's
go-ahead in the issue #8 thread), for `rust-build-verify.yml` to check independently before it lands.
