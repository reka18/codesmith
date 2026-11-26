# Releasing CodeSmith

GitHub Actions runs CI; releases are built and signed on our own machines,
and GitHub hosts the code and the release page. `install.sh` and Homebrew
download the tarballs from that page, and all signing happens on the owner's
own two Macs (see "Two Macs"). The CI workflow gets no secrets and no write
access.

## CI

`scripts/ci.sh` is the CI: `cargo fmt --check`, `cargo clippy --all-targets
--workspace -D warnings`, `cargo test --workspace` and `shellcheck`.
`.github/workflows/ci.yml` runs it on GitHub's hosted runners for every pull
request and every push to `main`: Linux x86_64, Linux arm64 and macOS arm64,
each against the latest stable Rust. A pull request merges only when all
three pass. Release from a commit on `main` whose CI passed.

`scripts/ci-linux.sh` runs the same checks on Linux in Docker (linux/arm64;
`--amd64` for x86_64 under emulation), for checking Linux-only `cfg` code
from a Mac before pushing. The repo is mounted
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
(`SHA256SUMS.minisig`, see "Signing the checksums"), and `codesmith upgrade`
checks that signature against the public key compiled into the binary
(`crates/codesmith/src/release_key.pub`) before it uses any checksum.
`install.sh` pins the same key. Without the signature, `SHA256SUMS` would
come from the same release as the tarballs, so anyone able to replace both
could ship a bad binary with a matching checksum.

**Known limitations:** the signature proves a release was signed with the
minisign key on Skeptic-Mac; it can't help if that key is stolen (rotate it,
below). `install.sh` checks it only when `minisign` is installed on the
user's machine, and fetches its keys from the same repository it is served
from. The Homebrew formula pins the sha256 values `update-homebrew.sh` copied
from the signed `SHA256SUMS` (check the signature before running it), and
Homebrew checks those, not the signature. Builds from before signing began
(their `release_key.pub` held only `PLACEHOLDER`) check checksums only, and
upgrade that way once, into a signed release; from then on they check the
signature.

Releases are created as **drafts** with `gh release create --draft` and stay
drafts until both signing steps are done (the macOS binaries on the mini,
then `SHA256SUMS.minisig` on Skeptic-Mac), so nobody downloads an unsigned
build from `/releases/latest` (which is also what `codesmith upgrade` reads),
and a build that pins the key never sees a release it would refuse. Neither
signing script publishes; both refuse a release that is no longer a draft.

## Two Macs

Signing is split across two Macs, and each key stays on its own Mac:

| Mac | Holds | Runs |
|---|---|---|
| **skeptic-macmini** | the Developer ID Application key, the `codesmith-notary` notarytool profile | `scripts/sign-macos.sh`: codesign, notarize, rewrite `SHA256SUMS`, upload |
| **Skeptic-Mac** | the minisign secret key `~/.minisign/codesmith.key` | `scripts/sign-release.sh`: check the draft, sign `SHA256SUMS`, upload `SHA256SUMS.minisig` |

`scripts/release.sh` (build and draft upload) runs on either; both are Apple
Silicon Macs with the build setup below. The order matters: the mini changes
the macOS tarballs and so `SHA256SUMS`, so the minisign signature can only be
made after the mini has uploaded them.

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

