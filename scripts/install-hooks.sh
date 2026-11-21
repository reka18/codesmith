#!/bin/sh
# Install the git hooks: a pre-commit format check and a pre-push run of
# scripts/ci.sh (and scripts/ci-linux.sh when CODESMITH_CI_LINUX=1). Hooks
# live in the shared git directory, so every worktree uses them.
set -eu
cd "$(dirname "$0")/.."
hooks="$(git rev-parse --git-common-dir)/hooks"
mkdir -p "$hooks"

cat > "$hooks/pre-commit" <<'EOF'
#!/bin/sh
# Refuse commits that fail `cargo fmt --all --check`.
if ! cargo fmt --all --check >/dev/null 2>&1; then
  echo "pre-commit: cargo fmt --all --check failed; run cargo fmt --all" >&2
  exit 1
fi
EOF

cat > "$hooks/pre-push" <<'EOF'
#!/bin/sh
# Run local CI before every push; a failure stops the push. With
# CODESMITH_CI_LINUX=1, the Linux run in Docker (scripts/ci-linux.sh) too.
set -e
root="$(git rev-parse --show-toplevel)"
"$root/scripts/ci.sh"
if [ "${CODESMITH_CI_LINUX:-0}" = 1 ]; then
  "$root/scripts/ci-linux.sh"
fi
EOF

chmod +x "$hooks/pre-commit" "$hooks/pre-push"
echo "installed pre-commit and pre-push hooks in $hooks"
