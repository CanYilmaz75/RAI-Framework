#!/usr/bin/env bash
# Prüfbefehle aus der Definition of Done. Bricht beim ersten Fehler ab.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
