#!/bin/sh
# Sign and notarize the macOS binaries of a CodeSmith release, rewrite
# SHA256SUMS, sign it with minisign (scripts/sign-release.sh), then replace
# them on the GitHub Release and add SHA256SUMS.minisig. Run this on the Mac
# that holds the Developer ID key and the minisign secret key; nothing here
# needs (or should be given) CI secrets.
#
#   CODESMITH_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
#   CODESMITH_NOTARY_PROFILE=codesmith-notary \
#   scripts/sign-macos.sh v0.1.0
#
# Environment:
#   CODESMITH_SIGN_IDENTITY   codesign identity (see `security find-identity -v -p codesigning`)
#   CODESMITH_NOTARY_PROFILE  notarytool keychain profile, created once with
#                             `xcrun notarytool store-credentials`
#   CODESMITH_REPO            GitHub repo (default reka18/codesmith)
#   CODESMITH_BUNDLE_ID       code-signing identifier (default codesmith)
#   CODESMITH_MINISIGN_KEY    minisign secret key (default ~/.minisign/codesmith.key)
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

[ "$(uname -s)" = Darwin ] || die "run this on macOS"
[ -n "$identity" ] || die "set CODESMITH_SIGN_IDENTITY"
[ "$dry" = 1 ] || [ -n "$profile" ] || die "set CODESMITH_NOTARY_PROFILE"
for c in gh codesign xcrun ditto shasum tar; do
    command -v "$c" >/dev/null 2>&1 || die "need $c"
done
here=$(cd "$(dirname "$0")" && pwd)
# Before minutes of notarizing: minisign, its secret key and a pinned key.
"$here/sign-release.sh" --check

work=$(mktemp -d -t codesmith-sign)
trap 'rm -rf "$work"' EXIT
trap 'exit 1' INT TERM
say "working in $work"

gh release download "$tag" --repo "$repo" --dir "$work" \
    --pattern SHA256SUMS --pattern 'codesmith-*-apple-darwin.tar.gz'
[ -f "$work/SHA256SUMS" ] || die "release $tag has no SHA256SUMS"

# Refuse to sign anything that doesn't match what CI built.
(cd "$work" && grep -- '-apple-darwin\.tar\.gz$' SHA256SUMS | shasum -a 256 -c -) \
    || die "downloaded tarballs do not match SHA256SUMS"

signed=""
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    name="codesmith-${version}-${target}"
    tarball="$work/${name}.tar.gz"
    [ -f "$tarball" ] || die "missing ${name}.tar.gz on release $tag"

    say "unpacking $name"
    rm -rf "${work:?}/$name"
    tar -xzf "$tarball" -C "$work"
    bin="$work/$name/codesmith"
    [ -f "$bin" ] || die "$name.tar.gz has no codesmith binary"

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
    rm -f "$tarball"
    # COPYFILE_DISABLE keeps macOS from adding ._ AppleDouble files.
    COPYFILE_DISABLE=1 tar -C "$work" -czf "$tarball" "$name"
    signed="$signed $tarball"
done

# Rewrite the macOS lines of SHA256SUMS, keeping the Linux lines as CI made them.
grep -v -- '-apple-darwin\.tar\.gz$' "$work/SHA256SUMS" > "$work/SHA256SUMS.new" || true
for t in $signed; do
    (cd "$work" && shasum -a 256 "$(basename "$t")") >> "$work/SHA256SUMS.new"
done
sort -k 2 "$work/SHA256SUMS.new" > "$work/SHA256SUMS"
say "new SHA256SUMS:"
cat "$work/SHA256SUMS" >&2

# Sign the final SHA256SUMS; codesmith upgrade refuses it unsigned.
"$here/sign-release.sh" "$tag" "$work/SHA256SUMS"

if [ "$dry" = 1 ]; then
    out="./codesmith-signed-$version"
    rm -rf "$out"
    mkdir -p "$out"
    # shellcheck disable=SC2086 # $signed is a space-separated list of paths without spaces
    cp $signed "$work/SHA256SUMS" "$work/SHA256SUMS.minisig" "$out/"
    say "dry run: signed files left in $out, nothing uploaded"
    exit 0
fi

say "uploading to release $tag"
# shellcheck disable=SC2086
gh release upload "$tag" --repo "$repo" --clobber $signed "$work/SHA256SUMS" "$work/SHA256SUMS.minisig"
say "done. Check the release, then publish it: gh release edit $tag --repo $repo --draft=false"
