#!/bin/sh
# Sign and notarize the macOS binaries of a CodeSmith draft release, rewrite
# the macOS lines of SHA256SUMS, and replace both on the draft. Run this on
# skeptic-macmini, the Mac that holds the Developer ID key; nothing here
# needs (or should be given) CI secrets.
#
# It does not run minisign: the minisign key lives on another Mac
# (Skeptic-Mac). Afterwards the draft's SHA256SUMS is final but unsigned, and
# the next step is scripts/sign-release.sh TAG on that Mac. The draft stays a
# draft; publish it only after sign-release.sh has uploaded
# SHA256SUMS.minisig (RELEASING.md).
#
#   CODESMITH_BUILT_SUMS_SHA256=<hash release.sh printed> \
#   CODESMITH_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
#   CODESMITH_NOTARY_PROFILE=codesmith-notary \
#   scripts/sign-macos.sh v0.1.0
#
# The draft is not trusted: release.sh uploads BUILT-SHA256SUMS, the
# manifest of what it built, and prints that file's SHA-256, which you bring
# here yourself (CODESMITH_BUILT_SUMS_SHA256). This refuses unless the
# draft's manifest has that hash and every tarball on the draft, Linux and
# macOS, matches it, before anything is signed. Each macOS tarball must hold
# only plain files and folders under codesmith-<version>-<target>/; only its
# binary is extracted to be signed, in a folder of its own. The new
# SHA256SUMS is made from the manifest's Linux lines and the re-tarred macOS
# tarballs, never from the draft's SHA256SUMS.
#
# Environment:
#   CODESMITH_BUILT_SUMS_SHA256  SHA-256 of BUILT-SHA256SUMS, as release.sh
#                             printed it (required)
#   CODESMITH_SIGN_IDENTITY   codesign identity (see `security find-identity -v -p codesigning`)
#   CODESMITH_NOTARY_PROFILE  notarytool keychain profile, created once with
#                             `xcrun notarytool store-credentials`
#   CODESMITH_REPO            GitHub repo (default reka18/codesmith)
#   CODESMITH_BUNDLE_ID       code-signing identifier (default codesmith)
#   CODESMITH_DRY_RUN=1       do everything except notarize and upload
#
# Notarization and stapling: a bare Mach-O executable cannot carry a stapled
# ticket (stapler only handles .app, .pkg and .dmg). So each binary is zipped
# for submission only, notarized, and shipped as the signed binary inside the
# tarball. Gatekeeper finds the notarization ticket online the first time a
# quarantined copy runs. Downloads via curl, install.sh and Homebrew are not
# quarantined at all, so for them the signature is what matters.
set -eu

die() { printf 'sign-macos: error: %s\n' "$*" >&2; exit 1; }
say() { printf 'sign-macos: %s\n' "$*" >&2; }

[ $# -eq 1 ] || die "usage: $0 TAG   (e.g. v0.1.0)"
tag="$1"
case "$tag" in v*) ;; *) tag="v$tag" ;; esac
version="${tag#v}"
repo="${CODESMITH_REPO:-reka18/codesmith}"
identity="${CODESMITH_SIGN_IDENTITY:-}"
profile="${CODESMITH_NOTARY_PROFILE:-}"
dry="${CODESMITH_DRY_RUN:-0}"
bundle_id="${CODESMITH_BUNDLE_ID:-codesmith}"

pinned="${CODESMITH_BUILT_SUMS_SHA256:-}"

[ "$(uname -s)" = Darwin ] || die "run this on macOS"
case "$pinned" in
    *[!0-9a-f]*|"") die "set CODESMITH_BUILT_SUMS_SHA256 to the BUILT-SHA256SUMS hash release.sh printed (64 lowercase hex digits)" ;;
esac
[ "${#pinned}" = 64 ] || die "CODESMITH_BUILT_SUMS_SHA256 must be 64 lowercase hex digits"
[ -n "$identity" ] || die "set CODESMITH_SIGN_IDENTITY"
[ "$dry" = 1 ] || [ -n "$profile" ] || die "set CODESMITH_NOTARY_PROFILE"
for c in gh codesign xcrun ditto shasum tar; do
    command -v "$c" >/dev/null 2>&1 || die "need $c"
done
# Only a draft: a published release would be live, and unsigned, until
# sign-release.sh runs.
if [ "$dry" != 1 ]; then
    draft=$(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft) \
        || die "no release $tag on $repo"
    [ "$draft" = true ] || die "release $tag is already published; sign a draft (RELEASING.md)"
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/codesmith-sign.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 1' INT TERM
say "working in $work"

gh release download "$tag" --repo "$repo" --dir "$work" \
    --pattern BUILT-SHA256SUMS --pattern 'codesmith-*.tar.gz'
built="$work/BUILT-SHA256SUMS"
[ -f "$built" ] || die "release $tag has no BUILT-SHA256SUMS (made by release.sh)"
[ "$(shasum -a 256 "$built" | cut -c1-64)" = "$pinned" ] \
    || die "the draft's BUILT-SHA256SUMS does not have the hash release.sh printed; the draft was changed, or the hash is wrong"

