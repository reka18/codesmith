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

`scripts/ci-linux.sh` runs the same checks on Linux in Docker (linux/arm64;
`--amd64` for x86_64 under emulation), which catches Linux-only `cfg` code and
lints the Mac run never compiles. It is slow, so the hook runs it only with
`CODESMITH_CI_LINUX=1` set (`CODESMITH_CI_LINUX=1 git push`); run it by hand
before a release or after touching platform-specific code. The repo is mounted
read-only, cargo's registry and target dir are cached in `codesmith-ci-*`
Docker volumes, and the checks run as a non-root user with `--network none`
under CPU, memory and pids limits (`CI_LINUX_CPUS`, `CI_LINUX_MEMORY`,
`CI_LINUX_PIDS`, `CI_LINUX_TIMEOUT`).

## What a release contains

`codesmith` for four targets, all built on one Apple Silicon Mac by
`scripts/release.sh`:

| Target | Built with | Notes |
|---|---|---|
| `aarch64-apple-darwin` | `cargo build` | native |
| `x86_64-apple-darwin` | `cargo build` | cross-compiled with Xcode's SDK; smoke-tested under Rosetta |
| `aarch64-unknown-linux-musl` | `cargo zigbuild` | static |
| `x86_64-unknown-linux-musl` | `cargo zigbuild` | static |

Linux binaries are static musl builds so they have no glibc version floor
(a glibc build from Ubuntu 24.04 would need glibc 2.39 and fail on Debian 12,
RHEL 9 and older LTS releases), and musl also covers Alpine. CodeSmith has no
native dependencies beyond bundled C (SQLite, ring, zstd), which
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) compiles and
links with zig against musl. (Homebrew's musl-cross toolchains would also
work, but zig is one formula for both architectures and needs no per-target
`CC_*`/linker settings.) The macOS binaries target macOS 11 and later
(`MACOSX_DEPLOYMENT_TARGET`).

Each build is packaged as `codesmith-<version>-<target>.tar.gz`, containing a
`codesmith-<version>-<target>/` directory with the binary, `LICENSE`,
`README.md`, `CHANGELOG.md` and the `docs/` guide, plus a `SHA256SUMS` file.
The text files and `docs/` are taken from `HEAD` with `git archive`, so only
committed, tracked files ship; an ignored or uncommitted file in the working
tree never does (a dry run with local edits packages the committed docs).

`SHA256SUMS` is signed with [minisign](https://jedisct1.github.io/minisign/)
on the signing Mac (`SHA256SUMS.minisig`, see "Signing the checksums"), and
`codesmith upgrade` checks that signature against a public key compiled into
the binary before it uses any checksum. Without the signature, `SHA256SUMS`
would come from the same release as the tarballs, so anyone able to replace
both could ship a bad binary with a matching checksum.

**Known limitations:** the signature proves a release was signed with the
key on the signing Mac; it can't help if that key is stolen (rotate it,
below). `install.sh` checks it only when `minisign` is installed on the
user's machine, and fetches its keys from the same repository it is served
from. The Homebrew formula pins the sha256 values `update-homebrew.sh` copied
from the signed `SHA256SUMS` (check the signature before running it), and
Homebrew checks those, not the signature. Builds from before the first key
was generated check checksums only, and upgrade the same way into the first
signed release.
Releases are created as **drafts** with `gh release create --draft` and stay
drafts until the macOS binaries are signed, so nobody downloads an unsigned
build from `/releases/latest` (which is also what `codesmith upgrade` reads).

## Building a release

One-time setup on the build Mac (Xcode or its command line tools installed):

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin \
  aarch64-unknown-linux-musl x86_64-unknown-linux-musl
