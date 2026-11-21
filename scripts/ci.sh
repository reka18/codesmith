#!/bin/sh
# Local CI: what must pass before anything is pushed. There is no hosted CI;
# the pre-push hook (scripts/install-hooks.sh) runs this.
set -eu
cd "$(dirname "$0")/.."

# Git sets GIT_DIR, GIT_INDEX_FILE and friends for the hooks it runs. A test
# that runs git in a temporary repository would inherit them and act on this
# repository instead (it once rewrote .git/config and a worktree's index), so
# clear every repository-local variable git knows before anything runs.
for v in $(git rev-parse --local-env-vars 2>/dev/null) GIT_DIR GIT_WORK_TREE \
  GIT_INDEX_FILE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY GIT_PREFIX; do
  unset "$v"
done

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
