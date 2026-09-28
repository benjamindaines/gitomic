# gitomic

`gitomic` records **atomic commits** while a git repository changes, then stamps a **single message** across the
whole batch when the work is done. A session of many small, individually recoverable steps collapses into one
authored intent — without flattening the intermediate history into a single commit.

It is deliberately narrow: it never touches credentials, never contacts a remote, and writes nothing but local
commits. Publishing stays an explicit, separate `git push`.

## Model

A session has two pieces of state, both inspectable with plain git:

- A ref `refs/gitomic/base/<branch>` marking the commit HEAD sat on when the session began.
- A background watcher process, identified by a pidfile under `<git-dir>/gitomic/<branch>/`.

Recording happens on the branch you are standing on. `gitomic init` does not check anything out, and no gitomic
command moves HEAD or the work tree while a session is open.

`gitomic init` plants the base and forks the watcher. As the work tree settles after each burst of edits, the
watcher records one commit with an empty placeholder message — unless the settled paths are exactly the paths
recorded by the session's most recent commit, in which case the capture extends that commit instead (see
`coalesce_same_file` below). `gitomic finish` stops the watcher, collects every commit in `base..HEAD`, obtains
one message, and rebuilds the batch onto the base with that message applied to each commit (original tree and
authorship preserved, fresh committer). The branch ref is advanced with a compare-and-swap and the base marker
is removed.

Because the batch is unpublished, rewriting the messages is safe. The result is N commits carrying the same
message; each remains individually recoverable by hash and reflog. Enable numbering to append ` [i/N]` so the
commits stay distinguishable in `git log`.

A commit that reaches `base..HEAD` without coming from the watcher — a `pull` that advanced the branch, a
manual commit, work from another client — is reported by `gitomic status` and keeps its own message through the
finalize; only the placeholders are restamped. Two cases stop the finalize instead, with the branch untouched
and the session preserved: a commit a remote-tracking branch already contains, since rewriting it would need a
force push to publish, and a merge commit, which a single-parent replay cannot carry. `push_guard` optionally
installs a pre-push hook that refuses to push a branch whose batch is still unfinalized.

## Usage

Run any command from anywhere inside the repository; the repository is inferred from the working directory.

```
gitomic init                 # mark HEAD as the base and start watching (alias: start)
gitomic status               # base, pending atomic-commit count, watcher state, log path
gitomic finish -m "message"  # stop watching and stamp the message across the batch (alias: commit)
gitomic finish               # same, but open $GIT_EDITOR for the message
gitomic stop                 # stop watching, keep the base and commits for later finish/resume
gitomic abort --force        # discard the session: reset the branch to the base
gitomic drop <hash>...       # delete individual unpublished commits (alias: rm)
gitomic cherry-pick [<hash>...]  # bring commits from another branch, via a patch file (alias: pick)
```

Running `init` again when a base already exists but no watcher is live **resumes** that session rather than
starting a new one, so an accidental `stop` — or a reboot — is recoverable without losing recorded commits.

### finish options

- `-m, --message <text>` — use `<text>` and skip the editor.
- `-n, --numbering` — append ` [i/N]` to each commit message this run.
- `--no-numbering` — do not append ordinals this run (overrides the config default).

### drop

`gitomic drop <hash>...` deletes individual commits that have not been pushed, using the short hashes printed
by the `finish` editor template, `gitomic diff`, or `git log`.

- **Eligible commits** are the open session's pending batch (`base..HEAD`) or, when no session is open, any
  commit on the checked-out branch that no remote-tracking branch contains. A commit that is already in a
  remote-tracking branch, a root commit, and a merge commit are refused.
- **History** is rewritten by re-applying the later commits onto the surviving parent, keeping each one's
  message and authorship. The work happens in the object database, so nothing on disk changes until every
  replay has succeeded.
- **Dependencies** are detected, not guessed at: a later commit that does not apply without a dropped change
  (for instance, one that edits a file the dropped commit created) aborts the whole operation and is named in
  the error. Pass its hash as well to drop it together with the commit it depends on.