It checks the version against `Cargo.toml`, then checks every outside page
the binary shows or opens (`codesmith debug links --check`: the API key pages
in `/login`, setup and help pages). A page that is gone (404, 410, no answer)
stops a `--draft-upload`; a dry run only reports it. A page that moved
permanently, blocks scripts (401, 403, 429) or had a server error is a
warning to look at: a move prints the new address to list instead, in
`crates/codesmith/src/links.rs` or wherever the link is defined.
`CODESMITH_SKIP_LINKS=1` skips the check, for building offline. It then builds the four targets, and
smoke-tests them: `--version` for the macOS binaries, `file` must call the
Linux ones static, and with Docker they run in containers (with no network).
It then packages into `dist/<version>/`, writes `SHA256SUMS`, renders the
Homebrew formula from it, and installs the Apple Silicon tarball with
`install.sh` to check that path too. Nothing is signed yet at this point, so
the check signs a copy of `SHA256SUMS` with a throwaway minisign key and runs
a copy of `install.sh` that trusts only that key: the signature check still
runs, and neither the real key nor `install.sh` is involved (without
`minisign` installed, it checks `SHA256SUMS` only). A dry run stops there and
prints the `gh` command it would run, without any call to GitHub (`--dry-run`
is the default and also wins over `--draft-upload`); `--targets
"aarch64-apple-darwin"` narrows a dry run to some targets. Without Rosetta
the x86_64 macOS binary is only checked with `file`, and without Docker the
Linux binaries are only checked with `file`; the script says which checks it
skipped. `CODESMITH_SKIP_DOCKER=1` skips the containers on purpose. `--draft-upload` also needs a clean tree
at the tag `v<version>`, pushed (`--verify-tag`), and runs
`gh release create v<version> --draft --verify-tag --generate-notes` with the
tarballs, `SHA256SUMS` and `BUILT-SHA256SUMS`.

`BUILT-SHA256SUMS` is the manifest of what this Mac built: each tarball's
hash, and for the macOS tarballs every member with its file's hash. The
script ends by printing the manifest's own SHA-256 (`BUILT-SHA256SUMS
sha256: ...`). **Write that hash down and carry it to the signing Macs
yourself** (a note, a message to yourself: not the draft). The draft is not
trusted: anyone who can change it could swap a tarball and rewrite
`SHA256SUMS` to match, so both signing scripts check the draft against the
manifest, and the manifest against that hash.

`--draft-upload` refuses while `crates/codesmith/src/release_key.pub` holds
no release key (only `PLACEHOLDER`), since such a release could not be
signed. With the CodeSmith key pinned it proceeds; the draft it creates is
unsigned and stays a draft until `sign-release.sh` has uploaded
`SHA256SUMS.minisig`.

## Signing and shipping in one run (Skeptic-Mac)

When the macOS identity, the notary profile and the minisign key are all on
Skeptic-Mac, one script does everything after `release.sh` has made the
draft:

```sh
scripts/ship-release.sh v0.2.0
```

It works from the Mac's own terminal or over SSH, and can be run again
after a failure; each step already done is skipped. It:

1. unlocks the login keychain if it is locked, as it is over SSH (you are
   asked for the Mac's login password); without that, `gh` cannot read its
   token and sees no draft, and `notarytool` cannot read its profile;
2. checks that `gh` can see the draft, finds the one `Developer ID
   Application` identity and its Team ID, checks the `codesmith-notary`
   profile and the minisign key, and takes the `BUILT-SHA256SUMS` hash from
   `dist/<version>/` on this Mac, where `release.sh` built it (set
   `CODESMITH_BUILT_SUMS_SHA256` when the build was elsewhere);
3. runs `sign-macos.sh`, unless the draft's macOS checksums already differ
   from the built ones, then `sign-release.sh` (minisign asks for the key's
   password), unless the draft already has `SHA256SUMS.minisig`;
4. asks before publishing (`CODESMITH_YES=1` skips the question);
5. renders the formula from the published `SHA256SUMS` and pushes it to the
   tap as a commit signed like this repository's.

The sections below describe each script on its own, and the two-Mac setup.

## Signing the macOS binaries (skeptic-macmini)

The Developer ID private key never leaves skeptic-macmini.
`scripts/sign-macos.sh` runs there once the draft release exists. It:

1. refuses unless `CODESMITH_BUILT_SUMS_SHA256` is set (the hash
   `release.sh` printed) and the release is still a draft;
2. downloads the draft's tarballs and `BUILT-SHA256SUMS` with `gh`, and
   refuses to continue unless the manifest has that hash and every tarball,
   Linux and macOS, matches it (the draft's `SHA256SUMS` is not used);
3. refuses a macOS tarball holding anything but plain files and folders
   under `codesmith-<version>-<target>/` (no symlinks or hardlinks, nothing
   outside that folder), and extracts only its `codesmith` binary into a
   folder of its own;
4. signs each binary with `codesign --options runtime --timestamp` (hardened
   runtime and secure timestamp, both required for notarization);
5. zips each signed binary with `ditto` and submits it with
   `xcrun notarytool submit --wait`;
6. re-tars the signed binary with the tarball's other files and writes
   `SHA256SUMS` from the manifest's Linux lines and the new macOS tarballs;
7. uploads the tarballs and `SHA256SUMS` with `gh release upload --clobber`,
   and deletes a `SHA256SUMS.minisig` left on the draft by an earlier run
   (it would sign the old `SHA256SUMS`);
8. prints the next step: run `scripts/sign-release.sh vX.Y.Z` on the
   minisign Mac.

It does not need or run minisign. After it, the draft's `SHA256SUMS` is
final but unsigned.

There is nothing to staple: `stapler` only handles `.app`, `.pkg` and `.dmg`,
not a bare executable, and the zip is only a vehicle for submission. Gatekeeper
looks the ticket up online the first time it checks a quarantined copy.
Downloads through `curl`, `install.sh` and Homebrew aren't quarantined, so for
them the signature is what matters.

One-time setup on skeptic-macmini:

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

## Signing the checksums (Skeptic-Mac)

The minisign secret key never leaves Skeptic-Mac. It is the
password-protected file `~/.minisign/codesmith.key` there, outside every
repository; only the public key (the second line of
`~/.minisign/codesmith.pub`) is committed, in `release_key.pub` and as
`RELEASE_KEYS` in `install.sh`. A test fails unless the two lists match, and
another pins the key itself, so a changed key never slips through.

```sh
CODESMITH_BUILT_SUMS_SHA256=<hash> CODESMITH_TEAM_ID=<team> \
  scripts/sign-release.sh v0.2.0        # check the draft, sign, upload SHA256SUMS.minisig
