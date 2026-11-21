# Ferry

A fast file transfer client for Linux and macOS, written in Rust with
[GPUI](https://www.gpui.rs/). Ferry is keyboard-first, renders on the GPU and keeps every
network and disk operation off the UI thread, so folders with 100,000 entries scroll, sort and
filter without dropping a frame.

Status: **Phase 2** (see [PLAN.md](PLAN.md)). Browsing local folders and SFTP servers in tabs,
favorites, history and saved passwords work; transfers and further protocols follow in later
phases.

## Features

- Tabs, each with its own connection, and a sidebar with favorites in (nested) folders, the
  hosts from `~/.ssh/config` and the local file system.
- Quick Connect on every new tab, plus the most recent connections. Favorites and history are
  stored in SQLite in the data directory (`~/.local/share/ferry` on Linux).
- Passwords and key passphrases can be remembered: in the Secret Service (GNOME Keyring,
  KeePassXC, KWallet) on Linux and the Keychain on macOS. Without a credential service, Ferry
  uses an encrypted file (Argon2id + XChaCha20-Poly1305) protected by a master password.
  Passwords are only saved after they worked; a saved password the server rejects is asked for
  again. `FERRY_SECRET_STORE=file` forces the encrypted file.
- SFTP via [russh](https://github.com/Eugeny/russh): SSH agent, key files (with passphrase
  prompt), keyboard-interactive and password authentication.
- Reads `~/.ssh/config`: host aliases, `HostName`, `User`, `Port`, `IdentityFile`,
  `IdentitiesOnly`, `ProxyCommand`, `UserKnownHostsFile`, `HostKeyAlias`, `Include`,
  `Match all`.
- Strict host key checking against `known_hosts` with OpenSSH semantics (hashed names,
  wildcards, `@revoked`). Unknown keys are confirmed in a dialog; changed keys are refused.
- Listings stream in while the server is still sending, are cached per connection and
  revalidated in the background. Going back is instant.
- Natural, case-insensitive sorting by name, size or date (folders first) and type-to-filter.
- Follows the system light/dark preference; respects server-side decorations on Wayland
  compositors such as niri.

## Install

Requires Rust 1.99 or newer and, on Linux, the usual GPUI dependencies (Vulkan, Wayland or X11,
fontconfig).

```sh
cargo install --locked --path crates/ferry-app
install -Dm644 packaging/linux/dev.ferry.Ferry.desktop ~/.local/share/applications/dev.ferry.Ferry.desktop
# If ~/.cargo/bin is not on the PATH of your desktop session, use an absolute Exec= path:
sed -i "s|^Exec=ferry|Exec=$HOME/.cargo/bin/ferry|" ~/.local/share/applications/dev.ferry.Ferry.desktop
```

## Usage

```sh
ferry                                   # Quick Connect, favorites and recent connections
ferry myserver                          # an ssh_config alias
ferry sftp://alice@example.org:2222/srv # user, port and start directory
ferry /var/log                          # local folder
ferry -F ./ssh_config myserver          # use another SSH client configuration
```

| Key (Linux) | Key (macOS) | Action |
|---|---|---|
| ↑ ↓ PgUp PgDn Home End | same | move the selection |
| Enter, Alt+↓ | Enter, ⌘↓ | open folder |
| Backspace, Alt+↑ | Backspace, ⌘↑ | parent folder (Backspace edits the filter while one is active) |
| Alt+← / Alt+→ | ⌘[ / ⌘] | back / forward |
| any character | same | type-to-filter, Esc clears |
| Ctrl+1 / 2 / 3 | ⌘1 / 2 / 3 | sort by name / size / date, again to reverse |
| F5, Ctrl+R | F5, ⌘R | refresh |
| Ctrl+L | ⌘L | focus the address field |
| Ctrl+T / Ctrl+W | ⌘T / ⌘W | new tab / close tab |
| Ctrl+Tab, Ctrl+PgDn / Ctrl+Shift+Tab, Ctrl+PgUp | Ctrl+Tab, ⌘⇧] / Ctrl+Shift+Tab, ⌘⇧[ | next / previous tab |
| Ctrl+D | ⌘D | save the current folder as a favorite |
| Ctrl+B | ⌘B | show or hide the sidebar |

Favorites are opened with a click (Ctrl/⌘-click opens a new tab); right-click a favorite or a
folder to edit, duplicate, rename or delete it.

## Development

See [CLAUDE.md](CLAUDE.md) for build, test and benchmark commands. In short:

```sh
cargo test --workspace
./testserver/up.sh && cargo test --workspace -- --include-ignored
cargo build --release && cargo run --release -p ferry-bench
```

The workspace has three crates: `ferry-core` (protocols, listings; no UI), `ferry-app` (GPUI
application) and `ferry-bench` (performance budget checks).
