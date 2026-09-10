# gitomic

`gitomic` records **atomic commits** while a git repository changes, then stamps a **single message** across the
whole batch when the work is done. A session of many small, individually recoverable steps collapses into one
authored intent — without flattening the intermediate history into a single commit.

It is deliberately narrow: it never touches credentials, never contacts a remote, and writes nothing but local
commits. Publishing stays an explicit, separate `git push`.

## Model

A session has two pieces of state, both inspectable with plain git:

- A ref `refs/gitomic/base` marking the commit HEAD sat on when the session began.
- A background watcher process, identified by a pidfile at `<git-dir>/gitomic/watch.pid`.

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

## Usage

Run any command from anywhere inside the repository; the repository is inferred from the working directory.

```
gitomic init                 # mark HEAD as the base and start watching (alias: start)
gitomic status               # base, pending atomic-commit count, watcher state, log path
gitomic finish -m "message"  # stop watching and stamp the message across the batch (alias: commit)
gitomic finish               # same, but open $GIT_EDITOR for the message
gitomic stop                 # stop watching, keep the base and commits for later finish/resume
gitomic abort --force        # discard the session: reset the branch to the base
```

Running `init` again when a base already exists but no watcher is live **resumes** that session rather than
starting a new one, so an accidental `stop` — or a reboot — is recoverable without losing recorded commits.

### finish options

- `-m, --message <text>` — use `<text>` and skip the editor.
- `-n, --numbering` — append ` [i/N]` to each commit message this run.
- `--no-numbering` — do not append ordinals this run (overrides the config default).

## Configuration

`${XDG_CONFIG_HOME:-~/.config}/gitomic/gitomic.cfg`, a flat `key = value` file. Every key has a compiled-in
default, so the file is optional. See `gitomic.cfg.example`.

| key                  | default | meaning                                                                 |
|----------------------|---------|-------------------------------------------------------------------------|
| `debounce_ms`        | `1000`  | Quiescence window; a burst of edits commits once this long has passed.  |
| `finalize_numbering` | `false` | Append ` [i/N]` to each finalized message.                              |
| `include_untracked`  | `true`  | Stage untracked files (`git add -A`) as well as modifications.          |
| `coalesce_same_file` | `true`  | Extend the prior atomic commit instead of starting a new one when a capture's paths exactly match it. |

## Behaviour and ~~guarantees~~ Intentions
(guarantees is a very strong word)

- **Staging respects `.gitignore`** — the watcher uses git's own `add`, and a cycle that stages nothing
  produces no commit.
- **Consecutive captures of the same file(s) share one commit** — when a debounce cycle's staged paths exactly
  match the paths of the session's most recent atomic commit, the capture amends that commit rather than
  starting a new one (`coalesce_same_file`, on by default). Editing a different file, or returning to an
  earlier file after editing something else, still starts a fresh commit. Not applied to `exec`, whose capture
  is a deliberate, explicitly requested result and always stands on its own.
- **The watcher stands down during multi-step operations** — an in-progress merge, rebase, cherry-pick, revert,
  or bisect suspends auto-committing; a held `index.lock` defers to a later cycle.
- **A detached HEAD is refused at `init`** — finalize needs a branch ref to advance.
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