- **Work tree**: files the dropped commits added are removed and files they changed are restored, as
  `git reset --keep` does. Uncommitted edits to any other file are kept; an uncommitted edit to a file that
  must be rewritten aborts the operation with nothing changed.
- **Session**: a live watcher is stopped, its final capture flushed, and the watcher restarted afterwards, so
  the session carries on. The base marker is untouched.
- **Recovery**: the full id of every dropped commit is printed. The objects remain until garbage collection,
  so `git cherry-pick <full id>` restores one.
- `-n, --dry-run` reports what would be dropped and whether every later commit still applies, and modifies
  nothing.

Requires git 2.38 or newer (`git merge-tree --write-tree`).

#### Interactive picker

`gitomic drop` with no hash opens a two-pane picker over the same eligible commits: the list on the left
(newest first, as in `git log`), the highlighted commit's header, stat and patch on the right. Marked
commits are shown in red with `[x]`.

| pane | key | action |
|------|-----|--------|
| list | `j` `k` (or arrows) | move; `g` / `G` jump to first / last |
| list | `space` | mark or unmark the commit |
| list | `l` (or Right) | move to the diff pane |
| list | `Enter` | submit the marked commits; a `y`/`n` prompt follows |
| list | `q`, `Esc` | quit (asks first when commits are marked) |
| diff | `j` `k` | scroll up and down, stopping at the ends |
| diff | `h` `l` | scroll left and right, stopping at the edges |
| diff | `Enter`, `n` | next commit down the list |
| diff | `Shift-Enter`, `N` | next commit up the list |
| diff | `space` | mark or unmark the commit being viewed |
| diff | `h` at the left edge, `Esc` | back to the list pane |
| diff | `g` `G`, `Ctrl-d` `Ctrl-u` | top / bottom, half a page |

- Pressing `Enter` on the list first runs the same dependency check as `drop`. A selection that cannot be
  dropped (a later commit depends on a marked one) is reported on the bottom line and stays editable; the
  `y`/`n` prompt appears only for a selection that can be dropped.
- Only `y` and `n` answer the prompt. `Enter`, `space` and every other key are ignored, so an accidental
  Return cannot confirm anything. `n` and `Esc` cancel and keep the marks.
- Leaving the diff pane with `h` requires a press that is not part of a run of `h` presses that was still
  scrolling, so holding `h` to reach the left edge stops there rather than jumping to the list.
- `Shift-Enter` is delivered only by terminals that support the kitty keyboard protocol (kitty, foot,
  WezTerm, Ghostty and others); elsewhere it arrives as a plain `Enter`, and `N` does the same job.
- `-n/--dry-run` applies to the picker as well: the confirmation is shown, then nothing is modified.
- Needs a terminal on stdin and stdout; otherwise the command stops with an error rather than waiting.

### cherry-pick

`gitomic cherry-pick` brings commits from another branch onto the checked-out one. The chosen commits are
replayed onto the current `HEAD` in the object database, the result is written as a patch file, and the patch
is applied to the work tree. History is not rewritten, so the operation is safe with unpushed commits and with
a live session.

- **Without a session** the change is left in the work tree, uncommitted (the effect of `git cherry-pick -n`).
- **With a session** (live or stopped) it is recorded as one atomic commit with the usual empty placeholder
  message, which `finish` stamps like any other step. Only the patch's own paths are committed; other staged
  changes stay out. A live watcher is stopped for the duration, as for `drop`, and restarted afterwards.
- **`HEAD` moved after the selection** (the watcher committed, or a commit was made by hand): the patch is
  recomputed against the new `HEAD` before it is applied, reusing every conflict decision whose conflict
  recurs unchanged. A conflict that was not decided stops the operation before anything is modified.
- **Local edits**: the patch is checked against the work tree first. An uncommitted edit that overlaps a file
  the patch touches refuses the pick with nothing modified.
- **Patch files** are kept in `.git/gitomic-picks/`. Each starts with a `# gitomic-patch 1` header naming its
  base, its source commits and the conflict decisions taken; `git apply` ignores it, and it is what allows the
  patch to be recomputed against another base. A pick is undone with `git apply -R <patch file>` (or
  `gitomic drop` once recorded).
