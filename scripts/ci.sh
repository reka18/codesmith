#!/bin/sh
# Local CI: what must pass before anything is pushed. There is no hosted CI;
# the pre-push hook (scripts/install-hooks.sh) runs this.
set -eu
cd "$(dirname "$0")/.."

step() { printf '\n== %s\n' "$*"; }

step "cargo fmt --all --check"
cargo fmt --all --check

step "cargo clippy --all-targets --workspace -- -D warnings"
cargo clippy --all-targets --workspace -- -D warnings

step "cargo test --workspace"
cargo test --workspace

if command -v shellcheck >/dev/null 2>&1; then
  step "shellcheck"
  shellcheck install.sh scripts/*.sh
else
  printf '\n(shellcheck not installed; skipped)\n'
fi

printf '\nCI passed\n'
