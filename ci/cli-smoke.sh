#!/usr/bin/env bash
# End-to-end check of the command line through the built binary. The unit tests call the library
# functions directly and so never pass through the argument parser in main.rs; this does, for the
# commands that read the repository without a terminal: `history`, and `pick --stash` /
# `restore --stash`, which take single files out of a stash and must leave the stash as it was.
#
# Usage: ci/cli-smoke.sh [path to the gitomic binary]   (default: target/release/gitomic)

set -euo pipefail

BIN="$(realpath "${1:-target/release/gitomic}")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Keep the user's own configuration out of the run.
export XDG_CONFIG_HOME="$WORK/xdg"
export HOME="$WORK/home"
mkdir -p "$XDG_CONFIG_HOME" "$HOME"

fail() {
    echo "cli-smoke: FAIL: $*" >&2
    exit 1
}

expect_eq() { # <what> <expected> <actual>
    [ "$2" = "$3" ] || fail "$1: expected '$2', got '$3'"
}

expect_fail() { # <what> <command...>
    local what="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        fail "$what: expected a non-zero exit"
    fi
}

cd "$WORK"
git init -q -b main repo
cd repo
git config user.name ci
git config user.email ci@example.invalid

# --- history -----------------------------------------------------------------------------------
mkdir sub
echo v1 >sub/f.txt
echo a >a.txt
echo b >b.txt
git add .
git commit -q -m "first"
echo v2 >sub/f.txt
git commit -q -am "second"
echo x >other.txt
git add other.txt
git commit -q -m "unrelated"
echo v3 >sub/f.txt
git commit -q -am "third"

listing="$("$BIN" history sub/f.txt)"
expect_eq "history lists the commits that changed the file" 3 "$(printf '%s\n' "$listing" | wc -l)"
printf '%s\n' "$listing" | sed -n 1p | grep -q 'third' || fail "history: newest first"
printf '%s\n' "$listing" | sed -n 3p | grep -q 'first' || fail "history: oldest last"
printf '%s\n' "$listing" | grep -q 'unrelated' && fail "history: lists a commit that left the file alone"

# Relative to the directory the command runs in.
(cd sub && "$BIN" history f.txt >/dev/null) || fail "history: path relative to the current directory"

expect_fail "history of a file nothing changed" "$BIN" history nope.txt
expect_fail "history without a path" "$BIN" history
expect_fail "history with two paths" "$BIN" history a.txt b.txt
expect_fail "history with an unknown option" "$BIN" history --nope a.txt

# A listed id is a version that restore takes.
oldest="$(printf '%s\n' "$listing" | sed -n 3p | cut -d' ' -f1)"
"$BIN" restore --from "$oldest" sub/f.txt >/dev/null 2>&1
expect_eq "restore of a listed version" v1 "$(cat sub/f.txt)"
git checkout -q -- sub/f.txt

# --- stashes -----------------------------------------------------------------------------------
echo a-stashed >a.txt
echo new >new.txt
git stash push -q -u -m "smoke"
echo b2 >b.txt
git commit -q -am "drift"
stash_before="$(git rev-parse 'stash@{0}')"

"$BIN" pick --stash a.txt >/dev/null 2>&1
expect_eq "pick --stash takes the file from stash@{0}" a-stashed "$(cat a.txt)"
expect_eq "pick --stash leaves other files alone" b2 "$(cat b.txt)"
[ ! -e new.txt ] || fail "pick --stash took a file it was not asked for"
git checkout -q -- a.txt

"$BIN" restore --stash --from 0 new.txt >/dev/null 2>&1
expect_eq "restore --stash takes an untracked file of the stash" new "$(cat new.txt)"

expect_eq "the stash entry is unchanged" "$stash_before" "$(git rev-parse 'stash@{0}')"
expect_eq "the stash is still listed" 1 "$(git stash list | wc -l)"

expect_fail "pick --stash with a file the stash does not hold" "$BIN" pick --stash nope.txt
expect_fail "pick --stash with a branch for --from" "$BIN" pick --stash --from main a.txt
expect_fail "pick --stash with --only" "$BIN" pick --stash --only a.txt a.txt

# --- modified files ----------------------------------------------------------------------------
# `restore --modified` discards unstaged edits in place, like `git restore <path>` for the named
# files: no session, no commit. Other edits stay.
echo a-edited >a.txt
echo b-edited >b.txt
head_before="$(git rev-parse HEAD)"
expect_fail "restore --modified of a file without edits" "$BIN" restore --modified new.txt
expect_fail "restore --modified with --stash" "$BIN" restore --modified --stash a.txt
expect_fail "restore --modified with --patch-only" "$BIN" restore --modified -p a.txt
"$BIN" restore --modified --dry-run a.txt >/dev/null 2>&1
expect_eq "a dry run keeps the edit" a-edited "$(cat a.txt)"

"$BIN" restore -M a.txt >/dev/null 2>&1
expect_eq "restore --modified returns the file to its staged state" a "$(cat a.txt)"
expect_eq "other edits are left alone" b-edited "$(cat b.txt)"
expect_eq "nothing is committed" "$head_before" "$(git rev-parse HEAD)"
git checkout -q -- b.txt

rm a.txt
"$BIN" restore -M a.txt >/dev/null 2>&1
expect_eq "restore --modified brings back a deleted file" a "$(cat a.txt)"
expect_eq "still nothing committed" "$head_before" "$(git rev-parse HEAD)"

echo "cli-smoke: ok"
