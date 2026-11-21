#!/bin/sh
# Sign a CodeSmith release's SHA256SUMS with minisign. Run this on the
# signing Mac, which holds the minisign secret key; the key never leaves it.
#
#   scripts/sign-release.sh v0.2.0 path/to/SHA256SUMS
#       sign that file: writes path/to/SHA256SUMS.minisig, uploads nothing
#       (sign-macos.sh does this, then uploads both with the tarballs)
#   scripts/sign-release.sh v0.2.0
#       download the draft release's SHA256SUMS, sign it and upload
#       SHA256SUMS.minisig (for a release whose SHA256SUMS is final already)
#   scripts/sign-release.sh --check
#       only check that minisign, the secret key and a pinned public key are
#       there (sign-macos.sh runs this before it starts)
#
# The signature's trusted comment is "codesmith <version> SHA256SUMS
# timestamp:<UTC time>". minisign signs the comment too, and `codesmith
# upgrade` and install.sh refuse a signature whose comment names another
# version, so an older release's signed checksums can't be served as a newer
# (or older) one.
#
# Environment:
#   CODESMITH_MINISIGN_KEY  the secret key (default ~/.minisign/codesmith.key);
#                           minisign asks for its password
#   CODESMITH_REPO          GitHub repo (default reka18/codesmith)
#   CODESMITH_DRY_RUN=1     sign, but don't upload (download-and-sign mode)
#
# One-time key generation is in RELEASING.md ("Signing the checksums").
set -eu

die() { printf 'sign-release: error: %s\n' "$*" >&2; exit 1; }
say() { printf 'sign-release: %s\n' "$*" >&2; }

[ $# -eq 1 ] || [ $# -eq 2 ] || die "usage: $0 TAG [SHA256SUMS] | --check   (e.g. v0.2.0)"
tag="$1"
case "$tag" in v*|--check) ;; *) tag="v$tag" ;; esac
version="${tag#v}"
sums="${2:-}"
repo="${CODESMITH_REPO:-reka18/codesmith}"
key="${CODESMITH_MINISIGN_KEY:-${HOME}/.minisign/codesmith.key}"
dry="${CODESMITH_DRY_RUN:-0}"
root=$(cd "$(dirname "$0")/.." && pwd)
pubfile="${root}/crates/codesmith/src/release_key.pub"

command -v minisign >/dev/null 2>&1 || die "need minisign: brew install minisign"
[ -f "$key" ] || die "no minisign secret key at $key (set CODESMITH_MINISIGN_KEY; RELEASING.md says how to make one)"
# The secret key must never be committed: refuse one inside this checkout.
case "$(cd "$(dirname "$key")" && pwd)/" in
    "${root}/"*) die "$key is inside the repository; keep the secret key outside it" ;;
esac

# The public keys codesmith upgrade trusts, as release_key.pub lists them.
keys=$(grep -v -e '^[[:space:]]*#' -e '^[[:space:]]*$' -e '^PLACEHOLDER$' "$pubfile" || true)
[ -n "$keys" ] || die "no release key in ${pubfile#"${root}/"}: generate one and add its public key first (RELEASING.md)"
[ "$tag" != --check ] || exit 0

work=""
if [ -z "$sums" ]; then
    command -v gh >/dev/null 2>&1 || die "need gh: brew install gh && gh auth login"
    work=$(mktemp -d -t codesmith-sign-release)
    trap 'rm -rf "$work"' EXIT
    trap 'exit 1' INT TERM
    gh release download "$tag" --repo "$repo" --dir "$work" --pattern SHA256SUMS
    sums="$work/SHA256SUMS"
fi
[ -f "$sums" ] || die "no such file: $sums"
grep -q "  codesmith-${version}-.*\.tar\.gz\$" "$sums" \
    || die "$sums lists no codesmith-${version} tarballs"

say "signing $sums for $tag with $key"
rm -f "${sums}.minisig"
minisign -S -s "$key" -m "$sums" -x "${sums}.minisig" \
    -c "CodeSmith ${version} release checksums" \
    -t "codesmith ${version} SHA256SUMS timestamp:$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# What codesmith upgrade will check: a prehashed signature (its line encodes
# "ED", so starts with RU), by a pinned key, naming this version.
case $(sed -n 2p "${sums}.minisig") in
    RU*) ;;
    *) die "minisign made a legacy signature; codesmith upgrade accepts prehashed ones only (use minisign 0.11 or later, without -l)" ;;
esac
verified=""
for k in $keys; do
    if comment=$(minisign -V -Q -P "$k" -m "$sums" -x "${sums}.minisig" 2>/dev/null); then
        verified="$k"
        break
    fi
done
[ -n "$verified" ] || die "the signature does not verify with any key in ${pubfile#"${root}/"}: is $key the right secret key?"
case "$comment" in
    "codesmith ${version} SHA256SUMS "*) ;;
    *) die "unexpected trusted comment: $comment" ;;
esac
say "verified with $verified: $comment"

if [ -n "$work" ]; then
    if [ "$dry" = 1 ]; then
        out="./codesmith-signed-$version"
        mkdir -p "$out"
        cp "$sums" "${sums}.minisig" "$out/"
        say "dry run: SHA256SUMS and its signature left in $out, nothing uploaded"
        exit 0
    fi
    say "uploading SHA256SUMS.minisig to release $tag"
    gh release upload "$tag" --repo "$repo" --clobber "${sums}.minisig"
    say "done. Check the release, then publish it: gh release edit $tag --repo $repo --draft=false"
fi
