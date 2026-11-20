#!/bin/sh
# Install CodeSmith from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/reka18/codesmith/main/install.sh | sh
#
# Environment:
#   CODESMITH_VERSION      version to install, e.g. 0.1.0 (default: latest release)
#   CODESMITH_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   CODESMITH_BASE_URL     where release files live (default: GitHub Releases;
#                          useful for mirrors and testing)
#
# This script only downloads the release tarball and its checksum file. It
# sends nothing else anywhere and installs nothing outside the install dir.
set -eu

REPO="reka18/codesmith"
INSTALL_DIR="${CODESMITH_INSTALL_DIR:-${HOME}/.local/bin}"

say() { printf 'codesmith-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "need '$1' (command not found)"; }

# fetch URL DEST
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https,file,http' -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        die "need curl or wget"
    fi
}

# The tag GitHub's /releases/latest redirects to, without the leading v.
latest_version() {
    url="https://github.com/${REPO}/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        final=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$url") || return 1
    else
        final=$(wget -q -S --spider "$url" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1 | tr -d '\r') || return 1
    fi
    tag=${final##*/}
    tag=${tag#v}
    case "$tag" in
        [0-9]*) printf '%s\n' "$tag" ;;
        *) return 1 ;;
    esac
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | sed 's/.*= *//'
    else
        die "need sha256sum, shasum or openssl to verify the download"
    fi
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) die "unsupported architecture: $arch" ;;
    esac
    case "$os" in
        Darwin)
            # A shell under Rosetta reports x86_64 on Apple Silicon; prefer native.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
                arch=aarch64
            fi
            printf '%s-apple-darwin\n' "$arch"
            ;;
        Linux) printf '%s-unknown-linux-musl\n' "$arch" ;;
        *) die "unsupported OS: $os (build from source: cargo install --git https://github.com/${REPO} codesmith)" ;;
    esac
}

main() {
    need uname
    need tar
    need mkdir

    target=$(detect_target)
    version="${CODESMITH_VERSION:-}"
    version="${version#v}"
    if [ -z "$version" ]; then
        version=$(latest_version) || die "could not find the latest release; set CODESMITH_VERSION"
    fi
    base="${CODESMITH_BASE_URL:-https://github.com/${REPO}/releases/download/v${version}}"
    name="codesmith-${version}-${target}"
    file="${name}.tar.gz"

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t codesmith)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' INT TERM

    say "downloading ${file}"
    fetch "${base}/${file}" "${tmp}/${file}" || die "download failed: ${base}/${file}"
    fetch "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" || die "download failed: ${base}/SHA256SUMS"

    expected=$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "${tmp}/SHA256SUMS")
    [ -n "$expected" ] || die "${file} is not listed in SHA256SUMS"
    actual=$(sha256_of "${tmp}/${file}")
    [ "$expected" = "$actual" ] || die "checksum mismatch for ${file} (expected ${expected}, got ${actual})"
    say "checksum ok"

    tar -xzf "${tmp}/${file}" -C "$tmp"
    [ -f "${tmp}/${name}/codesmith" ] || die "archive does not contain ${name}/codesmith"

    mkdir -p "$INSTALL_DIR"
    # Copy then rename so a running codesmith is never left half-written.
    cp "${tmp}/${name}/codesmith" "${INSTALL_DIR}/.codesmith.new"
    chmod 755 "${INSTALL_DIR}/.codesmith.new"
    mv -f "${INSTALL_DIR}/.codesmith.new" "${INSTALL_DIR}/codesmith"
    say "installed codesmith ${version} to ${INSTALL_DIR}/codesmith"

    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *) say "note: ${INSTALL_DIR} is not on your PATH; add it in your shell profile" ;;
    esac
}

main "$@"
