# Incident: Untracked root-level file loss, suspected gitomic cause

Date: 2026-09-09
Repo: `_dev_BenOS-U2_RC` (BenOS-Custota)
Priority: High. Affects any repo actively watched by gitomic.

## Summary

A large set of untracked files at the root of the working tree were deleted overnight. Subdirectory
contents were unaffected. Root cause is not confirmed, but a strong circumstantial case points to the
`gitomic` file watcher/atomic-commit tool as the trigger.

## Observed damage

- Untracked files at repo root: gone (build artifacts, images, patch dirs, scratch files, `.gitignore`
  itself).
- Untracked files in subdirectories (`sepol/logs/`, `super/lp_images/`, etc.): intact.
- Tracked files: unaffected.
- Deletion pattern is consistent with a non-recursive operation scoped to the working directory root,
  not a recursive `git clean -fd` or an `rm -rf`.

## Ruled out

- `git reset --hard`: does not delete untracked files, confirmed via reflog inspection.
- Commit `137e3cef1a` ("gitignore"): diff against parent shows only 89 insertions to `.gitignore`, no
  file deletions.
- Stale/dangling commit clobbered by reset: `git fsck --unreachable --no-reflogs` returned no commit
  matching the incident window; all dangling commits predate or postdate it by hours to days.
- `git stash` / `git stash drop`: stash list is empty, and `git drop` is not a valid git subcommand
  regardless.
- CI runner: runs in a Docker container with no confirmed bind mount to this working tree. Not fully
  eliminated (mount config not yet inspected) but deprioritized given evidence below.

## Evidence pointing to gitomic

`watch.log` timestamps (converted from UTC to local -0400) bracket the exact moment of the "gitignore"
commit:

```
23:33:30  gitomic starts watching
23:59:48  gitomic starts watching (again)
00:06:35  shutdown signal received
00:08:43  "gitignore" commit lands (89-line .gitignore addition)
00:09:10  gitomic starts watching (again)
00:10:48  shutdown signal received
00:11:18  gitomic starts watching (again)
```

The commit falls directly inside a shutdown/restart cycle of the watcher. Given gitomic is a
session-scoped atomic commit tool with a debounced watcher, still under active development, the working
theory is that the "gitignore" commit was made automatically by gitomic, and that some step in its
commit or cleanup logic operates non-recursively on the working directory root — matching the observed
damage pattern exactly (root-level untracked files gone, subdirectories untouched).

This has not been confirmed against gitomic's actual source. It is a hypothesis, not a diagnosis.

## What needs review in gitomic source

- Any staging step (`git add` equivalent): does it walk the tree recursively, or does it operate only on
  direct children of the working directory root?
- Any pre-commit or post-commit cleanup/reset step: does it call anything resembling a working-directory
  clean, prune, or reset scoped to the top-level directory only?
- Debounce/restart handling: what happens to in-flight state (staged changes, pending file list) when a
  shutdown signal arrives mid-cycle and the watcher restarts seconds later? Two restarts happened within
  roughly 5 minutes of the incident commit; a race between an old debounced batch and a new watcher
  instance is plausible.
- Whether gitomic ever invokes anything equivalent to `rm` directly on files it doesn't recognize as
  tracked or intentionally staged, versus relying exclusively on git plumbing.

## Immediate mitigation

- Do not run gitomic's watcher against a working tree containing untracked files of value until the
  above is reviewed.
- Repo is being restored from rdiff-backup separately; this file does not cover that recovery, only the
  root-cause investigation.

## Next steps

1. Pull gitomic source from `K9/agent-tools` (or wherever the working copy lives) and trace the
   add/commit/cleanup path for any non-recursive filesystem operation.
2. Reproduce in an isolated scratch repo: seed root-level and nested untracked files, run gitomic through
   a start/stop/start cycle similar to the observed timestamps, check for data loss.
3. If confirmed, patch and version-bump gitomic before resuming use on any live working tree.
