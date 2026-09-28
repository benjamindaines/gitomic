#!/bin/bash

c_red=$'\033[31m'; c_grn=$'\033[32m'; c_ylw=$'\033[33m'; c_blu=$'\033[34m'; c_rst=$'\033[0m'
log()  { printf '%s[*]%s %s\n' "$c_blu" "$c_rst" "$*"; }
ok()   { printf '%s[+]%s %s\n' "$c_grn" "$c_rst" "$*"; }
warn() { printf '%s[!]%s %s\n' "$c_ylw" "$c_rst" "$*" >&2; }
die2()  { printf '%s[x]%s %s\n' "$c_red" "$c_rst" "$*" >&2; exit 1; }

gitomic build-safe -q || die2 "gitomic is running, close out that session or you'll commit a whole bunch of garbage to your tree."
ok "gitomic not running... leggo.."

cargo build --release || exit 1

log "installing..."
cp -f target/release/gitomic ~/.local/bin/gitomic || \
	die2 "could not install for some reason..."

ok "all set"
exit 0
