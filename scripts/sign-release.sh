#!/bin/sh
# Sign a CodeSmith draft release's SHA256SUMS with minisign and upload
# SHA256SUMS.minisig. Run this on Skeptic-Mac, the Mac that holds the
# minisign secret key (the key never leaves it), after scripts/sign-macos.sh
# has run on skeptic-macmini: that signs and notarizes the macOS binaries,
# rewrites SHA256SUMS and uploads both, so the SHA256SUMS signed here is the
# final one.
#
#   CODESMITH_BUILT_SUMS_SHA256=<hash release.sh printed> \
#   CODESMITH_TEAM_ID=<Apple Team ID> scripts/sign-release.sh v0.2.0
#       download the draft's SHA256SUMS, BUILT-SHA256SUMS and tarballs, check
#       them (below), sign SHA256SUMS, verify the signature against the
#       pinned keys, and upload SHA256SUMS.minisig
#   scripts/sign-release.sh --check
#       only check that minisign, the secret key and a pinned public key are
#       there; talks to nothing
#
# The draft is not trusted. Before signing, this checks that:
#   - BUILT-SHA256SUMS, the manifest release.sh uploaded, has the SHA-256
#     release.sh printed (CODESMITH_BUILT_SUMS_SHA256, brought here by you);
#   - SHA256SUMS lists the four tarballs, each on the draft with that hash,
#     and its Linux lines are the manifest's;
#   - each macOS tarball holds only plain files and folders, exactly the
#     members the manifest lists, each file but the binary as built;
#   - each macOS binary is Developer ID-signed by the pinned team
#     (TeamIdentifier = CODESMITH_TEAM_ID).
#
# The signature's trusted comment is "codesmith <version> SHA256SUMS
# timestamp:<UTC time>". minisign signs the comment too, and `codesmith
# upgrade` and install.sh refuse a signature whose comment names another
# version, so an older release's signed checksums can't be served as a newer
# (or older) one.
#
# Environment:
#   CODESMITH_BUILT_SUMS_SHA256  SHA-256 of BUILT-SHA256SUMS, as release.sh
#                           printed it (required, except for --check)
#   CODESMITH_TEAM_ID       the Apple Team ID the macOS binaries must be
#                           signed by (required, except for --check)
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

pinned="${CODESMITH_BUILT_SUMS_SHA256:-}"
case "$pinned" in
    *[!0-9a-f]*|"") die "set CODESMITH_BUILT_SUMS_SHA256 to the BUILT-SHA256SUMS hash release.sh printed (64 lowercase hex digits)" ;;
esac
[ "${#pinned}" = 64 ] || die "CODESMITH_BUILT_SUMS_SHA256 must be 64 lowercase hex digits"
team="${CODESMITH_TEAM_ID:-}"
case "$team" in
    *[!0-9A-Z]*|"") die "set CODESMITH_TEAM_ID to the Apple Team ID that signs the macOS binaries (10 letters and digits)" ;;
esac

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
    --pattern SHA256SUMS --pattern BUILT-SHA256SUMS --pattern 'codesmith-*.tar.gz'
sums="$work/SHA256SUMS"
[ -f "$sums" ] || die "release $tag has no SHA256SUMS"
built="$work/BUILT-SHA256SUMS"
[ -f "$built" ] || die "release $tag has no BUILT-SHA256SUMS (made by release.sh)"
[ "$(shasum -a 256 "$built" | cut -c1-64)" = "$pinned" ] \
    || die "the draft's BUILT-SHA256SUMS does not have the hash release.sh printed; the draft was changed, or the hash is wrong"

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

# The Linux tarballs are the ones release.sh built.
for target in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    name="codesmith-${version}-${target}.tar.gz"
    want=$(grep "^[0-9a-f]\{64\}  ${name}\$" "$built" || true)
    [ -n "$want" ] || die "BUILT-SHA256SUMS does not list $name"
    [ "$(grep "  ${name}\$" "$sums")" = "$want" ] \
        || die "$name on draft $tag is not the one release.sh built (BUILT-SHA256SUMS)"
