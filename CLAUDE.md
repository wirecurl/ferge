# Ferry – working notes

PLAN.md is the binding source for technology, architecture and performance budgets.

## Layout

- `crates/ferry-core` – UI-independent core (remote backends, listings). Never depends on GPUI.
- `crates/ferry-app` – GPUI desktop application (binary `ferry`).
- `crates/ferry-bench` – benchmark harness for the PLAN.md budgets.

## Commands

```sh
cargo build                      # debug build of the app (deps at opt-level 3)
cargo run                        # start the app
cargo build --release            # release build
cargo test --workspace           # all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Test server

```sh
./testserver/up.sh               # OpenSSH on 127.0.0.1:2222, waits until ready
./testserver/down.sh             # stop and remove (fixtures are regenerated on next start)
cargo test --workspace -- --include-ignored   # includes SFTP integration tests
```

Credentials: user `ferry`, password `ferry`; keys in `.testserver/` (`id_ed25519` without
passphrase, `id_ed25519_encrypted` with passphrase `ferry`). Fixtures in the login directory:
`many/` (100,000 files, deterministic names, sparse sizes, spread mtimes) and `tree/` (nested
dirs, unicode, hidden files, symlinks incl. a dangling one). Tests that need the server are
`#[ignore]`d so plain `cargo test` works offline.

## Rules

- All dependency versions are pinned exactly in the root `Cargo.toml`. GPUI comes from
  `gpui-pre =0.3.12` (required by `gpui-component 0.10.0`), not `gpui` from crates.io.
- No I/O on the UI thread. Network and filesystem work runs on tokio inside ferry-core.
- For GPUI API questions read the sources in `~/.cargo/registry/src/*/gpui-pre-0.3.12` and
  `~/.cargo/registry/src/*/gpui-component-0.10.0` instead of guessing.
- Commit messages and authorship never mention AI tools.
