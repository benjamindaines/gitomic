# Receipt — issue #12: delete individual commits before they are pushed

## Context

The maintainer needs to remove a single unpublished commit, identified by the short hash shown on the
`finish` compose screen. The alternatives in plain git (cherry-pick, stash, `git rm`) either rewrite the
wrong thing or require pushing the commit that is to be removed.

## Design decisions

- **New command `gitomic drop <hash>...`** (alias `rm`), with `-n/--dry-run`. Hashes may be abbreviated.
- **Eligible commits.** With a session open: `base..HEAD`, so the base marker stays an ancestor of the tip.
  Without one: `HEAD --not --remotes`, i.e. commits no remote-tracking ref contains. This is the definition
  of "before they're pushed"; a published commit is refused. Root and merge commits are refused because
  replay needs exactly one parent.
- **Replay in the object database.** Commits after the first dropped one are re-applied onto the surviving
  parent with `git merge-tree --write-tree --merge-base=<c>^ <new-parent> <c>`; message and author are copied,
  committer is ambient (the same convention `finish` already uses). No index or work-tree access occurs
  during replay, so a conflict aborts with nothing modified.
- **Dependency handling.** A conflict (exit 1) aborts and names the commit that does not apply. Dropping it
  along with its dependency is a matter of listing both hashes. No automatic cascade: silently deleting
  commits the maintainer did not name was judged worse than asking.
- **Work-tree update** via `git reset --keep <new-tip>`. Files belonging to dropped commits are removed or
  restored on disk; uncommitted edits to other files survive; an uncommitted edit on a file that must change
  makes git refuse before touching anything. This is the property that avoids the "hard reset away all local
  changes" failure the issue describes. `reset --hard` is not used anywhere.
- **Watcher.** A live watcher is stopped and its final capture flushed (same helpers `stop`/`finish` use),
  then restarted through `init` after the rewrite, so the session continues and the reverted files are not
  re-captured as new changes. Bad hashes are rejected before the watcher is touched.
- **Recovery.** Full ids of dropped commits are printed; objects persist until gc, so
  `git cherry-pick <full id>` restores one.

## Changes

`src/git.rs` (additions only)
- `commits_with_parents`: `rev-list --reverse --parents` over either an explicit range or
  `HEAD --not --remotes`.
- `commit_message`: full message of a commit (empty placeholder messages round-trip).
- `Pick` and `cherry_pick_tree`: cherry-pick a commit onto a parent inside the object database.
- `reset_keep`: `git reset --keep` with a `GIT_REFLOG_ACTION` label.

`src/commands.rs`
- `drop_commits` (entry point), `drop_candidates`, `resolve_drop_targets`, `drop_locked`, and the `Chain` /
  `Candidates` type aliases.
- `template`: three comment lines telling the operator how to drop a commit from the compose screen's list
  (leave the message empty to abort finalize, run `gitomic drop <hash>`, run finish again).
- `drop_tests` module: 11 tests against throwaway repositories.

`src/main.rs`
- `drop`/`rm` dispatch arm, `DropOpts` parser, `print_usage` entry.

`README.md`
- `drop` added to the usage list and a `### drop` section.

No existing line was modified or removed in `src/`; all changes are additive.

## Intended outcome

`gitomic drop 4d90900` removes that one commit from the branch, re-applies the commits that followed it,
deletes or restores the files it touched, leaves unrelated uncommitted work alone, and keeps the session
running. If anything cannot be done safely, the command says why and changes nothing.

## Verification

- `cargo test`: 36 passed (25 pre-existing, 11 new). New tests cover: dropping a middle commit and its file;
  authorship and empty-message preservation on replayed commits; refusal when a later commit depends on the
  dropped one (HEAD and file contents unchanged); dropping dependent commits together; refusal of a commit in
  a remote-tracking branch; refusal of a root commit; uncommitted edits to an unrelated file surviving;
  refusal when an uncommitted edit blocks the work-tree update (nothing modified); dry-run leaving HEAD and
  files alone; an unknown hash; session scope excluding pre-base commits while leaving the base ref intact.
- End-to-end with the release binary and a live watcher: init a session, create three files as three atomic
  commits, `drop -n` on the middle one (reported, nothing changed), `drop` (watcher stopped, commit and its
  file removed, later commit re-applied, watcher restarted with pid changed, `status` showing 2 pending),
  `drop zzzz` (rejected, exit 1). No stray watcher afterwards.
- `cargo fmt --check` clean. `cargo clippy --all-targets`: no warnings from this change; the one
  pre-existing `clippy::collapsible_match` in `proc.rs` was left as found.
- All added lines are at most 100 columns, the rustfmt default `max_width`.

## Not done / follow-ups

- No option to unrecord a commit while keeping its files as uncommitted edits. Under a live watcher those
  edits would be re-captured, so this needs its own design.
- A restarted watcher does not inherit `-v/--verbose` from the original `init`.
- Not committed to the repository tree; delivered as a patch and zip on the issue thread.