done

# check_members TARBALL NAME: refuse a tarball holding anything but plain
# files and folders under NAME/ (no links, devices or FIFOs, no absolute
# or `..` paths).
check_members() {
    tar -tvzf "$1" > "$work/listing" || die "cannot list $1"
    while IFS= read -r l; do
        case "$l" in
            l*|h*|-*" -> "*|-*" link to "*) die "$(basename "$1") has a link: $l" ;;
            -*|d*) ;;
            *) die "$(basename "$1") has a member that is not a plain file or folder: $l" ;;
        esac
    done < "$work/listing"
    tar -tzf "$1" > "$work/names" || die "cannot list $1"
    while IFS= read -r m; do
        case "$m" in
            "$2/"|"$2/"*) ;;
            *) die "$(basename "$1") has a member outside $2/: $m" ;;
        esac
        case "/${m%/}/" in
            */../*|*/./*|*//*) die "$(basename "$1") has an unsafe member name: $m" ;;
        esac
    done < "$work/names"
}

# Each macOS tarball holds what release.sh built, but for the binary, and
# its binary is Developer ID-signed by the pinned team, so the checksums
# signed here are the final ones, not the unsigned builds.
dashes=$(printf '%064d' 0 | tr 0 -)
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    name="codesmith-${version}-${target}"
    tarball="$work/${name}.tar.gz"
    check_members "$tarball" "$name"
    tree="$work/tree-$target"
    rm -rf "$tree"
    mkdir "$tree"
    tar -xzf "$tarball" -C "$tree" || die "cannot unpack ${name}.tar.gz"
    LC_ALL=C sort "$work/names" | while IFS= read -r m; do
        case "$m" in
            "${name}/codesmith") h=binary ;;
            */) h="$dashes" ;;
            *) h=$(shasum -a 256 "$tree/$m" | cut -c1-64) ;;
        esac
        printf '%s  %s.tar.gz//%s\n' "$h" "$name" "$m"
    done > "$work/members"
    grep "  ${name}\.tar\.gz//" "$built" | LC_ALL=C sort -k 2 \
        | sed "s|^[0-9a-f]\{64\}  \(${name}\.tar\.gz//${name}/codesmith\)\$|binary  \1|" \
        > "$work/members.built"
    grep -q "^binary  ${name}\.tar\.gz//${name}/codesmith\$" "$work/members.built" \
        || die "BUILT-SHA256SUMS lists no binary for ${name}.tar.gz"
    LC_ALL=C sort -k 2 "$work/members" | cmp -s - "$work/members.built" \
        || die "${name}.tar.gz on draft $tag holds other files than release.sh built (BUILT-SHA256SUMS)"
    bin="$tree/${name}/codesmith"
    [ -f "$bin" ] && [ ! -L "$bin" ] || die "${name}.tar.gz has no codesmith binary"
    codesign --verify --strict "$bin" 2>/dev/null \
        || die "the binary in ${name}.tar.gz is not code-signed; run scripts/sign-macos.sh $tag on the mini first"
    info=$(codesign -dv --verbose=2 "$bin" 2>&1) \
        || die "cannot read the signature of the binary in ${name}.tar.gz"
    printf '%s\n' "$info" | grep -q '^Authority=Developer ID Application' \
        || die "the binary in ${name}.tar.gz is not signed with a Developer ID; run scripts/sign-macos.sh $tag on the mini first"
    [ "$(printf '%s\n' "$info" | sed -n 's/^TeamIdentifier=//p')" = "$team" ] \
        || die "the binary in ${name}.tar.gz is signed by team $(printf '%s\n' "$info" | sed -n 's/^TeamIdentifier=//p'), not $team"
done
say "SHA256SUMS matches the draft's tarballs, which are the ones built (but the signed binaries), and the macOS binaries are signed by team $team"

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
