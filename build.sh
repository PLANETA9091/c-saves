#!/usr/bin/env bash
# CRUSSTY module build: native Rust engine -> dist/saves/
# The dist/saves directory IS the module: drop it into a server's modules/
# (or `crussty install PLANETA9091/c-saves` from a release bundle).
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

( cd native && cargo build --release )

rm -rf dist/saves
mkdir -p dist/saves
cp native/target/release/libc_saves.so dist/saves/libc-saves.so
cp module.json dist/saves/

echo "--- module -> dist/saves/ (module.json + libc-saves.so) ---"
ls -la dist/saves/
