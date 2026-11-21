#!/bin/sh
# Sign a CodeSmith draft release's SHA256SUMS with minisign and upload
# SHA256SUMS.minisig. Run this on Skeptic-Mac, the Mac that holds the
# minisign secret key (the key never leaves it), after scripts/sign-macos.sh
# has run on skeptic-macmini: that signs and notarizes the macOS binaries,
# rewrites SHA256SUMS and uploads both, so the SHA256SUMS signed here is the
# final one.
#
#   scripts/sign-release.sh v0.2.0
#       download the draft's SHA256SUMS and its tarballs, check that every
#       tarball matches SHA256SUMS and that the macOS binaries carry a
#       Developer ID signature, sign SHA256SUMS, verify the signature against
#       the pinned keys, and upload SHA256SUMS.minisig
#   scripts/sign-release.sh --check
#       only check that minisign, the secret key and a pinned public key are
#       there; talks to nothing
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
#   CODESMITH_DRY_RUN=1     download, check and sign, but upload nothing; the
#                           files are left in ./codesmith-signed-<version>/
#
# Key generation, backup and rotation are in RELEASING.md ("Signing the
# checksums").
set -eu

die() { printf 'sign-release: error: %s\n' "$*" >&2; exit 1; }
say() { printf 'sign-release: %s\n' "$*" >&2; }

[ $# -eq 1 ] || die "usage: $0 TAG | --check   (e.g. v0.2.0)"
tag="$1"
case "$tag" in v*|--check) ;; -*) die "usage: $0 TAG | --check" ;; *) tag="v$tag" ;; esac
version="${tag#v}"
repo="${CODESMITH_REPO:-reka18/codesmith}"
key="${CODESMITH_MINISIGN_KEY:-${HOME}/.minisign/codesmith.key}"
dry="${CODESMITH_DRY_RUN:-0}"
root=$(cd "$(dirname "$0")/.." && pwd)
pubfile="${root}/crates/codesmith/src/release_key.pub"

command -v minisign >/dev/null 2>&1 || die "need minisign: brew install minisign"
[ -f "$key" ] || die "no minisign secret key at $key (set CODESMITH_MINISIGN_KEY; RELEASING.md says how to make one)"
# The secret key must never be committed: refuse one inside this checkout.
case "$(cd "$(dirname "$key")" && pwd -P)/" in
    "$(cd "$root" && pwd -P)/"*) die "$key is inside the repository; keep the secret key outside it" ;;
esac

# The public keys codesmith upgrade trusts, as release_key.pub lists them.
keys=$(grep -v -e '^[[:space:]]*#' -e '^[[:space:]]*$' -e '^PLACEHOLDER$' "$pubfile" || true)
[ -n "$keys" ] || die "no release key in ${pubfile#"${root}/"}: generate one and add its public key first (RELEASING.md)"
[ "$tag" != --check ] || { say "ok: minisign, $key and a pinned key are there"; exit 0; }

for c in gh shasum tar codesign; do
    command -v "$c" >/dev/null 2>&1 || die "need $c"
done

# Only a draft: a published release is live already, and the signature
# must be on it before anyone can download it.
draft=$(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft) \
    || die "no release $tag on $repo"
[ "$draft" = true ] || die "release $tag is already published; sign it while it is a draft (RELEASING.md)"

work=$(mktemp -d "${TMPDIR:-/tmp}/codesmith-sign-release.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 1' INT TERM
say "downloading SHA256SUMS and the tarballs of $tag"
gh release download "$tag" --repo "$repo" --dir "$work" \
    --pattern SHA256SUMS --pattern 'codesmith-*.tar.gz'
sums="$work/SHA256SUMS"
[ -f "$sums" ] || die "release $tag has no SHA256SUMS"

# SHA256SUMS must list exactly the four tarballs of this version, and each
# must be on the draft with that hash. The macOS lines are the ones
# sign-macos.sh rewrote: a mismatch there means it hasn't run, didn't finish
# uploading, or something else changed the draft.
for target in aarch64-apple-darwin x86_64-apple-darwin \
    aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    name="codesmith-${version}-${target}.tar.gz"
    [ "$(grep -c "^[0-9a-f]\{64\}  ${name}\$" "$sums" || true)" = 1 ] \
        || die "SHA256SUMS of $tag does not list $name exactly once"
    [ -f "$work/$name" ] || die "release $tag has no $name"
done
[ "$(grep -c . "$sums")" = 4 ] || die "SHA256SUMS of $tag has lines other than the four tarballs"
(cd "$work" && shasum -a 256 -c SHA256SUMS >&2) \
    || die "the tarballs on draft $tag do not match its SHA256SUMS; run scripts/sign-macos.sh $tag on the mini first, or find out what changed the draft"

# The macOS binaries must be the Developer ID-signed ones, so the checksums
# signed here are the final ones, not CI's unsigned builds.
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    name="codesmith-${version}-${target}"
    tar -xzf "$work/${name}.tar.gz" -C "$work" "${name}/codesmith" \
        || die "${name}.tar.gz has no codesmith binary"
    codesign --verify --strict "$work/${name}/codesmith" 2>/dev/null \
        || die "the binary in ${name}.tar.gz is not code-signed; run scripts/sign-macos.sh $tag on the mini first"
    codesign -dv --verbose=2 "$work/${name}/codesmith" 2>&1 \
        | grep -q '^Authority=Developer ID Application' \
        || die "the binary in ${name}.tar.gz is not signed with a Developer ID; run scripts/sign-macos.sh $tag on the mini first"
done
say "SHA256SUMS matches the draft's tarballs, and the macOS binaries are Developer ID-signed"

say "signing SHA256SUMS of $tag with $key"
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
comment=""
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

if [ "$dry" = 1 ]; then
    out="./codesmith-signed-$version"
    mkdir -p "$out"
    cp "$sums" "${sums}.minisig" "$out/"
    say "dry run: SHA256SUMS and its signature left in $out, nothing uploaded"
    exit 0
fi

say "uploading SHA256SUMS.minisig to draft $tag"
gh release upload "$tag" --repo "$repo" --clobber "${sums}.minisig"
say "done: $tag is signed. Check it (RELEASING.md), then publish it: gh release edit $tag --repo $repo --draft=false"
