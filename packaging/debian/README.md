# Debian packaging

`build.sh` creates Ubuntu 24.04 `amd64` binary packages from already-built release binaries:

- `potty_<version>_amd64.deb` — GUI terminal, desktop file, icons, and runtime GUI dependencies.
- `potty-tools_<version>_amd64.deb` — `potty-session` and `potty-notify` without GUI dependencies.

The GitHub release workflow builds on `ubuntu-24.04`, so the package targets Ubuntu 24.04 LTS and
newer systems with compatible glibc/runtime libraries.

## APT repository via GitHub Pages

The release workflow publishes a signed static APT repository to the `gh-pages` branch, matching
the pattern used by `decaychain/uncloud`. Once GitHub Pages is enabled for this repository, it is
served from:

```text
https://decaychain.github.io/potty/
```

Repository shape:

```text
pool/main/p/potty/*.deb
dists/stable/main/binary-amd64/Packages
dists/stable/main/binary-amd64/Packages.gz
dists/stable/Release
dists/stable/InRelease
```

The workflow expects a GitHub Actions secret named `APT_GPG_PRIVATE_KEY`, containing an ASCII-armored
private key that can sign unattended. This can be the same organization/package-repository key used
by Uncloud, but a repo-scoped secret from `decaychain/uncloud` is not automatically visible here;
either expose it as an organization secret to `decaychain/potty`, or copy it into this repository's
secrets.

One-time user setup after the first successful `publish-apt` run:

```sh
curl -fsSL https://decaychain.github.io/potty/pubkey.gpg \
  | sudo gpg --dearmor -o /usr/share/keyrings/potty.gpg
echo "deb [signed-by=/usr/share/keyrings/potty.gpg] https://decaychain.github.io/potty stable main" \
  | sudo tee /etc/apt/sources.list.d/potty.list
sudo apt update
sudo apt install potty
```
