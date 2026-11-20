#!/bin/sh
# Render the Homebrew formula for a release.
#
#   scripts/update-homebrew.sh VERSION [SHA256SUMS] [OUTPUT]
#
# VERSION is e.g. 0.1.0 or v0.1.0. SHA256SUMS defaults to downloading the
# release's checksum file with `gh`. OUTPUT defaults to stdout. Run it after
# scripts/sign-macos.sh, since signing changes the macOS checksums.
set -eu

die() { printf 'update-homebrew: error: %s\n' "$*" >&2; exit 1; }

[ $# -ge 1 ] || die "usage: $0 VERSION [SHA256SUMS] [OUTPUT]"
version="${1#v}"
sums="${2:-}"
out="${3:-}"
root=$(cd "$(dirname "$0")/.." && pwd)
template="${root}/packaging/homebrew/codesmith.rb"
repo="${CODESMITH_REPO:-reka18/codesmith}"

case "$version" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) die "bad version: $version" ;;
esac

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t codesmith)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM

if [ -z "$sums" ]; then
    command -v gh >/dev/null 2>&1 || die "pass a SHA256SUMS file or install gh"
    gh release download "v${version}" --repo "$repo" --pattern SHA256SUMS --dir "$tmp"
    sums="${tmp}/SHA256SUMS"
fi
[ -f "$sums" ] || die "no such file: $sums"

sha_for() {
    file="codesmith-${version}-$1.tar.gz"
    sha=$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$sums")
    printf '%s\n' "$sha" | grep -Eq '^[0-9a-f]{64}$' || die "no sha256 for ${file} in ${sums}"
    printf '%s\n' "$sha"
}

a64_mac=$(sha_for aarch64-apple-darwin)
x64_mac=$(sha_for x86_64-apple-darwin)
a64_linux=$(sha_for aarch64-unknown-linux-musl)
x64_linux=$(sha_for x86_64-unknown-linux-musl)

sed -e "s/@VERSION@/${version}/" \
    -e "s/@SHA256_AARCH64_APPLE_DARWIN@/${a64_mac}/" \
    -e "s/@SHA256_X86_64_APPLE_DARWIN@/${x64_mac}/" \
    -e "s/@SHA256_AARCH64_UNKNOWN_LINUX_MUSL@/${a64_linux}/" \
    -e "s/@SHA256_X86_64_UNKNOWN_LINUX_MUSL@/${x64_linux}/" \
    -e '/^#$/,/^# Formula\/codesmith.rb/d' \
    "$template" > "${tmp}/codesmith.rb"

if grep -q '@[A-Z0-9_]*@' "${tmp}/codesmith.rb"; then
    die "unfilled placeholder left in formula"
fi

if [ -n "$out" ]; then
    cp "${tmp}/codesmith.rb" "$out"
    printf 'update-homebrew: wrote %s\n' "$out" >&2
else
    cat "${tmp}/codesmith.rb"
fi
