#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$project_dir/web"
npm ci
npm run check
cd "$project_dir/server"
cargo fmt --all -- --check
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-features --all-targets -- -D warnings
cd "$project_dir/server/tools/browser"
npm ci
npm run check
npm run build
node --test dist/tests/protocol.test.js dist/tests/worker.test.js
