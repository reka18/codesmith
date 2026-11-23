#!/bin/sh
# Sign, publish and ship a drafted release, in one run on Skeptic-Mac.
#
#   scripts/ship-release.sh v0.2.0
#
# Run it after `scripts/release.sh` has built the release and the draft
# exists on GitHub. It works the same from the Mac's own terminal or over
# SSH, and it can be run again after a failure: each step that is already
# done is skipped.
#
#   1. Checks everything first: unlocks the login keychain if it is locked
#      (it is over SSH; you are asked for the Mac's login password), checks
#      that `gh` is signed in as an account that can see the draft, finds
#      the Developer ID identity and its Team ID, and checks the notarytool
#      profile and the minisign key.
#   2. Signs and notarizes the macOS binaries (scripts/sign-macos.sh),
#      unless the draft's macOS checksums already differ from the built ones.
#   3. Signs SHA256SUMS with minisign (scripts/sign-release.sh; minisign asks
#      for the key's password), unless the draft already has the signature.
#   4. Asks before publishing the draft.
#   5. Points the Homebrew tap's formula at the published release, as a
#      signed commit.
#
# The BUILT-SHA256SUMS hash defaults to the one of dist/<version>/ on this
# Mac, which is where release.sh built it. Building elsewhere, pass the hash
# release.sh printed there as CODESMITH_BUILT_SUMS_SHA256 instead.
#
# Environment (all optional):
#   CODESMITH_BUILT_SUMS_SHA256  hash of BUILT-SHA256SUMS (see above)
#   CODESMITH_SIGN_IDENTITY      codesign identity (default: the only
#                                "Developer ID Application" in the keychain)
#   CODESMITH_NOTARY_PROFILE     notarytool profile (default codesmith-notary)
#   CODESMITH_REPO               GitHub repo (default reka18/codesmith)
#   CODESMITH_TAP                Homebrew tap repo (default reka18/homebrew-tap)
#   CODESMITH_YES=1              publish without asking

set -eu

say() { printf 'ship-release: %s\n' "$*" >&2; }
die() { printf 'ship-release: error: %s\n' "$*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*" >&2; }

[ $# -eq 1 ] || die "usage: scripts/ship-release.sh vX.Y.Z"
tag=$1
case "$tag" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) die "tag must look like v0.2.0, not '$tag'" ;;
esac
version=${tag#v}

root=$(git rev-parse --show-toplevel 2>/dev/null) || die "run this from the codesmith checkout"
cd "$root"
repo=${CODESMITH_REPO:-reka18/codesmith}
tap=${CODESMITH_TAP:-reka18/homebrew-tap}
profile=${CODESMITH_NOTARY_PROFILE:-codesmith-notary}
dist="$root/dist/$version"

# ------------------------------------------------------------- pre-flight

step "pre-flight"

# Over SSH the login keychain is locked, so gh's token, the signing key and
# the notary profile are all out of reach until it is unlocked.
keychain="$HOME/Library/Keychains/login.keychain-db"
if ! security show-keychain-info "$keychain" >/dev/null 2>&1; then
    say "the login keychain is locked; unlocking it (this Mac's login password)"
    security unlock-keychain "$keychain" || die "could not unlock $keychain"
fi
say "keychain: unlocked"

command -v gh >/dev/null 2>&1 || die "gh is not installed (brew install gh)"
draft=$(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft 2>/dev/null) || {
    gh auth status >&2 2>&1 || true
    die "gh cannot see release $tag on $repo. Drafts are visible only to accounts with write access: sign in or switch to one (gh auth switch --user <account>), or create the draft first (RELEASING.md)"
}
say "release $tag: draft=$draft"

if [ "$draft" = true ]; then
    identity=${CODESMITH_SIGN_IDENTITY:-}
    if [ -z "$identity" ]; then
        ids=$(security find-identity -v -p codesigning \
            | sed -n 's/.*"\(Developer ID Application: .*\)"$/\1/p' | sort -u)
        case $(printf '%s\n' "$ids" | grep -c .) in
            0) die "no Developer ID Application identity in the keychain (RELEASING.md, one-time setup)" ;;
            1) identity=$ids ;;
            *) die "several Developer ID Application identities; pick one with CODESMITH_SIGN_IDENTITY:
