#!/bin/sh
# Local CI on Linux: the checks of scripts/ci.sh (fmt, clippy -D warnings,
# workspace tests, shellcheck) run in a Docker container, to catch Linux-only
# breakage a macOS run misses.
#
#   scripts/ci-linux.sh            linux/arm64 (native on Apple Silicon)
#   scripts/ci-linux.sh --amd64    linux/x86_64 (emulated; much slower)
#
# The repo is mounted read-only and copied into the container; the host tree
# is never written. Cargo's registry and target dir live in named volumes
# (codesmith-ci-*-<arch>) so reruns are incremental; `docker volume rm` them
# to start cold. Dependencies are fetched with network, then every check runs
# with --network none. The container runs as a non-root user under CPU,
# memory and pids limits, tunable through:
#
#   CI_LINUX_CPUS (8)  CI_LINUX_MEMORY (6g)  CI_LINUX_PIDS (4096)
#   CI_LINUX_JOBS (cargo jobs, 4)  CI_LINUX_TIMEOUT (seconds for the checks, 3600)
#
# Debug info is cut to line tables: full debug info with parallel links
# does not fit in the memory limit.
set -eu
cd "$(dirname "$0")/.."
root="$(pwd)"

arch=arm64
for arg in "$@"; do
  case "$arg" in
    --amd64) arch=amd64 ;;
    --arm64) arch=arm64 ;;
    -h | --help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "ci-linux: unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

# Rust matches the workspace's rust-version (Cargo.toml); bump both together.
rust_image="rust:1.91.1-trixie"
platform="linux/$arch"
image="codesmith-ci-linux:1.91.1-$arch"
cargo_vol="codesmith-ci-cargo-$arch"
target_vol="codesmith-ci-target-$arch"
cpus="${CI_LINUX_CPUS:-8}"
memory="${CI_LINUX_MEMORY:-6g}"
pids="${CI_LINUX_PIDS:-4096}"
jobs="${CI_LINUX_JOBS:-4}"
timeout="${CI_LINUX_TIMEOUT:-3600}"

if ! docker info >/dev/null 2>&1; then
  echo "ci-linux: docker is not running" >&2
  exit 1
fi

step() { printf '\n== %s\n' "$*"; }
start=$(date +%s)

step "image $image ($platform, from $rust_image)"
# The shells are for the shell differential tests, python3 for the MCP
# fixtures. uid 1000 is not root: root bypasses the permission tests.
docker build --quiet --platform "$platform" --tag "$image" - >/dev/null <<EOF
FROM $rust_image
RUN apt-get update \
 && apt-get install -y --no-install-recommends bash dash zsh python3 git shellcheck \
 && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy rustfmt
RUN useradd --create-home --uid 1000 ci \
 && mkdir -p /ci/cargo /ci/target /work \
 && chown ci:ci /ci/cargo /ci/target /work
ENV CARGO_HOME=/ci/cargo CARGO_TARGET_DIR=/ci/target PATH=/usr/local/cargo/bin:\$PATH
USER ci
WORKDIR /work
EOF

# Each phase is a script on stdin. It first copies the tree (tracked and
# untracked, without target/ and .git, whose worktree pointer names a host
# path) into the container's own /work.
run() {
  docker run --rm -i --init --platform "$platform" \
    --network "$1" \
    --cpus "$cpus" --memory "$memory" --memory-swap "$memory" \
    --pids-limit "$pids" \
    --security-opt no-new-privileges \
    --mount "type=bind,source=$root,target=/src,readonly" \
    --mount "type=volume,source=$cargo_vol,target=/ci/cargo" \
    --mount "type=volume,source=$target_vol,target=/ci/target" \
    -e CARGO_TERM_COLOR=never -e CARGO_BUILD_JOBS="$jobs" -e TIMEOUT="$timeout" \
    -e CARGO_PROFILE_DEV_DEBUG=line-tables-only \
    "$image" sh -euc 'tar -C /src --exclude=./target --exclude=./.git -cf - . | tar -C /work -xf -; exec sh -s'
}

step "cargo fetch (network)"
run bridge <<'EOF'
cargo fetch --locked
EOF

step "checks (--network none)"
status=0
run none <<'EOF' || status=$?
export CARGO_NET_OFFLINE=true
cat > /tmp/checks.sh <<'CHECKS'
fail=0
step() { printf '\n== %s\n' "$*"; }
step "cargo fmt --all --check"
cargo fmt --all --check || fail=1
step "cargo clippy --all-targets --workspace -- -D warnings"
cargo clippy --all-targets --workspace --locked --keep-going -- -D warnings || fail=1
step "cargo test --workspace"
{ cargo test --workspace --locked --no-fail-fast; echo $? > /tmp/test.rc; } 2>&1 | tee /tmp/test.log
[ "$(cat /tmp/test.rc)" = 0 ] || fail=1
step shellcheck
shellcheck install.sh scripts/*.sh || fail=1
if grep -q ' \.\.\. FAILED$\|^error: could not compile' /tmp/test.log; then
  printf '\n== failing tests\n'
  grep ' \.\.\. FAILED$\|^error: could not compile' /tmp/test.log |
    sed 's/^test /  /; s/ \.\.\. FAILED$//; s/^error: /  /'
fi
exit "$fail"
CHECKS
exec timeout "$TIMEOUT" sh /tmp/checks.sh
EOF

elapsed=$(($(date +%s) - start))
if [ "$status" -ne 0 ]; then
  [ "$status" -eq 124 ] && echo "ci-linux: checks timed out after ${timeout}s" >&2
  printf '\nCI (%s) FAILED after %ss\n' "$platform" "$elapsed" >&2
  exit 1
fi
printf '\nCI (%s) passed in %ss\n' "$platform" "$elapsed"