scripts/sign-release.sh --check         # only check minisign, the key and a pinned key
CODESMITH_DRY_RUN=1 scripts/sign-release.sh v0.2.0   # everything but the upload
```

Run it from a checkout of the tag: it trusts that checkout's
`release_key.pub`. For `v0.2.0` it:

1. refuses unless `CODESMITH_BUILT_SUMS_SHA256` (the hash `release.sh`
   printed) and `CODESMITH_TEAM_ID` (your Apple Team ID) are set, and the
   release is still a draft;
2. downloads the draft's `SHA256SUMS` (the one the mini rewrote),
   `BUILT-SHA256SUMS` and its tarballs, refuses unless the manifest has the
   pinned hash, and refuses unless `SHA256SUMS` lists exactly the four
   `codesmith-0.2.0-*` tarballs and each tarball on the draft has the hash
   it lists. A mismatch on a macOS line means `sign-macos.sh` hasn't run,
   didn't finish uploading, or something else changed the draft;
3. refuses unless the Linux lines of `SHA256SUMS` are the manifest's, each
   macOS tarball holds only plain files and folders, exactly the members the
   manifest lists, each file but the binary as built, and both macOS
   binaries pass `codesign --verify --strict` with a `Developer ID
   Application` authority and `TeamIdentifier` equal to `CODESMITH_TEAM_ID`,
   so it never signs checksums of unsigned builds, of another team's, or of
   anything else put on the draft;
4. runs `minisign -S` with the trusted comment `codesmith 0.2.0 SHA256SUMS
   timestamp:<UTC time>` (minisign asks for the key's password);
5. checks what `codesmith upgrade` checks: the signature is prehashed
   (minisign's default), verifies with a key in `release_key.pub`, and its
   trusted comment names the version;
6. uploads `SHA256SUMS.minisig` to the draft, and prints the publish command.

minisign signs the trusted comment as well, and `codesmith upgrade` and
`install.sh` refuse a comment that names another version, so an older
release's signed `SHA256SUMS` can't be replayed as a newer or older one. The
key defaults to `~/.minisign/codesmith.key`; set `CODESMITH_MINISIGN_KEY`
for another path. The script refuses a key inside the repository (also
through a symlink).

One-time setup on Skeptic-Mac:

```sh
brew install minisign gh && gh auth login   # with write access to reka18/codesmith
```

### Backing up the key

Keep an **encrypted offline copy** of `~/.minisign/codesmith.key` and,
separately, its password (the password manager entry). Without both,
releases can't be signed for builds that trust only this key, and their
users would have to reinstall with `install.sh`. For example, an encrypted
disk image on a USB drive kept off-site:

```sh
hdiutil create -size 10m -fs APFS -encryption AES-256 -volname codesmith-key \
  ~/codesmith-key.dmg            # asks for the image's own password