- Merge commits are not offered, and commits whose change `HEAD` already holds under another id are hidden.
- Options: `--from <branch>` lists that branch first; `-n/--dry-run` reports the patch and applies nothing;
  `-p/--patch-only` writes the patch file and stops. With commits named on the command line (replayed in the
  order given) no terminal is needed, and any conflict is refused.

#### Interactive screen

`gitomic cherry-pick` with no commit opens the same two-pane screen as the `drop` picker, with the same
vim-style keys, `space` to mark and the same rule that only `y`/`n` answer a prompt. What differs:

- `Tab` opens an overlay listing the other local and remote-tracking branches, most recently updated first.
  `j`/`k` move, `Enter` uses the branch (marks are cleared), `Esc` closes it. Without `--from` the overlay is
  the first thing shown.
- The right pane shows what each commit would change relative to the `HEAD` that was current when the screen
  opened, not the commit's own diff. A commit that would conflict is announced above its diff.
- A commit that would change nothing on `HEAD` (its change is already there in another form) is drawn in gray
  and skipped by `j`/`k`, `g`/`G` and, in the diff pane, `n`/`N`. Whether a commit applies is worked out for
  the commits near the selection while the screen is idle, and on demand when movement reaches one; a commit
  that conflicts counts as applying. When no later commit applies, the selection stays put and the status
  line says so.
- The diff of a commit loads once the selection has rested on it for 150 ms, so holding `j` or `k` does not
  run a merge for every commit passed. A diff already loaded is shown at once.
- The screen opens, and a newly chosen branch is shown, with the selection marker outside the list: no commit
  is highlighted and no diff is loaded. `j` (or `Down`) enters from the top and lands on the first commit that
  applies; `k` (or `Up`) enters from the bottom. `g` and `G` also leave this state. While it lasts, the two ends
  of the list are classified in the background, and `space`, `R` and `l` do nothing.
- A commit that changes a file larger than `cherry_size_limit_mb` (32 MiB by default) is not merged onto
  `HEAD` and its files are not read: the preview lists the large files with their sizes and shows the commit
  as it was made, and the commit is never grayed, since that would need the merge. `D` offers to read the
  files and show the change relative to `HEAD`; only `y` confirms. Merging reads a large file on both sides,
  which is what makes the screen unusable on slow hardware when successive commits modify the same image.
  The text of one preview is also capped at 8 MiB, after which git is stopped and the cut is announced.
- The git work runs on two background threads, so keys and drawing never wait for it. One loads the diff of
  the selected commit; when the selection moves on, its git process is killed and only the newest request is
  answered. The other works out which commits to gray, nearest the selection first, at a lower processor
  priority; it does not start a check while a diff is being loaded, and abandons one in progress when a diff
  is requested, asking again afterwards.
- `R` follows one file. It marks the highlighted commit and every older commit of the source branch that
  changes the same file (the commits `HEAD` lacks), each restricted to that file, so that replaying them
  brings the file to its state on the source branch at the highlighted commit. When the highlighted commit
  changes several files, a small overlay asks which one to follow. A commit in the chain that also changes
  other files (one made with `git add` rather than by gitomic) is applied for the followed file only; its
  other files are left out, and the status line counts such commits. Restricted marks show as `[f]` (plain
  space marks stay `[x]`) and the diff title names the file; the preview shows the restricted form.
  Commits above the cursor keep their marks, and pressing `R` again on a fully marked chain clears it. Renames
  are not followed (a file is tracked under the name it has in each commit), and merge commits are not in the
  list, so changes that reached the branch only through a merge are not part of a chain.
- On the command line the same restriction is `--only <path>`: every named commit is applied for that file
  only, and a commit that does not change it is refused.
- `Enter` on the list replays the marked commits, oldest first. A clean result goes straight to the `y`/`n`
  prompt, which names the number of commits and the size of the change.