brew install zig
cargo install --locked cargo-zigbuild
softwareupdate --install-rosetta   # optional: runs the x86_64 macOS binary
brew install gh && gh auth login   # for --draft-upload
```

Docker Desktop is optional; with it, each Linux binary is also run in Alpine
and Debian containers of its own architecture. The script names whatever is
missing.

```sh
scripts/release.sh 0.2.0                  # dry run: build, test, package; no GitHub
scripts/release.sh 0.2.0 --draft-upload   # the same, then create the draft release
```

It checks the version against `Cargo.toml`, builds the four targets, and
smoke-tests them: `--version` for the macOS binaries, `file` must call the
Linux ones static, and with Docker they run in containers (with no network).
It then packages into `dist/<version>/`, writes `SHA256SUMS`, renders the
Homebrew formula from it, and installs the Apple Silicon tarball with
`install.sh` from `dist/` to check that path too. A dry run stops there and
prints the `gh` command it would run, without any call to GitHub (`--dry-run`
is the default and also wins over `--draft-upload`); `--targets
"aarch64-apple-darwin"` narrows a dry run to some targets. Without Rosetta
the x86_64 macOS binary is only checked with `file`, and without Docker the
Linux binaries are only checked with `file`; the script says which checks it
skipped. `CODESMITH_SKIP_DOCKER=1` skips the containers on purpose. `--draft-upload` also needs a clean tree
at the tag `v<version>`, pushed (`--verify-tag`), and runs
`gh release create v<version> --draft --verify-tag --generate-notes` with the
tarballs and `SHA256SUMS`.

`--draft-upload` refuses while `crates/codesmith/src/release_key.pub` holds
no release key (only `PLACEHOLDER`): a draft is uploaded only when it can be
signed, and it stays an unsigned draft until `sign-macos.sh` has signed it.
A dry run only notes the missing key.

## Signing the macOS binaries

The Developer ID private key never leaves the signing Mac. `scripts/sign-macos.sh`
runs on that Mac once the draft release exists. It:

1. downloads the draft release's macOS tarballs and `SHA256SUMS` with `gh`, and
   refuses to continue unless they match;
2. signs each binary with `codesign --options runtime --timestamp` (hardened
   runtime and secure timestamp, both required for notarization);
3. zips each signed binary with `ditto` and submits it with
   `xcrun notarytool submit --wait`;
4. re-tars the signed binary and rewrites the macOS lines of `SHA256SUMS`;
5. signs the final `SHA256SUMS` with minisign (`scripts/sign-release.sh`,
   below), and checks the signature against `release_key.pub`;
6. uploads the tarballs, `SHA256SUMS` and `SHA256SUMS.minisig` together with
   `gh release upload --clobber`.

It checks for minisign and the secret key before it starts, so a missing key
fails before minutes of notarizing, not after.

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
brew install minisign   # and generate the key: "Generating the key (once)"
```

`CODESMITH_DRY_RUN=1` signs without notarizing or uploading and leaves the
results in `./codesmith-signed-<version>/` (with `SHA256SUMS.minisig`),
which is handy for checking the identity and the minisign key work.

## Signing the checksums

The minisign secret key, like the Developer ID key, never leaves the signing
Mac. It is a password-protected file there, outside every repository; only
the public key is committed.

`scripts/sign-release.sh` signs a `SHA256SUMS`:

```sh
scripts/sign-release.sh v0.2.0 path/to/SHA256SUMS   # writes SHA256SUMS.minisig next to it
scripts/sign-release.sh v0.2.0                       # downloads the draft's SHA256SUMS, signs, uploads the .minisig
```

