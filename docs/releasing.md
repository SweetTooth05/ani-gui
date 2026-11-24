# Releasing ani-gui

Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`. The workflow
tests and builds the app, packages a tarball, signs its checksums and
publishes a GitHub release. Installed copies of ani-gui find that release and
update themselves.

## How updates are trusted

GitHub alone isn't trusted. Anyone with a token that has Contents:write on
this repository can create releases and upload any file, including a
checksum file that matches their own archive. So:

- Each release carries `SHA256SUMS` plus `SHA256SUMS.sig`, a raw Ed25519
  signature of that file.
- The app embeds the public key (`RELEASE_PUBLIC_KEYS` in `src/updater.rs`,
  and the same key in `docs/release-signing-key.pub.pem`; a unit test keeps
  the two equal).
- The updater installs nothing unless:
  - the signature verifies,
  - the archive's SHA-256 matches its line in the signed file, and
  - the archive name carries the release's version and target triple.
- The private key never goes into the repository. It exists in two places
  only: your machine, and the `RELEASE_SIGNING_KEY` Actions secret.
- Only the workflow's last job (`release`) can read the secret. That job
  doesn't build or run project code. The workflow refuses to sign with a key
  that isn't listed in `docs/release-signing-key.pub.pem`.

## One-time setup

### 1. The signing key

The key was generated with `openssl genpkey -algorithm ed25519` and is
stored at:

```
~/.config/ani-gui-release/release-signing-key.pem   (directory 700, file 600)
```

Back it up somewhere offline, such as an encrypted USB stick or your
password manager. If you lose it, the installed apps can only be moved to a
new key by having users reinstall by hand (see "Rotating the key" below).
Never commit it, paste it into an issue, or send it to anyone.

To see the public half, which is safe to share:

```sh
openssl pkey -in ~/.config/ani-gui-release/release-signing-key.pem -pubout
```

It must match `docs/release-signing-key.pub.pem`.

### 2. A protected `release` environment (strongly recommended)

A token with Contents:write can also push a commit and a tag. That would
make the workflow sign whatever they pushed. An environment with a required
reviewer means nothing gets signed until you approve it in the browser:

1. Go to the repository's **Settings → Environments → New environment** and
   name it `release`. The workflow already uses this name.
2. Under **Required reviewers**, add yourself.
3. Under **Deployment branches and tags**, choose **Selected branches and
   tags** and add the tag rule `v*.*.*`.

Keep the tokens you use day to day free of the Actions, Environments,
Deployments, Secrets and Administration permissions. Without those, a leaked
token can't approve a deployment or replace the secret.

### 3. The secret

Run this yourself, from a shell where `gh` is logged in with an account that
can administer the repository (`gh auth login` uses a browser sign-in; don't
paste a token):

```sh
gh secret set RELEASE_SIGNING_KEY --env release --repo SweetTooth05/ani-gui \
  < ~/.config/ani-gui-release/release-signing-key.pem
```

This stores the key as an environment secret, which only jobs approved for
`release` can read. If you skipped step 2, leave out `--env release` to make
it a repository secret instead. That is weaker: any workflow run on a pushed
tag can read it.

To check that it is there (gh shows names only, never values):

```sh
gh secret list --env release --repo SweetTooth05/ani-gui
```

## Cutting a release

1. Pick the version and set it in `Cargo.toml` (`version = "X.Y.Z"`).
2. Run `cargo build` so `Cargo.lock` matches.
3. Add a `CHANGELOG.md` entry.
4. Commit and push to the release branch.
5. Tag that commit and push the tag:

   ```sh
   git tag -a vX.Y.Z -m "ani-gui X.Y.Z"
   git push origin vX.Y.Z
   ```

   The tag must be exactly `v` plus the Cargo version. The first job fails
   on any mismatch, before anything is built.
6. Watch the run under **Actions → Release**:
   - **Check the tag matches Cargo.toml**
   - **Build x86_64-unknown-linux-gnu**: `cargo test`, the release build,
     then packaging (on ubuntu-22.04, for an older glibc)
   - **Build aarch64-apple-darwin** (macos-14) and **Build
     x86_64-apple-darwin** (macos-15-intel): the same, packaged as an
     ad hoc signed `ani-gui.app` (`make app`)
   - **Sign and publish**: waits for your approval if you set up the
     environment. Approve only tags you pushed yourself.
7. The release contains:
   - `ani-gui-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz`: `bin/ani-gui`, the
     desktop entry and icons, laid out as an install prefix
   - `ani-gui-X.Y.Z-aarch64-apple-darwin.tar.gz` and
     `ani-gui-X.Y.Z-x86_64-apple-darwin.tar.gz`: `ani-gui.app` at the top
   - `SHA256SUMS`
   - `SHA256SUMS.sig`

Installed apps notice the release within six hours, or straight away from
**Settings → Updates → Check now**.

Before the first release, you can also run the signed artifacts through the
real updater locally:

1. Download the run's artifacts into `dist/`. Or run the workflow's Package
   and sign steps by hand, with the key in `RELEASE_SIGNING_KEY`.
2. Run:

   ```sh
   ANI_GUI_DIST=dist cargo test release_artifacts -- --ignored
   ```

### If something goes wrong

- **The tag doesn't match**: delete it with
  `git push origin :refs/tags/vX.Y.Z` and `git tag -d vX.Y.Z`, fix
  `Cargo.toml`, then tag again.
- **"RELEASE_SIGNING_KEY is not set"**: do setup step 3.
- **"RELEASE_SIGNING_KEY is not a key listed..."**: the secret holds a
  different key from the one the app embeds. Set the secret again from the
  right file.
- **A published release is broken**: publish a fixed `vX.Y.(Z+1)`. Don't
  edit an existing release's assets; the updater only moves forward.

## Verifying a release by hand

```sh
v=X.Y.Z
base=https://github.com/SweetTooth05/ani-gui/releases/download/v$v
curl -fL -O "$base/ani-gui-$v-x86_64-unknown-linux-gnu.tar.gz" \
  -O "$base/SHA256SUMS" -O "$base/SHA256SUMS.sig"
