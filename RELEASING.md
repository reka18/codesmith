# Releasing CodeSmith

Releases are GitHub Releases built by GitHub Actions. There are no servers of
our own: the release page hosts the tarballs, `install.sh` and Homebrew
download from it, and macOS signing happens on the owner's own Mac.

## What gets built

`.github/workflows/release.yml` runs when a `v*` tag is pushed. It builds
`codesmith` for:

| Target | Runner | Notes |
|---|---|---|
| `aarch64-apple-darwin` | `macos-15` (Apple Silicon) | native |
| `x86_64-apple-darwin` | `macos-15` | cross-compiled with Xcode's SDK; smoke-tested under Rosetta |
| `x86_64-unknown-linux-musl` | `ubuntu-24.04` | static, native |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | static, native |

Linux binaries are static musl builds so they have no glibc version floor: a
glibc build made on Ubuntu 24.04 would need glibc 2.39 and fail on Debian 12,
RHEL 9 and older LTS releases, and musl also covers Alpine. CodeSmith has no
native dependencies beyond bundled C (SQLite, ring), which build with
`musl-gcc`. The cost is musl's slower allocator, which doesn't matter for an
agent that mostly waits on the network.

Each build is packaged as `codesmith-<version>-<target>.tar.gz`, containing a
`codesmith-<version>-<target>/` directory with the binary, `LICENSE` and
`README.md`. The final job writes `SHA256SUMS` and creates a **draft** release
with everything attached. It stays a draft until the macOS binaries are signed,
so nobody downloads an unsigned build from `/releases/latest`.

The workflow uses only `gh` and GitHub's own `actions/checkout`,
`actions/upload-artifact` and `actions/download-artifact`, pinned to commit
SHAs. Rust comes from the runner's preinstalled `rustup`.

## Signing without secrets in CI

The Developer ID private key never leaves the signing Mac, and CI holds no
Apple credentials. Instead, `scripts/sign-macos.sh` runs on that Mac after CI
finishes. It:

1. downloads the draft release's macOS tarballs and `SHA256SUMS` with `gh`, and
   refuses to continue unless they match;
2. signs each binary with `codesign --options runtime --timestamp` (hardened
   runtime and secure timestamp, both required for notarization);
3. zips each signed binary with `ditto` and submits it with
   `xcrun notarytool submit --wait`;
4. re-tars the signed binary, rewrites the macOS lines of `SHA256SUMS`, and
   uploads both with `gh release upload --clobber`.

There is nothing to staple: `stapler` only handles `.app`, `.pkg` and `.dmg`,
not a bare executable, and the zip is only a vehicle for submission. Gatekeeper
looks the ticket up online the first time it checks a quarantined copy.
Downloads through `curl`, `install.sh` and Homebrew aren't quarantined, so for
them the signature is what matters.

One-time setup on the signing Mac:

```sh
# The Developer ID Application identity must be in the login keychain:
security find-identity -v -p codesigning
# Store notarization credentials (an app-specific password or an API key):
xcrun notarytool store-credentials codesmith-notary \
  --apple-id you@example.com --team-id TEAMID
gh auth login   # with write access to reka18/codesmith
```

`CODESMITH_DRY_RUN=1` signs without notarizing or uploading and leaves the
results in `./codesmith-signed-<version>/`, which is handy for checking the
identity works.

## Checklist

1. **Bump the version** in `[workspace.package]` in `Cargo.toml` (and the
   `version` on the `codesmith-ai`/`codesmith-core` entries under
   `[workspace.dependencies]`), then run `cargo check` so `Cargo.lock` follows.
   Run `cargo fmt --all --check`, `cargo clippy --all-targets --workspace` and
   `cargo test --workspace`. Commit: `Release 0.2.0`.
2. **Tag and push.** The workflow fails if the tag doesn't match the version.

   ```sh
   git tag -s v0.2.0 -m "CodeSmith 0.2.0"
   git push origin main v0.2.0
   ```

3. **Wait for the Release workflow** to finish (`gh run watch`). A draft
   release `v0.2.0` now has four tarballs and `SHA256SUMS`.
4. **Sign on the signing Mac:**

   ```sh
   CODESMITH_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
   CODESMITH_NOTARY_PROFILE=codesmith-notary \
   scripts/sign-macos.sh v0.2.0
   ```

5. **Review and publish** the draft: edit the generated notes, then
   `gh release edit v0.2.0 --draft=false`.
6. **Check an install:** `CODESMITH_VERSION=0.2.0 sh install.sh` on a Mac and a
   Linux box; on the Mac, `codesign -dv --verbose=2 ~/.local/bin/codesmith`
   should show your Developer ID and `flags=0x10000(runtime)`, and
   `xcrun notarytool history --keychain-profile codesmith-notary` should list
   both submissions as Accepted.
7. **Update the Homebrew tap** (below).

## Homebrew tap

`packaging/homebrew/codesmith.rb` is a template for a formula that installs the
prebuilt tarballs. `scripts/update-homebrew.sh` fills in the version and the
four sha256 values from a release's `SHA256SUMS` (downloaded with `gh`, or
passed as a file). Run it after signing, since signing changes the macOS
checksums.

The tap is a separate public repo, `reka18/homebrew-tap`, which Homebrew finds
as `reka18/tap`. To set it up once:

```sh
gh repo create reka18/homebrew-tap --public
git clone git@github.com:reka18/homebrew-tap.git
mkdir -p homebrew-tap/Formula
```

For each release:

```sh
scripts/update-homebrew.sh 0.2.0 "" ../homebrew-tap/Formula/codesmith.rb
cd ../homebrew-tap
git add Formula/codesmith.rb && git commit -m "codesmith 0.2.0" && git push
brew update && brew upgrade reka18/tap/codesmith || brew install reka18/tap/codesmith
brew test reka18/tap/codesmith
brew audit --strict reka18/tap/codesmith   # optional
```

Users then run `brew install reka18/tap/codesmith`, and `brew upgrade` picks up
new versions.

## crates.io

The crates have the metadata crates.io needs (description, license, readme,
keywords, categories, and versions on the internal path dependencies), but
nothing is published. If that changes, publish in dependency order:
`codesmith-ai`, `codesmith-core`, then `codesmith`.
