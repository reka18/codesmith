#!/bin/sh
# Build, smoke-test and package a CodeSmith release on this Mac, and with
# --draft-upload create the draft GitHub release. See RELEASING.md.
#
#   scripts/release.sh VERSION [--dry-run | --draft-upload] [--targets "T1 T2"]
#
# VERSION is e.g. 0.2.0 or v0.2.0 and must match [workspace.package] in
# Cargo.toml. The four targets:
#
#   aarch64-apple-darwin, x86_64-apple-darwin   cargo build, with Xcode's SDK
#   aarch64-unknown-linux-musl,
#   x86_64-unknown-linux-musl                   cargo zigbuild: static, with zig
#                                               as the C compiler and linker
#                                               (bundled SQLite, ring, zstd)
#
# Results go to dist/<version>/: codesmith-<version>-<target>.tar.gz, each
# holding codesmith-<version>-<target>/ with the binary, LICENSE, README.md,
# CHANGELOG.md and docs/, plus SHA256SUMS and the rendered Homebrew formula.
#
#   --dry-run       build, smoke-test and package, then print the gh command
#                   instead of running it. Never talks to GitHub. (Default;
#                   it also overrides --draft-upload.)
#   --draft-upload  also run `gh release create vVERSION --draft --verify-tag`
#                   with the tarballs and SHA256SUMS. Needs a clean tree at
#                   the pushed tag vVERSION.
#   --targets "..." build only these targets (smoke tests and packaging too);
#                   only allowed with --dry-run.
#
# Environment:
#   CODESMITH_REPO               GitHub repo (default reka18/codesmith)
#   MACOSX_DEPLOYMENT_TARGET     oldest macOS the binaries run on (default 11.0)
#   CODESMITH_SKIP_DOCKER=1      don't run the Linux binaries in containers
#
# One-time setup (Apple Silicon Mac with Xcode or its command line tools):
#
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin \
#     aarch64-unknown-linux-musl x86_64-unknown-linux-musl
#   brew install zig
#   cargo install --locked cargo-zigbuild
#   brew install gh && gh auth login   # only for --draft-upload
#
# Docker Desktop is optional: with it, each Linux binary also runs in Alpine
# and Debian containers of its architecture.
set -eu

die() { printf 'release: error: %s\n' "$*" >&2; exit 1; }
say() { printf '\n== %s\n' "$*" >&2; }

usage="usage: $0 VERSION [--dry-run | --draft-upload] [--targets \"T1 T2\"]"
[ $# -ge 1 ] || die "$usage"
version="${1#v}"
shift
mode="dry-run"
dry=no
all="aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl"
targets="$all"
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry=yes ;;
        --draft-upload) mode=upload ;;
        --targets)
            [ $# -ge 2 ] || die "--targets needs a list"
            targets="$2"
            shift
            ;;
        *) die "$usage" ;;
    esac
    shift
done
# --dry-run wins over --draft-upload, whatever the order.
[ "$dry" = yes ] && mode="dry-run"
if [ "$mode" = upload ] && [ "$targets" != "$all" ]; then
    die "--targets is only for dry runs; a release has all four targets"
fi
for t in $targets; do
    case " $all " in
        *" $t "*) ;;
        *) die "unknown target: $t (expected one of: $all)" ;;
    esac
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
repo="${CODESMITH_REPO:-reka18/codesmith}"
tag="v${version}"
dist="${root}/dist/${version}"
target_dir="${CARGO_TARGET_DIR:-${root}/target}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"

# ---------------------------------------------------------------- preflight

[ "$(uname -s)" = Darwin ] || die "release.sh runs on macOS (it builds the macOS binaries natively)"
crate_version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[ -n "$crate_version" ] || die "no version in [workspace.package] in Cargo.toml"
[ "$version" = "$crate_version" ] || die "version $version does not match Cargo.toml's $crate_version"

missing=""
need() { command -v "$1" >/dev/null 2>&1 || missing="${missing}
  $1: $2"; }
need cargo "install Rust with rustup (https://rustup.rs)"
need rustup "install Rust with rustup (https://rustup.rs)"
need xcrun "install Xcode or run: xcode-select --install"
need shasum "part of macOS"
case "$targets" in
    *linux*)
        need zig "brew install zig"
        cargo zigbuild --help >/dev/null 2>&1 || missing="${missing}
  cargo-zigbuild: cargo install --locked cargo-zigbuild"
        ;;
esac
[ "$mode" = upload ] && need gh "brew install gh && gh auth login"
installed=$(rustup target list --installed 2>/dev/null || true)
for t in $targets; do
    printf '%s\n' "$installed" | grep -qx "$t" || missing="${missing}
  rust target $t: rustup target add $t"
done
[ -z "$missing" ] || die "missing tools (one-time setup):${missing}"
sdk=$(xcrun --sdk macosx --show-sdk-path 2>/dev/null) || die "no macOS SDK: install Xcode or run xcode-select --install"

if [ "$mode" = upload ]; then
    [ -z "$(git status --porcelain)" ] || die "the working tree is not clean"
    git rev-parse -q --verify "refs/tags/${tag}" >/dev/null || die "no tag ${tag}; tag and push it first (RELEASING.md)"
    [ "$(git rev-parse HEAD)" = "$(git rev-parse "${tag}^{commit}")" ] || die "HEAD is not at ${tag}"
    gh auth status >/dev/null 2>&1 || die "gh is not logged in: gh auth login"
