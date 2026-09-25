# Receipt — issue #12, part 2: interactive picker for `gitomic drop`

## Context

Comment 273 on issue #12: `drop` works, but choosing among ~14 commits by alternating `git log`,
`git show` and `gitomic drop <hash>` is slow. Requested: a terminal interface with the commits on the
left and the selected commit's diff on the right, vim-style movement, space to mark, Return to submit
behind a y/N confirmation that Return itself cannot answer, and pane switching that does not overshoot
when scrolling reaches an edge.

## Design decisions

- **Entry point.** `gitomic drop` with no hash opens the picker; with hashes, behaviour is unchanged.
  Without a terminal on stdin and stdout it errors instead of waiting. A picker submit ends in the same
  `drop_commits` call as the command line, so watcher handling, `reset --keep`, refusal rules and the
  printed recovery ids are shared, not duplicated.
- **Pre-check before the prompt.** `drop_locked` was split into `plan_drop` (pure: resolves, validates,
  replays in the object database) and the apply step. The picker calls `check_drop`, which runs
  `plan_drop`, when Return is pressed. A dependency conflict is therefore reported inside the picker with
  the marks intact, and the y/n prompt only ever appears for a selection that can be dropped.
- **Library.** `ratatui` 0.30 with its default crossterm backend, over a hand-written terminal layer.
  Resize, Unicode width, wide characters and raw-mode cleanup (including on panic) are the parts most
  likely to be subtly wrong when written by hand. Cost: release binary 0.78 MB -> 1.10 MB, and a larger
  dependency tree. Only current, non-deprecated entry points are used (`try_init`, `restore`,
  `Frame::area`, `Block::bordered`); the build reports no deprecation warnings.
- **List order.** Newest first, as `git log` prints, so "down" means older. The finish template keeps its
  oldest-first order.
- **Key model** (as requested, with the readings below).
  - List: `j`/`k`, `space`, `l`, Return (submit), `q`/Esc. Diff: `j`/`k`, `h`/`l`, Return next,
    Shift-Return previous. All scrolling clamps at its edges; vertical and right scrolling never change
    pane.
  - *Edge behaviour, as interpreted.* "Not switch panes unless the user again hits the key" is read as: an
    `h` that follows another `h` within 200 ms is part of the same run and never leaves the diff, while a
    press after a pause does. At column 0 with no recent `h`, one press returns to the list.
  - *Confirmation.* Only `y`/`Y` confirm and `n`/`N`/Esc cancel. Return, space and everything else are
    ignored. Cancelling keeps the marks.
  - *Beyond the request, small:* `g`/`G`, Ctrl-d/Ctrl-u, arrow keys; `space` also toggles from the diff
    pane (so the loop is view, mark, Return, view, ...); `q` with marks asks before discarding them.
- **Shift-Return.** Most terminals send a plain Return for it. The kitty keyboard protocol flag is pushed
  so that supporting terminals report it, and `N` is provided everywhere as the equivalent. The flag is
  pushed without querying support first: a query blocks until the terminal answers (found during
  testing, where a silent terminal stalled startup), while terminals that do not know the sequence ignore
  it.
- **Diff text.** `git show --stat --patch` with a fixed header, tabs expanded, control characters replaced
  with a visible dot so a file containing escape sequences cannot drive the terminal, diffs cut at 20 000
  lines with a notice. Patch lines are coloured only after the first `diff --git`, so a commit message
  starting with `+` or `-` is left alone.
- **Wording.** The dependency error now ends "Drop it too to proceed." rather than "Add its hash to the
  command", which was wrong inside the picker and reads correctly on the command line too.

## Changes

`Cargo.toml`, `Cargo.lock`: `ratatui = "0.30.2"` added.

`src/pick.rs` (new): `App` state machine, `Source` trait with a git-backed implementation, rendering,
diff styling, terminal setup and restore, event loop, and 20 tests.

`src/commands.rs`: `drop_locked` split into `DropPlan` / `plan_drop` plus the apply step; added
`check_drop` and `drop_choices`; conflict message reworded. Behaviour of `drop <hash>` is unchanged and its
11 tests pass untouched.

`src/main.rs`: `mod pick`, dispatch to the picker when no hash is given, parser no longer rejects an
empty list, usage text.

`README.md`: picker section with a key table.

`.forgejo/workflows/rust-build-verify.yml`: clippy step no longer passes `-- -D warnings`, so lint
warnings are reported without failing the build. Requested in the thread; the pre-existing
`collapsible_match` warning in `proc.rs` was failing that step under current clippy.

## Intended outcome

`gitomic drop`, then `j`/`k` through the list with the diff alongside, `space` to mark what should go,
`Return` and `y` to remove it. A selection that cannot work is explained on screen before anything is
asked, and nothing changes unless `y` is pressed.

## Verification

- `cargo test`: 56 passed (25 original, 11 for `drop`, 20 new for the picker; all earlier tests pass
  unchanged). `cargo fmt --check` clean. `cargo clippy --all-targets`: only the pre-existing warning.
  All added lines are at most 100 columns.
- Picker tests cover: movement and clamping; space in both panes; entering and leaving the diff; vertical
  and right-edge scroll limits without pane change; the held-`h` run stopping at the edge and a later press
  leaving; Return, Shift-Return, `n`, `N` navigation with scroll reset and end stops; Return with nothing
  marked; the prompt ignoring Return, space and other keys; `n`/Esc cancelling with marks kept; a failed
  pre-check keeping the selection editable; quit with and without marks; Ctrl-C; key-release events;
  diff sanitising and colouring; the line cap; rendering of hashes, marks, diff, hints and prompt; tiny
  terminal sizes; and horizontal scroll moving the rendered text.
- Real terminal: the release binary was driven through a pseudo-terminal and its screen read with a
  terminal emulator (`pyte`), 15 checks plus 7 on navigation. Marked a commit from each pane, Return with
  ignored Returns at the prompt, then `y`: exactly the two marked commits were removed, their files gone,
  the rest re-applied, exit 0. A dependent selection was reported in the picker with no prompt; adding
  the dependency produced the prompt; `n` then `q`/`y` left HEAD unchanged. Kitty-encoded Shift-Return
  moved up, Return moved down, both stop at the ends, `N` matches. Without a terminal the command exits 1
  with a clear message. On exit the alternate screen and keyboard flags were restored. With a live watcher
  session, the picker dropped a recorded atomic commit, the watcher restarted, and `status` showed the
  session continuing.
- Bug found and fixed while testing: the first version asked the terminal whether it supported the kitty
  protocol and blocked until answered.

## Not done / follow-ups

- Mouse support, search, and a key to show only the selected commit's stat are not included.
- A restarted watcher still does not inherit `-v/--verbose` (from part 1).
- Not committed to the repository tree; delivered as a zip and patch on the issue thread. The patch
  excludes `Cargo.lock`, which cargo regenerates; the zip contains it.
