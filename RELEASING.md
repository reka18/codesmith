# Releasing CodeSmith

There is no hosted CI and no GitHub Actions: everything is built and checked
on our own machines, and GitHub only hosts the code and the release page.
`install.sh` and Homebrew download the tarballs from that page, and macOS
signing happens on the owner's own Mac.

## Checks before every push

`scripts/ci.sh` is the CI: `cargo fmt --check`, `cargo clippy --all-targets
--workspace -D warnings`, `cargo test --workspace` and `shellcheck`. Install the
hooks once per clone with `scripts/install-hooks.sh`; the pre-push hook runs
`scripts/ci.sh` and a failure stops the push.

## What a release contains

`codesmith` for four targets:

| Target | Built on | Notes |
|---|---|---|
| `aarch64-apple-darwin` | an Apple Silicon Mac | native |
| `x86_64-apple-darwin` | the same Mac | cross-compiled with Xcode's SDK |
| `x86_64-unknown-linux-musl` | a Linux machine, or a cross toolchain | static |
| `aarch64-unknown-linux-musl` | a Linux machine, or a cross toolchain | static |

Linux binaries are static musl builds so they have no glibc version floor
(a glibc build from Ubuntu 24.04 would need glibc 2.39 and fail on Debian 12,
RHEL 9 and older LTS releases), and musl also covers Alpine. CodeSmith has no
native dependencies beyond bundled C (SQLite, ring, zstd), which build with
`musl-gcc`.

Each build is packaged as `codesmith-<version>-<target>.tar.gz`, containing a
`codesmith-<version>-<target>/` directory with the binary, `LICENSE`,
`README.md`, `CHANGELOG.md` and the `docs/` guide, plus a `SHA256SUMS` file.
Releases are created as **drafts** with `gh release create --draft` and stay
drafts until the macOS binaries are signed, so nobody downloads an unsigned
build from `/releases/latest`.

Building and packaging the four targets is currently manual; a local release
script is still to be written.

## Signing the macOS binaries

The Developer ID private key never leaves the signing Mac. `scripts/sign-macos.sh`
runs on that Mac once the draft release exists. It:

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
   Run `scripts/ci.sh`. Add the release's section to `CHANGELOG.md`.
   Commit: `Release 0.2.0`.
2. **Tag and push.** Check that the tag matches the version.

   ```sh
   git tag -s v0.2.0 -m "CodeSmith 0.2.0"
   git push origin main v0.2.0
   ```

3. **Build and upload** the four tarballs and `SHA256SUMS` (see "What a
   release contains"), then `gh release create v0.2.0 --draft` with them
   attached.
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
