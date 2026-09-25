# Receipt — issue #13: `gitomic cherry-pick` with the picker UI and in-screen conflict resolution

## Context

Issue #13 asks for the `drop` picker's interface to exist for cherry-picking: same layout and vim keys, Tab
to choose the source branch, multi-select, a diff pane computed against the HEAD current when the screen
opened, application through a patch file, a patch that can be recomputed when HEAD has moved, and conflict
resolution that opens at the conflict and is settled with A or B per hunk. The issue also asks whether a pick
can work cleanly inside a live session, and to flag (and skip) anything needing a stronger model. Built on the
`exitScopeFullSpeed` branch tip; not merged and not committed to the tree.

## Answers to the questions in the issue

- **Live session pick: clean, so implemented.** The operation never rewrites history. The patch is computed
  against HEAD (unpushed commits included) and applied to the work tree; HEAD only moves forward through
  gitomic's own capture. Uncommitted work and unpushed commits therefore need no special handling, and
  refusing until everything is committed and pushed is unnecessary. The one real hazard, HEAD moving while
  the screen is open, is handled by recomputing the patch (below).
- **Model recommendation:** nothing requested was skipped for capability reasons. Items that were left out,
  with a recommendation for each, are under "Not done".
- **Diffuse / external source:** not needed. Per-hunk A/B/both is a small module of its own
  (`src/conflict.rs`); nothing was pulled in and `Cargo.toml` and `Cargo.lock` are unchanged.

## Design decisions

- **Pipeline.** Chosen commits are replayed oldest first onto HEAD with
  `git merge-tree --write-tree` (merge base `<commit>^`), entirely in the object database; the patch is
  `git diff --binary --full-index --no-renames <HEAD> <result>`; it is written to `.git/gitomic-picks/` and
  applied with `git apply` (work tree only, index untouched) after a `git apply --check`. Merge inputs must be
  commits, so each intermediate result is wrapped in an unreferenced scratch commit (collected at gc).
- **Outcome by situation.** No session on the branch: change left in the work tree, uncommitted (as
  `git cherry-pick -n`). Session present (live or stopped): recorded as one atomic commit with the empty
  placeholder message, restricted to the patch's own paths (`commit --only`), so other staged changes stay out
  and `finish` stamps it like any other step. A live watcher is stopped, its final capture flushed, and then
  restarted, using the same helpers as `drop`.
- **One patch, one atomic commit** for a multi-commit selection. Per-commit granularity inside a session would
  need one apply/commit per pick; the intermediate trees already exist, so it is a contained follow-up.
- **Diff pane reading.** "Diff between the current HEAD established when the UI opened" is implemented as:
  what applying the highlighted commit to that HEAD would change (`git diff <HEAD> <merge result>`), not the
  commit's own diff. A conflicting commit is announced above its diff; the conflicted files show their markers.
  Each preview is for the commit alone. A commit that depends on an earlier one in the selection therefore
  previews as conflicting yet replays cleanly in sequence.
- **Rebuild on HEAD movement.** After the watcher is quiet, HEAD is compared with the base the selection was
  made against. If they differ the patch is rebuilt on the new HEAD (`patch::rebuild`). Each conflict decision
  is keyed by a fingerprint of (path, tree side, picked side); a conflict that recurs identically reuses its
  decision, a changed one is undecided, and an undecided conflict stops the operation before anything is
  modified (nothing written, nothing applied).
- **Patch header.** Each patch file begins with `# gitomic-patch 1`, `# base`, `# pick` and `# decision`
  lines. `git apply` skips text ahead of the first `diff --git` line (checked by test). `Spec::parse` reads the
  header back, so re-basing an existing patch file is a thin command over existing functions.
- **Patch code separated.** `src/patch.rs` has no terminal or session dependency: `Job` (replay with pause at a
  conflict), `Spec` (recipe and header), `build`, `rebuild`, `write`, `check`, `apply`. This is the piece to
  promote to a stand-alone patch re-write command.
