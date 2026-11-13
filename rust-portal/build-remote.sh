#!/bin/sh
# Copies this crate to portal2 and builds it there (release, low priority, 2 jobs).
# Usage: ./build-remote.sh [cargo args...]   e.g. ./build-remote.sh test
set -e
HOST="${HOST:-ubuntu@portal2.vigor.buzz}"
PORT="${PORT:-5922}"
cd "$(dirname "$0")"
tar -c --exclude=target Cargo.toml Cargo.lock src migrations web deploy 2>/dev/null \
  | ssh -p "$PORT" "$HOST" 'mkdir -p ~/rust-portal && tar -x -C ~/rust-portal'
CMD="${*:-build --release}"
ssh -p "$PORT" "$HOST" "cd ~/rust-portal && . ~/.cargo/env && nice -n 19 cargo $CMD -j2 2>&1"
# Keep the lock file in sync locally.
scp -q -P "$PORT" "$HOST:rust-portal/Cargo.lock" Cargo.lock