$ids" ;;
        esac
    fi
    team=$(printf '%s\n' "$identity" | sed -n 's/.*(\([0-9A-Z]\{10\}\))$/\1/p')
    [ -n "$team" ] || die "cannot read a Team ID from identity '$identity'"
    say "identity: $identity"

    built_sha=${CODESMITH_BUILT_SUMS_SHA256:-}
    if [ -z "$built_sha" ]; then
        [ -f "$dist/BUILT-SHA256SUMS" ] \
            || die "no $dist/BUILT-SHA256SUMS; set CODESMITH_BUILT_SUMS_SHA256 to the hash release.sh printed"
        built_sha=$(shasum -a 256 "$dist/BUILT-SHA256SUMS" | cut -c1-64)
        say "built manifest: $built_sha (from dist/$version)"
    else
        say "built manifest: $built_sha (from CODESMITH_BUILT_SUMS_SHA256)"
    fi

    assets=$(gh release view "$tag" --repo "$repo" --json assets --jq '.assets[].name')
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    # sign-macos.sh rewrites the macOS lines of SHA256SUMS; while the draft's
    # copy is still the built one, the binaries are unsigned.
    gh release download "$tag" --repo "$repo" --pattern SHA256SUMS --dir "$work" \
        || die "cannot download SHA256SUMS from the draft"
    if [ -f "$dist/SHA256SUMS" ]; then
        if cmp -s "$work/SHA256SUMS" "$dist/SHA256SUMS"; then need_macos=1; else need_macos=0; fi
    elif printf '%s\n' "$assets" | grep -qx 'SHA256SUMS.minisig'; then
        need_macos=0
    else
        die "no $dist/SHA256SUMS to tell whether the macOS binaries are signed yet"
    fi
    if printf '%s\n' "$assets" | grep -qx 'SHA256SUMS.minisig'; then need_minisig=0; else need_minisig=1; fi

    if [ "$need_macos" = 1 ]; then
        xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1 \
            || die "notarytool profile '$profile' is missing or its credentials are rejected. Create it once with:
  xcrun notarytool store-credentials $profile --apple-id <Apple ID> --team-id $team --password <app-specific password>"
        say "notary profile: $profile"
    fi
    if [ "$need_minisig" = 1 ]; then
        scripts/sign-release.sh --check >/dev/null || die "the minisign check failed (scripts/sign-release.sh --check)"
        say "minisign key: ok"
    fi

    # -------------------------------------------------------- signing
    if [ "$need_macos" = 1 ]; then
        step "sign and notarize the macOS binaries"
        CODESMITH_BUILT_SUMS_SHA256=$built_sha CODESMITH_SIGN_IDENTITY=$identity \
            CODESMITH_NOTARY_PROFILE=$profile CODESMITH_REPO=$repo \
            scripts/sign-macos.sh "$tag"
    else
        say "macOS binaries: already signed on the draft; skipping"
    fi

    if [ "$need_minisig" = 1 ]; then
        step "sign SHA256SUMS (minisign asks for the key's password)"
        CODESMITH_BUILT_SUMS_SHA256=$built_sha CODESMITH_TEAM_ID=$team CODESMITH_REPO=$repo \
            scripts/sign-release.sh "$tag"
    else
        say "SHA256SUMS.minisig: already on the draft; skipping"
    fi

    # -------------------------------------------------------- publish
    step "publish"
    if [ "${CODESMITH_YES:-}" != 1 ]; then
        printf 'Publish %s to everyone now? [y/N] ' "$tag" >&2
        read -r answer || answer=
        case "$answer" in
            y | Y | yes | YES) ;;
            *) say "left $tag as a draft. Run this again to publish it."; exit 0 ;;
        esac
    fi
    gh release edit "$tag" --repo "$repo" --draft=false >/dev/null
    say "published https://github.com/$repo/releases/tag/$tag"
else
    say "already published; going on to the Homebrew tap"
fi

# ------------------------------------------------------------- homebrew

step "Homebrew tap $tap"
# The tap commit is signed like this repository's commits.
name=$(git config user.name) || die "git user.name is not set in $root"
email=$(git config user.email) || die "git user.email is not set in $root"
key=$(git config user.signingkey || true)

tapdir=$(mktemp -d)
gh repo clone "$tap" "$tapdir" -- --quiet || die "cannot clone $tap"
scripts/update-homebrew.sh "$version" > "$tapdir/Formula/codesmith.rb.new" \
    || die "update-homebrew.sh failed"
mv "$tapdir/Formula/codesmith.rb.new" "$tapdir/Formula/codesmith.rb"
if git -C "$tapdir" diff --quiet; then
    say "the formula is already at $version"
else
    set -- -c "user.name=$name" -c "user.email=$email" -c commit.gpgsign=true
    [ -z "$key" ] || set -- "$@" -c "user.signingkey=$key"
    git -C "$tapdir" "$@" commit -q -am "codesmith $version"
    git -C "$tapdir" push -q origin HEAD
    say "formula updated to $version"
fi
rm -rf "$tapdir"

step "done"
say "upgrade with:  brew update && brew upgrade codesmith"