- **Conflict decisions.** A text conflict is split into hunks (`conflict::parse`) and decided per hunk:
  `a` tree copy, `b` picked commit, `c` both (tree copy first), `u` undo. The resolved file is written into a
  throwaway index (`GIT_INDEX_FILE`), never the repository's own; the result tree is `write-tree`. Files that
  cannot be split (binary, unparseable markers, a modify/delete) are decided whole with `a`/`b`.
  `merge.conflictStyle` is pinned to `merge` for these calls so a user's `diff3`/`zdiff3` setting cannot change
  the marker layout; a diff3 base section is still tolerated by the parser.
- **Unsupported conflicts refused, not guessed.** Anything other than contents, binary, modify/delete and
  add/add (renames on both sides, file/directory clashes, distinct types) is reported with git's message and the
  selection is refused before anything is modified.
- **Screen.** A new module, `src/cherry_ui.rs`, reuses `style_diff`, `DiffView`, `H_STEP`, `RUN_WINDOW` and a
  terminal-setup helper from `pick.rs` (visibility changes plus one extraction), so both screens share diff
  styling, the held-`h` rule, kitty-protocol handling and raw-mode cleanup. Keys match the drop picker. Added:
  `Tab` (branch overlay, from either pane), the decision screen, and `Enter` meaning "prepare" rather than
  "drop". Switching branch clears the marks, since they belong to the previous list. Only `y`/`n` answer a
  prompt, as before. After a decision the cursor moves to the next undecided hunk.
- **Candidates.** `git log --cherry-pick --right-only --no-merges HEAD...<branch>`: commits HEAD lacks, newest
  first, merges excluded (replay needs a chosen parent), changes already present under another id hidden. The
  list is capped at 500 and the title says so.
- **Command line.** `gitomic cherry-pick [--from <branch>] [-n] [-p] [<hash>...]` (alias `pick`). With hashes no
  terminal is needed, commits replay in the order given, contained/merge/root commits are refused, and any
  conflict is refused with the files named.
- **Bug found by testing.** For a binary conflict git reports the type `CONFLICT (binary)` alongside
  `CONFLICT (contents)`; the first version treated it as unsupported. It is now accepted and decided whole.

## Changes

New files:

- `src/conflict.rs` (333 lines): marker parser and renderer, `Side`, `Hunk`, `Segment`, fingerprint. 11 tests.
- `src/patch.rs` (999 lines): merge, conflict extraction from `merge-tree -z` output, resolved-tree
  construction, `Job`, `Spec`/header, `build`/`rebuild`, patch file write/check/apply. 12 tests.
- `src/cherry.rs` (633 lines): branches, candidates, preview, command entry, session/watcher handling, apply.
  15 tests against real repositories.
- `src/cherry_ui.rs` (1589 lines): `Source` trait and git-backed source, `App` state machine, overlay, decision
  screen, rendering, event loop. 25 tests (fake source, `TestBackend`).
- `src/testrepo.rs` (81 lines, test builds only): throwaway repository helper.

Modified files (line numbers are in the delivered versions):

- `src/main.rs`: `mod cherry; mod cherry_ui; mod conflict; mod patch;` and `#[cfg(test)] mod testrepo;` (lines
  14-24); dispatch arm `"cherry-pick" | "pick"` (line 107); `parse_cherry` (lines 178-208); usage entry
  (lines 306-317).
- `src/git.rs`: appended at the end (lines 397-488): `cat_blob`, `hash_object_write`, `commit_only`. Nothing
  existing changed.
- `src/pick.rs`: `H_STEP`, `RUN_WINDOW`, `DiffView` (and its two fields) and `style_diff` made `pub(crate)`
  (lines 44, 47, 124-126, 516); `with_terminal` extracted (lines 566-584) and `run` now calls it
  (line 603) in place of the twelve inline lines it replaces (584-595 originally). Behaviour of `drop` unchanged.
