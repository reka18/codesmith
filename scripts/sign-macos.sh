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
