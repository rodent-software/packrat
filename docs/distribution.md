# Distributing packrat

[`dist`](https://axodotdev.github.io/cargo-dist) builds the release artifacts
from `dist-workspace.toml` and publishes the GitHub release, the shell and
PowerShell installers, the Homebrew formula and the updater. Everything else is
handled by `.github/workflows/package-managers.yml`, which runs after the
`Release` workflow finishes.

## Channels

| Channel | Repository | Produced by | Secret |
| --- | --- | --- | --- |
| GitHub release + installers | `rodent-software/packrat` | `release.yml` (dist) | — |
| Homebrew | `rodent-software/homebrew-tap` | `release.yml` (dist) | `HOMEBREW_TAP_TOKEN` |
| apt + rpm | `rodent-software/packrat-repo` | `package-managers.yml` | `PACKAGES_TOKEN`, `APT_GPG_PRIVATE_KEY` |
| AUR (`packrat-bin`) | `aur.archlinux.org/packrat-bin` | `package-managers.yml` | `AUR_SSH_PRIVATE_KEY` |
| Winget | `microsoft/winget-pkgs` | `package-managers.yml` | `WINGET_TOKEN` |
| Scoop | `rodent-software/scoop-bucket` | self-updating manifest | — |
| Nix | `flake.nix` in this repository | — | — |

`.deb` and `.rpm` packages are attached to every release, prereleases included.
Publishing to Homebrew, apt/rpm, the AUR and Winget is limited to stable
releases; each job explains itself and skips when its secret is unset.

The package-managers workflow listens with `workflow_run` rather than
`release: published`, because a release created with the default
`GITHUB_TOKEN` does not trigger further workflows.

## Required secrets

| Secret | Scope | Used for |
| --- | --- | --- |
| `HOMEBREW_TAP_TOKEN` | `contents: write` on `homebrew-tap` | dist Homebrew publisher |
| `PACKAGES_TOKEN` | `contents: write` on `packrat-repo` | apt/rpm repository push |
| `APT_GPG_PRIVATE_KEY` | armored private key for the repository signing key | apt/rpm signing |
| `AUR_SSH_PRIVATE_KEY` | SSH key registered with the AUR account | AUR push |
| `WINGET_TOKEN` | *classic* PAT with `public_repo` and `workflow` | Winget PR |

## One-time setup

### apt and rpm repository

`rodent-software/packrat-repo` serves the repository over GitHub Pages. The
layout the workflow writes is:

```text
apt/    reprepro pool and dists
rpm/    packages and repodata
packrat.gpg
```

Generate a signing key once, publish its public half, and store the private
half as `APT_GPG_PRIVATE_KEY`:

```sh
gpg --quick-generate-key "packrat repository <noreply@github.com>" default default never
gpg --armor --export-secret-keys "packrat repository" > private.asc   # -> secret
gpg --armor --export "packrat repository" > packrat.gpg               # -> commit to packrat-repo
```

Then create a PAT with `contents: write` on `packrat-repo` and store it as
`PACKAGES_TOKEN`. The `linux-packages` job in `package-managers.yml` always
builds the packages; the `publish-linux-repo` job refreshes the repository.

### AUR

The `packrat-bin` package must exist on the AUR and be owned by the account
whose key is in `AUR_SSH_PRIVATE_KEY`; the action pushes to
`aur@aur.archlinux.org:packrat-bin.git`. Create it once by cloning the AUR
repository, copying `packaging/aur/PKGBUILD`, generating `.SRCINFO`
(`makepkg --printsrcinfo > .SRCINFO`) and pushing. After that the workflow
updates the version and checksums on every stable release.

### Winget

`vedantmgoyal9/winget-releaser` updates an existing package, so at least one
version must already be in `microsoft/winget-pkgs`. Submit the first version
once with [`wingetcreate`](https://github.com/microsoft/winget-create):

```sh
wingetcreate new https://github.com/rodent-software/packrat/releases/download/v0.1.0/packrat-x86_64-pc-windows-msvc.msi
```

The package identifier is `RodentSoftware.Packrat`. Add a classic PAT with
`public_repo` and `workflow` scopes as `WINGET_TOKEN`; the `winget` job then
opens the update PR automatically.

### Scoop

`bucket/packrat.json` uses Scoop's `checkver`/`autoupdate`, so `scoop update`
refreshes the version and checksum from the GitHub release without any CI. Bump
the committed `version`/`url`/`hash` by hand when convenient, or run
`scoop checkver -u packrat` locally.

## Testing a release

```sh
dist plan                 # artifact names, targets, installers
dist generate --mode ci --check
dist generate --mode msi --check
```

Push a `v*` tag to build and publish. Prereleases build and attach the
`.deb`/`.rpm` packages but skip every package manager.
