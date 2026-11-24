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

# A pass is recorded for the exact code it checked: the commit's tree, when
# the working tree is clean (no changes, no untracked files), and the
# toolchain. `--unless-passed TREE...` (the pre-push hook) skips the run
# when every TREE is the one recorded, so pushing right after a passing run
# doesn't run it again.
stamp_file="$(git rev-parse --git-dir)/codesmith-ci-passed"
toolchain="$(rustc -V 2>/dev/null); $(cargo -V 2>/dev/null)"
clean_tree() {
  [ -z "$(git status --porcelain --untracked-files=normal)" ] &&
    git rev-parse 'HEAD^{tree}'
}
if [ "${1:-}" = --unless-passed ]; then
  shift
  if [ "$#" -gt 0 ] && [ -f "$stamp_file" ]; then
    passed_tree="$(sed -n 1p "$stamp_file")"
    passed_toolchain="$(sed -n 2p "$stamp_file")"
    all=1
    for t in "$@"; do
      [ "$t" = "$passed_tree" ] || all=0
    done
    if [ "$all" = 1 ] && [ "$passed_toolchain" = "$toolchain" ]; then
      printf 'CI already passed for this code (tree %s); not running it again.\n' \
        "$(printf %s "$passed_tree" | cut -c1-12)"
      exit 0
    fi
  fi
fi
start_tree="$(clean_tree || true)"
rm -f "$stamp_file"

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

# Recorded only when nothing changed while it ran.
if [ -n "$start_tree" ] && [ "$(clean_tree || true)" = "$start_tree" ]; then
  printf '%s\n%s\n' "$start_tree" "$toolchain" > "$stamp_file"
fi
printf '\nCI passed\n'
