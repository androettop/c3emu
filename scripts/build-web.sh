#!/usr/bin/env bash
# Build the web version into ./site (static files: open it with any web server,
# e.g. `cargo run --release -p c3emu-web --bin serve`).
# Needs the wasm32-unknown-unknown target: rustup target add wasm32-unknown-unknown
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p c3emu-web --lib --target wasm32-unknown-unknown
rm -rf site
mkdir -p site
cp web/index.html web/app.js web/worker.js site/
cp crates/c3emu/assets/shell.png site/
cp target/wasm32-unknown-unknown/release/c3emu_web.wasm site/
ls -l site
