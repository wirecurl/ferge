# ferge – working notes

PLAN.md is the binding source for technology, architecture and performance budgets.

## Layout

- `crates/ferge-core` – UI-independent core: remote backends, listings, `transfer` (queue,
  pause/resume, conflicts), `library` (SQLite: favorites, folders, history), `secrets`
  (keyring / encrypted file), `settings` (TOML file, edited in place, watched), `desktop`
  (desktop fonts via the XDG portal), `document` (previews: spreadsheets, office text, PDF
  pages, pictures; bounded against hostile files, and read by the app only through
  `document::worker`, a separate `ferge --preview-worker` process with a memory limit from the
  app's `cap` allocator and a timeout). Never depends on GPUI.
- `crates/ferge-app` – GPUI desktop application (binary `ferge`).
- `crates/ferge-bench` – benchmark harness for the PLAN.md budgets.

## Commands

```sh
cargo build                      # debug build of the app (deps at opt-level 3)
cargo run                        # start the app
cargo build --release            # release build
cargo test --workspace           # all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo deny check                 # licenses, advisories, sources (cargo install --locked cargo-deny@0.22.0)
```

The compiler is pinned in `rust-toolchain.toml`; CI (`.github/workflows/ci.yml`) runs all of
the above plus the SFTP integration tests. Actions in workflows are pinned to commit SHAs.

## Test server

```sh
./testserver/up.sh               # OpenSSH on 127.0.0.1:2222, waits until ready
./testserver/down.sh             # stop and remove (fixtures are regenerated on next start)
cargo test --workspace -- --include-ignored   # includes SFTP integration tests
```

Credentials: user `ferge`, password `ferge`; keys in `.testserver/` (`id_ed25519` without
passphrase, `id_ed25519_encrypted` with passphrase `ferge`). For working as root: `ferge` may use sudo
with its password, `ferge-nopasswd` without one, `ferge-nosudo` not at all (same keys and
password); `/root/private/secret.txt` is readable only by root. Fixtures in the login directory:
`many/` (100,000 files, deterministic names, sparse sizes, spread mtimes) and `tree/` (nested
dirs, unicode, hidden files, symlinks incl. a dangling one). Tests that need the server are
`#[ignore]`d so plain `cargo test` works offline.

## Benchmarks

```sh
./testserver/up.sh
cargo build --release
cargo run --release -p ferge-bench   # prints measurements and a budget table; exit 1 = budget missed
                                     # also reports transfer throughput (256 MiB up/down, vs. OpenSSH sftp)
```

The harness starts the release binary in hidden `--bench startup|interaction` modes. The app
prints `ferge-bench:` markers to stderr; `ZED_MEASUREMENTS=1` makes GPUI print the CPU time of
every frame, which the harness attributes to the preceding input. Frame budget: 8.33 ms (120 Hz).

Running the app against the test server: `cargo run -- -F .testserver/ssh_config sftp://ferge-test/home/ferge/many`.

To try favorites and saved passwords without touching your own data, point the data directory
elsewhere and force the encrypted-file store:
`XDG_DATA_HOME=/tmp/ferge-data FERGE_SECRET_STORE=file cargo run`. Settings work the same way
with `XDG_CONFIG_HOME=/tmp/ferge-config` (`ferge/settings.toml` below it; the app watches it).
Benchmark runs (`--bench`) keep everything in memory and never read or write settings. `cargo test -p ferge-core --test keyring -- --ignored` exercises the
real Secret Service / Keychain.

## Rules

- All dependency versions are pinned exactly in the root `Cargo.toml`. GPUI comes from
  `gpui-pre =0.3.12` (required by `gpui-component 0.10.0`), not `gpui` from crates.io.
- No I/O on the UI thread. Network and filesystem work runs on tokio inside ferge-core.
- For GPUI API questions read the sources in `~/.cargo/registry/src/*/gpui-pre-0.3.12` and
  `~/.cargo/registry/src/*/gpui-component-0.10.0` instead of guessing.
- Commit messages and authorship never mention AI tools.
- Commit subjects follow Conventional Commits: `type(scope): lowercase imperative summary`.
  Types: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `build`, `ci`, `chore`. Scopes:
  `core`, `sftp`, `transfer`, `library`, `secrets`, `settings`, `preview`, `app`, `bench`,
  `testserver`; a change spanning several takes the one most of it is in, and repository-wide
  changes go without a scope. The body explains what changed and why, in full sentences.
- The name is always written in lowercase: "ferge", also at the start of a sentence and in
  window titles. The app ID is `com.wirecurl.ferge`. Licensed GPL-3.0-or-later.
- Look and feel come only from `crates/ferge-app/src/style.rs`, the design system:
  - Sizes are `style::em(x)`, multiples of body text. The theme's rem is body / 0.875, so
    gpui-component's `text_sm` controls (menus, fields, buttons) match ferge's body text.
  - Text sizes: `text_body`, `text_small`, `text_title`. Controls come in two sizes
    (`ControlSize::Regular` in the toolbar, `Compact` in headers and panels).
  - Icon-only buttons are `style::IconButton`, choices are `style::segmented`, related controls
    sit in a `style::track`, fields in bars go through `style::field`, lists of items in a
    `style::group`. Text buttons are gpui-component `Button`s in their default or `.small()`
    size.
  - `cargo test` runs `style::design_rules`, which rejects `text_sm`/`text_xs`, `rems(`,
    `.xsmall()`, icon buttons from gpui-component, `ButtonGroup` and ad-hoc radii in views.
    Check visual changes in both themes and at a larger font size before committing.