`sign-macos.sh` runs the first form itself, so a normal release needs nothing
more. The second form is for re-signing a draft whose `SHA256SUMS` is final.
It runs `minisign -S` with the trusted comment `codesmith <version>
SHA256SUMS timestamp:<UTC time>` (minisign asks for the key's password),
then checks what `codesmith upgrade` checks: the signature is prehashed
(minisign's default), verifies with a key in `release_key.pub`, and its
trusted comment names the version. minisign signs the trusted comment as
well, and `codesmith upgrade` and `install.sh` refuse a comment that names
another version, so an older release's signed `SHA256SUMS` can't be replayed
as a newer or older one. The key defaults to `~/.minisign/codesmith.key`;
set `CODESMITH_MINISIGN_KEY` for another path. The script refuses a key
inside the repository.

### Generating the key (once)

On the signing Mac:

```sh
brew install minisign
mkdir -p ~/.minisign && chmod 700 ~/.minisign
minisign -G -p ~/.minisign/codesmith.pub -s ~/.minisign/codesmith.key
chmod 600 ~/.minisign/codesmith.key
```

`minisign -G` asks for a password for the secret key: use a strong one and
keep it in a password manager. Don't pass `-W`, which leaves the key
unencrypted. Keep an encrypted offline backup of `codesmith.key` (and the
password) somewhere safe: without it, releases can't be signed for builds
that trust only this key (see "Rotating the key").

Then put the public key in the repo. It is the second line of
`codesmith.pub` (56 characters starting with `RW`):

1. In `crates/codesmith/src/release_key.pub`, replace the `PLACEHOLDER` line
   with that line.
2. In `install.sh`, set `RELEASE_KEYS="<that line>"`.
3. Run `scripts/ci.sh`; a test fails unless the two lists match, and another
   unless every line of `release_key.pub` is a valid key.
4. Commit and push. Builds from then on refuse an unsigned release.

### The transition to signed releases

While `release_key.pub` holds `PLACEHOLDER`, builds check `SHA256SUMS` only
and `codesmith upgrade` prints `warning: CodeSmith releases aren't signed
yet; checking SHA256SUMS only`; `install.sh` says the same. Once a real key
is compiled in, a build refuses any release whose `SHA256SUMS.minisig` is
missing, doesn't verify, or names another version, and there is no flag to
override that. So:

- Generate the key and commit its public key before the release that should
  be the first signed one; `release.sh --draft-upload` refuses until then.
- Builds made before the key (with `PLACEHOLDER`) upgrade into a signed
  release with checksums only; their next upgrade checks the signature.
- A signed build refuses to roll back to a release from before signing
  began, since it has no signature; install that one with `install.sh` or by
  hand.

### Rotating the key

A build trusts the keys it was compiled with, and a release carries one
signature. To move to a new key:

1. Generate the new key as above, under another name
   (`~/.minisign/codesmith-2.key`).
2. Add its public key as a second line of `release_key.pub` and to
   `RELEASE_KEYS` in `install.sh` (space-separated), and release as usual,
   still signing with the old key. That release trusts both.
3. After that release has been out long enough that users have it, sign
   with the new key (`CODESMITH_MINISIGN_KEY=~/.minisign/codesmith-2.key`)
   and, in a later release, drop the old key from both lists.

A build older than step 2 trusts only the old key and refuses releases
signed with the new one; its users reinstall with `install.sh`. If the old
key is compromised, skip the overlap: remove it from both lists at once,
sign with the new key, and tell users to reinstall, since builds that trust
the old key can't tell its signatures from yours.

## Checklist

In this order. A release is a draft, and unsigned, from step 4 until step 5
finishes; it is published only in step 7.

0. **Once:** generate the minisign key and commit its public key
   ("Generating the key"). `release.sh --draft-upload` refuses without it.
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

3. **Dry run:** `scripts/release.sh 0.2.0` (see "Building a release").
4. **Build and upload the draft:** `scripts/release.sh 0.2.0 --draft-upload`.
   The draft has the tarballs and an unsigned `SHA256SUMS`.
5. **Sign on the signing Mac**, at a checkout of the tag (the script checks
   the signature against that checkout's `release_key.pub`):

   ```sh
   CODESMITH_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
   CODESMITH_NOTARY_PROFILE=codesmith-notary \
   scripts/sign-macos.sh v0.2.0
   ```

   This signs and notarizes the macOS binaries, rewrites `SHA256SUMS`, signs
   it (minisign asks for the key's password) and uploads the tarballs,
   `SHA256SUMS` and `SHA256SUMS.minisig`.
6. **Check the signature** on the draft before publishing:

   ```sh
   gh release download v0.2.0 --repo reka18/codesmith --dir /tmp/v0.2.0 \
     --pattern SHA256SUMS --pattern SHA256SUMS.minisig
   minisign -V -p ~/.minisign/codesmith.pub -m /tmp/v0.2.0/SHA256SUMS
   ```

   The trusted comment it prints must be `codesmith 0.2.0 SHA256SUMS ...`.
7. **Review and publish** the draft: edit the generated notes, then
   `gh release edit v0.2.0 --draft=false`.
8. **Check an install:** `CODESMITH_VERSION=0.2.0 sh install.sh` on a Mac and a
   Linux box, with minisign installed (it should print `signature ok`); on the
   Mac, `codesign -dv --verbose=2 ~/.local/bin/codesmith`
   should show your Developer ID and `flags=0x10000(runtime)`, and
   `xcrun notarytool history --keychain-profile codesmith-notary` should list
   both submissions as Accepted. From an older install, `codesmith upgrade
   0.2.0` should succeed.
9. **Update the Homebrew tap** (below).

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