openssl pkeyutl -verify -rawin -pubin -inkey docs/release-signing-key.pub.pem \
  -in SHA256SUMS -sigfile SHA256SUMS.sig
sha256sum --check --ignore-missing SHA256SUMS
```

Both commands must succeed: `Signature Verified Successfully`, then `OK`.

## macOS builds

The build job is a matrix: Linux, `aarch64-apple-darwin` on `macos-14` and
`x86_64-apple-darwin` on `macos-15-intel`. Each writes
`dist/ani-gui-<version>-<target>.tar.gz`; the release job signs every archive
it receives, and the updater picks its asset by the target triple it was
built for (`build.rs`). On macOS the updater replaces the whole
`ani-gui.app`, so the archive must hold exactly `ani-gui.app/` at its top
with `Contents/MacOS/ani-gui` inside. The app is signed ad hoc, not with a
Developer ID, and isn't notarized.

## Rotating the key

`RELEASE_PUBLIC_KEYS` is a list, and `docs/release-signing-key.pub.pem` may
hold several PEM blocks in the same order. The workflow signs with whichever
listed key the secret holds.

### Planned rotation (the old key is still safe)

1. Generate the new key next to the old one:

   ```sh
   umask 077
   openssl genpkey -algorithm ed25519 \
     -out ~/.config/ani-gui-release/release-signing-key-new.pem
   chmod 600 ~/.config/ani-gui-release/release-signing-key-new.pem
   ```

2. **Transition release, signed with the old key.**
   - Append the new public key (`openssl pkey -in ... -pubout`) to
     `docs/release-signing-key.pub.pem`.
   - Add its raw 32 bytes as a second entry in `RELEASE_PUBLIC_KEYS`. Get
     them with:

     ```sh
     openssl pkey -in ... -pubout -outform DER | tail -c 32 | xxd -i
     ```

   - Run `cargo test`. `embedded_keys_match_published_pem` checks the two
     lists agree.
   - Release as usual; the secret still holds the old key. Installed apps
     accept the release because the old key signed it. The new binary
     trusts both keys.
3. **Switch the secret** to the new key with the `gh secret set` command
   above, pointed at the new file.
4. **Next release, signed with the new key.** Remove the old key from both
   lists, then release as usual.
5. Copies still older than the transition release can't verify releases
   signed only with the new key. They report a failed update, and their
   users need to reinstall from the README once.
6. Destroy the old key file once you no longer need it:

   ```sh
   shred -u ~/.config/ani-gui-release/release-signing-key.pem
   ```

   Then rename the new key file to the old name.

### The key leaked

Whoever holds the leaked key can sign updates that every installed copy
accepts. The app has no way to revoke a key, so:

1. Make sure the release workflow can't run for anyone else:
   - Revoke the leaked credentials.
   - Rotate the secret.
   - Check the `release` environment's reviewers.
2. Generate a new key. Release a version that embeds only the new key,
   signed with the new key. Already-installed copies will refuse it, which
   is intended.
3. Tell users to reinstall from the README, and to check the release by hand
   (see above) against the new `docs/release-signing-key.pub.pem` from the
   repository.
