#!/bin/sh
# Regenerate crates/codesmith-core/PARITY.md from an OpenCode checkout.
# Usage: scripts/parity/core/run.sh <opencode checkout>
# Needs node 18+ and the `typescript` package on NODE_PATH (see README.md).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
opencode=${1:?usage: run.sh <opencode checkout>}
tests="$opencode/packages/core/test"
[ -d "$tests" ] || { echo "no $tests" >&2; exit 1; }
out="$repo/target/parity-core"
mkdir -p "$out"
node "$here/extract.js" "$tests" "$out"
node "$here/match.js" "$repo/crates/codesmith-core" "$out"
node "$here/classify.js" "$out"
node "$here/render.js" "$out" "$repo/crates/codesmith-core/PARITY.md"
echo "wrote crates/codesmith-core/PARITY.md"