hdiutil attach ~/codesmith-key.dmg
cp ~/.minisign/codesmith.key ~/.minisign/codesmith.pub /Volumes/codesmith-key/
hdiutil detach /Volumes/codesmith-key
# move codesmith-key.dmg to the offline drive, then delete the local copy
```

The key file is already encrypted with its minisign password; the disk
image is a second layer. Check the backup once by restoring it and running
`scripts/sign-release.sh --check` with `CODESMITH_MINISIGN_KEY` pointing at
the restored copy. If Skeptic-Mac is replaced, restore the key to
`~/.minisign/` on the new Mac (`chmod 700 ~/.minisign`, `chmod 600` the key).

### Generating a key

The CodeSmith key was generated on Skeptic-Mac with:

```sh
brew install minisign
mkdir -p ~/.minisign && chmod 700 ~/.minisign
minisign -G -p ~/.minisign/codesmith.pub -s ~/.minisign/codesmith.key
chmod 600 ~/.minisign/codesmith.key
```

`minisign -G` asks for a password for the secret key: use a strong one and
keep it in a password manager. Don't pass `-W`, which leaves the key
unencrypted. Use the same steps, with another file name, for a new key when
rotating.

### Builds from before signing

Builds made while `release_key.pub` held `PLACEHOLDER` check `SHA256SUMS`
only, and print `warning: CodeSmith releases aren't signed yet; checking
SHA256SUMS only`. They upgrade into a signed release on checksums once;
that release pins the key, so their next upgrade checks the signature. A
build that pins the key refuses any release whose `SHA256SUMS.minisig` is
missing, doesn't verify, or names another version, with no flag to override
that, and so refuses to roll back to a release from before signing began;
install that one with `install.sh` or by hand.

### Rotating the key

A build trusts the keys it was compiled with, and a release carries one
signature. To move to a new key, all on Skeptic-Mac unless it says otherwise:

1. **Generate** the new key ("Generating a key") as
   `~/.minisign/codesmith-2.key` / `codesmith-2.pub`, and **back it up**
   ("Backing up the key") before it signs anything.
2. **Pin both keys:** add the new public key (the second line of
   `codesmith-2.pub`) as a second key line in `release_key.pub`, update the
   comment there, and add it to `RELEASE_KEYS` in `install.sh`,
   space-separated, in the same order. Update `CODESMITH_KEY` in the test
   that pins the key (`crates/codesmith/src/upgrade/tests.rs`) to list both.
   Run `scripts/ci.sh`, commit and push, and wait for CI to pass.
3. **Release with both pinned, still signing with the old key** (the normal
   checklist). That release trusts both keys.
4. Once that release has been out long enough that users have it, **sign
   with the new key**:
   `CODESMITH_MINISIGN_KEY=~/.minisign/codesmith-2.key scripts/sign-release.sh vX.Y.Z`.
5. **Drop the old key** from `release_key.pub`, `install.sh` and the test, in
   a later release. Keep the old key's backup until no supported build
   trusts it, then destroy it.

A build older than step 3 trusts only the old key and refuses releases
signed with the new one; its users reinstall with `install.sh`. If the old
key is compromised, skip the overlap: remove it from both lists at once,
sign with the new key, and tell users to reinstall, since builds that trust
the old key can't tell its signatures from yours.

## Checklist

In this order. Each step says which Mac it runs on. The release is a draft
from step 4 until step 8, unsigned until step 6 finishes, and published only
in step 8, after both signing steps.

1. **Bump the version** (any Mac with a clone) in `[workspace.package]` in
   `Cargo.toml` (and the `version` on the `codesmith-ai`/`codesmith-core`
   entries under `[workspace.dependencies]`), then run `cargo check` so
   `Cargo.lock` follows. Run `scripts/ci.sh`, and after pushing check that
   CI passed on GitHub. Add the release's section to
   `CHANGELOG.md`. Commit: `Release 0.2.0`.
2. **Tag and push.** Check that the tag matches the version.

   ```sh
   git tag -s v0.2.0 -m "CodeSmith 0.2.0"
   git push origin main v0.2.0
   ```

3. **Dry run** (the build Mac): `scripts/release.sh 0.2.0` (see "Building a
   release").
4. **Build and upload the draft** (the build Mac):
   `scripts/release.sh 0.2.0 --draft-upload`. The draft has the four
   tarballs, `SHA256SUMS` and `BUILT-SHA256SUMS`, unsigned. **Copy the
   `BUILT-SHA256SUMS sha256` it prints** and take it to both signing Macs
   yourself, not through the draft.
5. **Sign and notarize the macOS binaries on skeptic-macmini**, at a
   checkout of the tag:

   ```sh
   CODESMITH_BUILT_SUMS_SHA256=<the hash from step 4> \
   CODESMITH_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
   CODESMITH_NOTARY_PROFILE=codesmith-notary \
   scripts/sign-macos.sh v0.2.0
   ```

   It refuses unless every tarball on the draft is the one built in step 4.
   This signs and notarizes the macOS binaries, rewrites `SHA256SUMS` and
   uploads the tarballs and `SHA256SUMS`. It ends with: run
   `scripts/sign-release.sh v0.2.0` on the minisign Mac.
6. **Sign the checksums on Skeptic-Mac**, at a checkout of the tag:

   ```sh
   git fetch --tags && git checkout v0.2.0
   CODESMITH_BUILT_SUMS_SHA256=<the hash from step 4> \
   CODESMITH_TEAM_ID=<your Team ID> \
   scripts/sign-release.sh v0.2.0
   ```

   It checks the draft's final `SHA256SUMS` against the tarballs, the Linux
   tarballs and the macOS tarballs' other files against the build manifest,
   and the macOS binaries' Team ID, then signs it (minisign asks for the key's password),
   verifies the signature against `release_key.pub` and uploads
   `SHA256SUMS.minisig`.
7. **Check the signature** on the draft (Skeptic-Mac) before publishing:

   ```sh
   gh release download v0.2.0 --repo reka18/codesmith --dir /tmp/v0.2.0 \
     --pattern SHA256SUMS --pattern SHA256SUMS.minisig
   minisign -V -p ~/.minisign/codesmith.pub -m /tmp/v0.2.0/SHA256SUMS
   ```

   The trusted comment it prints must be `codesmith 0.2.0 SHA256SUMS ...`.
8. **Review and publish** the draft: edit the generated notes, then
   `gh release edit v0.2.0 --repo reka18/codesmith --draft=false`.
9. **Check an install:** `CODESMITH_VERSION=0.2.0 sh install.sh` on a Mac and a
   Linux box, with minisign installed (it should print `signature ok`); on the
   Mac, `codesign -dv --verbose=2 ~/.local/bin/codesmith`
   should show your Developer ID and `flags=0x10000(runtime)`, and on
   skeptic-macmini `xcrun notarytool history --keychain-profile
   codesmith-notary` should list both submissions as Accepted. From an older
   install, `codesmith upgrade 0.2.0` should succeed.
10. **Update the Homebrew tap** (below).

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