- `src/commands.rs`: `live_watcher`, `terminate_watcher`, `report_flush`, `short` made `pub(crate)` (lines 24,
  801, 842, 889). No logic changed.
- `README.md`: `cherry-pick` in the usage list and a `### cherry-pick` section with the key description.

## Intended outcome

`gitomic cherry-pick`, Tab to a branch, `j`/`k` and `space` to mark commits, `Enter`; conflicts are settled a
hunk at a time with `a`/`b`; `y` applies the patch. In a session the result is one recorded step; outside one it
is an uncommitted change with a patch file to undo it. Nothing is modified unless `y` is pressed, and any
refusal (local edits in the way, unsupported conflict, undecided conflict after HEAD moved) leaves the tree as
it was.

## Verification

- `cargo test`: 119 passed (56 existing, all unchanged and passing; 63 new). `cargo fmt --check` clean. All
  added lines are at most 100 columns. `cargo clippy --all-targets`: only lints of the kind already tolerated;
  no deprecated API used.
- New tests cover: parser and renderer (diff3 base, CRLF, empty side, longer markers, malformed input);
  per-hunk decisions, keeping both, undecided hunk blocking, modify/delete both ways, binary conflict,
  rename/rename reported unsupported, sequential picks, decision reuse on rebuild and its non-reuse when the
  conflict changed, header round trip with `git apply`, and that building leaves the index, work tree and
  `.git` untouched; uncommitted result without a session, several commits in order, refusal on conflict with
  nothing changed, dry-run and patch-only, one atomic commit in a session with an unrelated staged file
  excluded, local edit blocking, contained/merge/root/unknown refusals, in-progress cherry-pick refusal,
  already-present change, HEAD moving after selection, candidate filtering and order, branch list, preview;
  the screen: overlay, source switching, marking order, prompts ignoring Enter/space, cancel keeping marks,
  decision navigation/undo/both, whole-file "no both", chained conflicts, quit rules, rendering of every
  screen and tiny terminals.
- Real terminal (release binary in a pseudo-terminal, screen read with `pyte`), live session with a running
  watcher, two hunks conflicting on the same lines, plus a clean commit: overlay opens first without `--from`;
  Enter loads the branch; the preview flags the conflict; Tab reopens the overlay and Esc keeps the marks;
  Enter is refused while undecided; `b` then `a` decided the hunks; Enter, space and `j` were ignored at the
  prompt; `y` produced exactly one new commit with an empty message touching `b.txt` and `f.txt`, `f.txt`
  holding the chosen sides, the watcher stopped and restarted (pid changed, `status` showing 1 pending), no
  stray changes in `git status`. Second run: the watcher committed an unrelated file while the confirmation
  was open; after `y` the "HEAD moved ... recomputing" notice was printed, the pick applied and was recorded,
  both commits present, watcher alive.

## Not done / follow-ups

- **Editing inside a hunk** (neither side, or a mix). Cheapest route is handing the single file to `$EDITOR`
  from the decision screen: Sonnet, medium. An in-screen text editor is a different size of job: Opus, high.
- **Renames on both sides and file/directory clashes** are refused, not resolved. Presenting them needs a
  model of paths that move between sides: Opus, high.
- **Stand-alone patch re-write command** (`gitomic patch rebase <file>`). The pieces exist (`Spec::parse`,
  `patch::rebuild`, `write`, `check`, `apply`); the command, its interaction with a live session and the
  question of editing a patch file by hand are left for a session of their own: Sonnet, medium.
- **Per-commit granularity in a session** (one atomic commit per picked commit): Sonnet, low.
- The preview is per commit, so an early conflict that a chosen earlier commit would resolve is still flagged
  there; the replay is authoritative.
- A restarted watcher still does not inherit `-v/--verbose` (from part 1 of #12).
- Delivered as a zip and patch on the issue thread; the patch excludes nothing (`Cargo.toml` and `Cargo.lock`
  are unchanged).