# Every tarball on the draft must be the one release.sh built.
for target in aarch64-apple-darwin x86_64-apple-darwin \
    aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    name="codesmith-${version}-${target}.tar.gz"
    line=$(grep "^[0-9a-f]\{64\}  ${name}\$" "$built" || true)
    [ -n "$line" ] && [ "$(printf '%s\n' "$line" | grep -c .)" = 1 ] \
        || die "BUILT-SHA256SUMS does not list $name exactly once"
    [ -f "$work/$name" ] || die "missing $name on release $tag"
    (cd "$work" && printf '%s\n' "$line" | shasum -a 256 -c - >/dev/null 2>&1) \
        || die "$name on the draft does not match BUILT-SHA256SUMS: it is not what release.sh built"
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

signed=""
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    name="codesmith-${version}-${target}"
    tarball="$work/${name}.tar.gz"
    check_members "$tarball" "$name"

    say "unpacking the binary of $name"
    bindir="$work/bin-$target"
    rm -rf "$bindir"
    mkdir "$bindir"
    tar -xzf "$tarball" -C "$bindir" "${name}/codesmith" \
        || die "$name.tar.gz has no codesmith binary"
    bin="$bindir/$name/codesmith"
    [ -f "$bin" ] && [ ! -L "$bin" ] || die "$name.tar.gz has no codesmith binary"

    say "signing $name"
    # --options runtime: hardened runtime, required for notarization.
    # --timestamp: secure timestamp, required for notarization.
    codesign --force --sign "$identity" --options runtime --timestamp \
        --identifier "$bundle_id" "$bin"
    codesign --verify --strict --verbose=2 "$bin"

    if [ "$dry" = 1 ]; then
        say "dry run: skipping notarization of $name"
    else
        say "notarizing $name (this can take a few minutes)"
        ditto -c -k --keepParent "$bin" "$work/$name-notarize.zip"
        xcrun notarytool submit "$work/$name-notarize.zip" \
            --keychain-profile "$profile" --wait --timeout 30m \
            > "$work/$name-notary.log" 2>&1 || {
            cat "$work/$name-notary.log" >&2
            die "notarytool failed for $name"
        }
        cat "$work/$name-notary.log" >&2
        grep -q 'status: Accepted' "$work/$name-notary.log" \
            || die "notarization of $name was not accepted (see log above; \`xcrun notarytool log <id>\` has details)"
        rm -f "$work/$name-notarize.zip"
    fi

    say "repacking $name"
    # The rest of the tarball (checked above, and matching what release.sh
    # built), with the signed binary in place of the unsigned one.
    tree="$work/tree-$target"
    rm -rf "$tree"
    mkdir "$tree"
    tar -xzf "$tarball" -C "$tree"
    rm -f "$tree/$name/codesmith"
    cp "$bin" "$tree/$name/codesmith"
    rm -f "$tarball"
    # COPYFILE_DISABLE keeps macOS from adding ._ AppleDouble files.
    COPYFILE_DISABLE=1 tar -C "$tree" --no-xattrs --no-mac-metadata \
        --uid 0 --gid 0 --uname root --gname root -czf "$tarball" "$name"
    signed="$signed $tarball"
done

# The final SHA256SUMS: the Linux lines as release.sh built them, and the
# re-tarred macOS tarballs.
grep "^[0-9a-f]\{64\}  codesmith-${version}-[a-z0-9_-]*-linux-musl\.tar\.gz\$" "$built" \
    > "$work/SHA256SUMS.new" || die "BUILT-SHA256SUMS has no Linux lines"
for t in $signed; do
    (cd "$work" && shasum -a 256 "$(basename "$t")") >> "$work/SHA256SUMS.new"
done
sort -k 2 "$work/SHA256SUMS.new" > "$work/SHA256SUMS"
say "new SHA256SUMS:"
cat "$work/SHA256SUMS" >&2

if [ "$dry" = 1 ]; then
    out="./codesmith-signed-$version"
    rm -rf "$out"
    mkdir -p "$out"
    # shellcheck disable=SC2086 # $signed is a space-separated list of paths without spaces
    cp $signed "$work/SHA256SUMS" "$out/"
    say "dry run: signed files left in $out, nothing uploaded"
    exit 0
fi

say "uploading to release $tag"
# shellcheck disable=SC2086
gh release upload "$tag" --repo "$repo" --clobber $signed "$work/SHA256SUMS"
# A signature from an earlier run signs the old SHA256SUMS; drop it so the
# draft never carries a .minisig that doesn't match.
if gh release view "$tag" --repo "$repo" --json assets --jq '.assets[].name' \
    | grep -qx 'SHA256SUMS\.minisig'; then
    say "removing the stale SHA256SUMS.minisig"
    gh release delete-asset "$tag" SHA256SUMS.minisig --repo "$repo" --yes
fi
say "done: the draft $tag has the signed macOS tarballs and the final SHA256SUMS, still unsigned."
say "next: run \`scripts/sign-release.sh $tag\` on the minisign Mac (Skeptic-Mac); publish only after that"