fi

# ---------------------------------------------------------------- build

for t in $targets; do
    say "build $t"
    case "$t" in
        *apple-darwin) SDKROOT="$sdk" cargo build --release --locked -p codesmith --target "$t" ;;
        *linux-musl) cargo zigbuild --release --locked -p codesmith --target "$t" ;;
    esac
done

# ---------------------------------------------------------------- smoke tests

docker_ok=no
if [ "${CODESMITH_SKIP_DOCKER:-}" != 1 ] && command -v docker >/dev/null 2>&1 \
    && docker info >/dev/null 2>&1; then
    docker_ok=yes
fi

expect="codesmith ${version}"
check_output() { # TARGET HOW OUTPUT
    [ "$3" = "$expect" ] || die "$1 ($2) printed '$3', expected '$expect'"
    printf '  %s: %s ok\n' "$2" "$3" >&2
}

for t in $targets; do
    say "smoke test $t"
    bin="${target_dir}/${t}/release/codesmith"
    [ -x "$bin" ] || die "$bin was not built"
    file "$bin" >&2
    printf '  size: %s bytes\n' "$(wc -c < "$bin" | tr -d ' ')" >&2
    case "$t" in
        aarch64-apple-darwin)
            check_output "$t" native "$("$bin" --version)"
            ;;
        x86_64-apple-darwin)
            # Rosetta's runtime; /usr/bin tools are arm64e-only, so they
            # can't serve as the probe.
            if [ -e /Library/Apple/usr/libexec/oah/libRosettaRuntime ]; then
                check_output "$t" rosetta "$(arch -x86_64 "$bin" --version)"
            else
                printf '  Rosetta is not installed (softwareupdate --install-rosetta); not running it\n' >&2
            fi
            ;;
        *linux-musl)
            file "$bin" | grep -Eq 'statically linked|static-pie linked' || die "$bin is not statically linked"
            printf '  statically linked: ok\n' >&2
            if [ "$docker_ok" = yes ]; then
                case "$t" in
                    aarch64*) platform=linux/arm64 ;;
                    *) platform=linux/amd64 ;;
                esac
                for image in alpine:3.20 debian:12-slim; do
                    out=$(docker run --rm --network none --platform "$platform" \
                        -v "${target_dir}/${t}/release:/release:ro" "$image" \
                        /release/codesmith --version) || die "$t did not run in $image ($platform)"
                    check_output "$t" "$image $platform" "$out"
                done
            else
                printf '  Docker is not available; not running it\n' >&2
            fi
            ;;
    esac
done

# ---------------------------------------------------------------- package

say "package into dist/${version}"
rm -rf "$dist"
mkdir -p "$dist"
for t in $targets; do
    name="codesmith-${version}-${t}"
    mkdir "${dist}/${name}"
    cp "${target_dir}/${t}/release/codesmith" LICENSE README.md CHANGELOG.md "${dist}/${name}/"
    # The README links into docs/, so ship the guide too.
    cp -R docs "${dist}/${name}/docs"
    # No AppleDouble files, extended attributes or local user names.
    COPYFILE_DISABLE=1 tar -C "$dist" --no-xattrs --no-mac-metadata \
        --uid 0 --gid 0 --uname root --gname root \
        -czf "${dist}/${name}.tar.gz" "$name"
    rm -rf "${dist:?}/${name}"
done
(cd "$dist" && shasum -a 256 codesmith-*.tar.gz > SHA256SUMS)
cat "${dist}/SHA256SUMS" >&2
count=$(wc -l < "${dist}/SHA256SUMS" | tr -d ' ')
for t in $targets; do
    tar -tzf "${dist}/codesmith-${version}-${t}.tar.gz" \
        | grep -qx "codesmith-${version}-${t}/codesmith" || die "the ${t} tarball has no binary"
done

# The formula and install.sh must work with what was built.
if [ "$targets" = "$all" ]; then
    [ "$count" -eq 4 ] || die "expected 4 tarballs, found $count"
    scripts/update-homebrew.sh "$version" "${dist}/SHA256SUMS" "${dist}/codesmith.rb"
    printf '  rendered dist/%s/codesmith.rb\n' "$version" >&2
fi
case " $targets " in
    *" aarch64-apple-darwin "*)
        say "install.sh from dist/${version}"
        bin_dir=$(mktemp -d)
        CODESMITH_VERSION="$version" CODESMITH_BASE_URL="file://${dist}" \
            CODESMITH_INSTALL_DIR="$bin_dir" sh install.sh
        check_output aarch64-apple-darwin install.sh "$("${bin_dir}/codesmith" --version)"
        rm -rf "$bin_dir"
        ;;
esac

# ---------------------------------------------------------------- release

set -- "${dist}"/codesmith-*.tar.gz "${dist}/SHA256SUMS"
if [ "$mode" = dry-run ]; then
    say "dry run: not creating a release. With --draft-upload this runs:"
    printf '  gh release create %s --repo %s --draft --verify-tag --title "CodeSmith %s" --generate-notes \\\n' \
        "$tag" "$repo" "$tag" >&2
    for f in "$@"; do printf '    %s\n' "${f#"${root}/"}" >&2; done
    exit 0
fi

say "create the draft release ${tag}"
gh release create "$tag" "$@" --repo "$repo" --draft --verify-tag \
    --title "CodeSmith ${tag}" --generate-notes
say "done: sign it with scripts/sign-macos.sh ${tag} (RELEASING.md)"