- **Conflicts** open a decision screen: the conflicted files on the left, one entry per conflict hunk, and the
  hunk on the right with a few lines of context and both sides one above the other. `a` keeps the tree copy,
  `b` takes the picked commit's version, `c` keeps both (tree copy first), `u` undoes, `j`/`k` move between
  conflicts and `Enter` continues once every one is decided. A file that cannot be split into hunks (binary
  content, or one side deleted the file) is decided as a whole with `a`/`b`. A later commit in the selection
  that conflicts opens the screen again. When the selection was made with `R`, `X` abandons the replay of
  that file's history and restores the file whole from the newest commit marked for it (its content on the
  source branch, or absent if the commit lacks it); the confirmation prompt names the restored files.
- Conflicts with no A/B meaning (a file renamed differently on both sides, a file/directory clash) are
  reported and the selection is refused; `git cherry-pick` handles those.

## Configuration

`${XDG_CONFIG_HOME:-~/.config}/gitomic/gitomic.cfg`, a flat `key = value` file. Every key has a compiled-in
default, so the file is optional. See `gitomic.cfg.example`.

| key                  | default | meaning                                                                 |
|----------------------|---------|-------------------------------------------------------------------------|
| `debounce_ms`        | `1000`  | Quiescence window; a burst of edits commits once this long has passed.  |
| `finalize_numbering` | `false` | Append ` [i/N]` to each finalized message.                              |
| `stage`              | `observed` | Staging breadth: `observed` (only the paths the watcher saw change, new/renamed/copy-over included), `tracked` (`git add -u`), or `all` (`git add -A`). |
| `coalesce_same_file` | `true`  | Extend the prior atomic commit instead of starting a new one when a capture's paths exactly match it. |
| `cherry_size_limit_mb` | `32`  | Size in MiB above which `cherry-pick`'s screen does not read a changed file for its preview (see below). `0` turns the limit off. |

## Behaviour and ~~guarantees~~ Intentions
(guarantees is a very strong word)

- **Staging respects `.gitignore`** — the watcher uses git's own `add`, and a cycle that stages nothing
  produces no commit.
- **Staging is scoped to what the watcher saw change** — under the default `stage = observed`, a capture stages
  only the paths that fired events this cycle, intersected with the paths git reports as changed. A file
  created by a patch, a file-manager rename (delete of the old name plus creation of the new), or a copy that
  overwrites a tracked file is captured, because the watcher observed it; an untracked file the watcher never
  touched is left alone rather than swept into the session. `stage = tracked` restricts staging to already-
  tracked paths (`git add -u`), and `stage = all` stages every change including untracked files (`git add -A`).
- **Consecutive captures of the same file(s) share one commit** — when a debounce cycle's staged paths exactly
  match the paths of the session's most recent atomic commit, the capture amends that commit rather than
  starting a new one (`coalesce_same_file`, on by default). Editing a different file, or returning to an
  earlier file after editing something else, still starts a fresh commit. Not applied to `exec`, whose capture
  is a deliberate, explicitly requested result and always stands on its own.
- **The watcher stands down during multi-step operations** — an in-progress merge, rebase, cherry-pick, revert,
  or bisect suspends auto-committing; a held `index.lock` defers to a later cycle.
- **A detached HEAD is refused at `init`** — finalize needs a branch ref to advance.
- **One session per branch, and the watcher follows its own branch** — a watcher records only while its branch
  is the checked-out one, and resumes by itself when you switch back, with no re-init. Switching to a different
  branch suspends capture for that session rather than committing onto whatever is current.
- **Events inside the git directory are ignored** — committing writes to `.git`, so those writes are filtered
  to prevent a feedback loop.
- **Hooks are bypassed** — placeholder commits use `--no-verify`, and the finalize rewrite uses `commit-tree`,
  which does not run hooks. Run hooks manually if a session depends on them.

## Scope, and what is intentionally left out

The original design contemplated an always-running daemon watching several configured directories. That is out
of scope here in favour of a session-scoped, manually started watcher tied to the current repository, which
avoids recording commits during periods the maintainer did not intend to track. The `watch_dir` config key is
accepted and ignored, reserved for a future multi-repository daemon should one prove useful. The known cost of
the manual model — forgetting to `init` before working — is documented rather than solved.

## Building

Requires a Rust toolchain and the `git` binary at run time. Linux only (inotify).

```
cargo build --release
install -Dm755 target/release/gitomic ~/.local/bin/gitomic
```
