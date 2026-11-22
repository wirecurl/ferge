# Packaging and releases

| Path | What |
|---|---|
| `build.sh` | Builds the binary that is handed out (`dist` profile), with the build machine's paths rewritten and checked. `--target TRIPLE` for another target. On macOS, `CARGO_TARGET_DIR` must lie outside the home folder: the Metal compiler records it. |
| `release.sh` | Reads the version, checks that tag, `Cargo.toml`, `CHANGELOG.md` and metainfo agree, prints the release notes. |
| `publish.sh` | After a release is published: updates the AUR recipes and the Homebrew cask. |
| `linux/` | Desktop file, AppStream metainfo, icons, and `package.sh` for the tarball, `.deb` and `.rpm` (with [nfpm](https://nfpm.goreleaser.com/)). |
| `macos/` | `Info.plist`, `ferge.icns`, and `bundle.sh` for `ferge.app` and the disk image. |
| `aur/` | `PKGBUILD`s for the AUR: `ferge` builds from source, `ferge-bin` installs the release tarball. |
| `homebrew/` | The cask template for [wirecurl/homebrew-tap](https://github.com/wirecurl/homebrew-tap). |
| `icons/` | Icon variants and colours, see its README. |

## What a release contains

The release workflow (`.github/workflows/release.yml`) builds:

- `ferge-VERSION-x86_64-linux.tar.gz` and `…-aarch64-linux.tar.gz`: a prefix tree with `bin/`
  and `share/`, to unpack into `~/.local` or `/usr/local` with `--strip-components=1`.
- `ferge_VERSION_amd64.deb`, `…_arm64.deb`, `ferge-VERSION-1.x86_64.rpm`, `…aarch64.rpm`.
- `ferge-VERSION-macos.dmg`: `ferge.app` for Apple silicon and Intel, signed ad hoc.
- `SHA256SUMS`.

Linux binaries are built in an Ubuntu 22.04 container against glibc 2.35, so they run on
Ubuntu 22.04, Debian 12, Fedora 36 and anything newer.

Started by hand (Actions → Release → Run workflow), the workflow builds everything and keeps it
as workflow artifacts, without a release.

## Making a release

1. Set the version in `Cargo.toml` (`[workspace.package]`) and run `cargo check` so that
   `Cargo.lock` follows.
2. In `CHANGELOG.md`, turn `## [Unreleased]` into `## [X.Y.Z] - YYYY-MM-DD` above a new, empty
   `## [Unreleased]`, and add the link at the bottom.
3. Add `<release version="X.Y.Z" date="YYYY-MM-DD"/>` at the top of `<releases>` in
   `linux/com.wirecurl.ferge.metainfo.xml`.
4. Commit (`chore: release X.Y.Z`), tag `vX.Y.Z`, push both. The workflow checks that all of
   the above agree and creates a draft release with the packages.
5. Look at the draft, try a package, publish it.
6. `packaging/publish.sh cask X.Y.Z` updates the tap. `packaging/publish.sh aur X.Y.Z` updates
   `aur/*/PKGBUILD` and `.SRCINFO`; commit them, then copy each into its AUR clone
   (`ssh://aur@aur.archlinux.org/ferge.git`, `…/ferge-bin.git`) and push.

macOS shows a warning before the first start of a downloaded copy until the app is signed with
an Apple Developer ID and notarized; `bundle.sh` is where that would go.
