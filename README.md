# Ferry

A fast file transfer client for Linux and macOS, written in Rust with
[GPUI](https://www.gpui.rs/). Ferry is keyboard-first, renders on the GPU and keeps every
network and disk operation off the UI thread, so folders with 100,000 entries scroll, sort and
filter without dropping a frame.

Status: **Phase 3** (see [PLAN.md](PLAN.md)). Browsing, file operations and transfers between
this computer and SFTP servers work, with favorites, history and saved passwords; further
protocols and sync follow in later phases.

The window follows the dual-pane concept of classic file transfer clients: this computer on
the left, a server on the right. Each side switches between its files and a server panel with
favorites, the hosts from `~/.ssh/config`, recent connections and Quick Connect.

## Features

- Transfers between any two panes or tabs: uploads, downloads and server-to-server copies of
  files and whole folders. Copy with Ctrl+C / Ctrl+V, F5 into the other pane, or drag and
  drop, including files dropped from the desktop's file manager. Local files can also be dragged
  out of Ferry into other applications.
- A transfer queue with progress, speed and time left; up to four files in parallel. Transfers
  can be paused and resumed in the middle of a file. Files are written as `name.ferry-part` and
  renamed when complete, keeping their modification time; cancelling removes partial files.
- Existing files are not overwritten silently: replace, skip or keep both, optionally for the
  rest of a transfer.
- SFTP reads and writes are pipelined (16 requests in flight, up to 256 KiB each) over separate
  data channels, so transfers never block browsing.
- Two file browsers per tab, side by side (one in narrow windows). A server panel in each:
  favorites in folders, `~/.ssh/config` hosts, recent connections, Quick Connect, and a search
  that connects with Enter. Favorites and history are stored in SQLite in the data directory
  (`~/.local/share/ferry` on Linux).
- File operations: new folder and file, rename in place, duplicate, delete (folders with their
  contents, after asking), copy address; also from a row's context menu.
- An inspector with the details of the selection and editable permissions; optional Kind and
  Permissions columns; hidden files on demand.
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
- Uses the desktop's interface and fixed-width fonts and its light or dark preference, and
  follows changes to them while running; respects server-side decorations on Wayland
  compositors such as niri. Theme, fonts and font size can be changed in the settings window
  or in a settings file.

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
ferry                                   # this computer on the left, the servers on the right
ferry myserver                          # an ssh_config alias, opened on the right
ferry sftp://alice@example.org:2222/srv # user, port and start directory
ferry /var/log                          # local folder
ferry -F ./ssh_config myserver          # use another SSH client configuration
```

| Key (Linux) | Key (macOS) | Action |
|---|---|---|
| ↑ ↓ PgUp PgDn Home End | same | move the selection |
| Shift+↑ / Shift+↓, Shift-click, Ctrl-click, Ctrl+A | Shift+↑ / ↓, ⇧-click, ⌘-click, ⌘A | select several items |
| Enter, Alt+↓ | Enter, ⌘↓ | open folder |
| Backspace, Alt+↑ | Backspace, ⌘↑ | parent folder (Backspace edits the filter while one is active) |
| Alt+← / Alt+→ | ⌘[ / ⌘] | back / forward |
| any character, Ctrl+F | same, ⌘F | filter the folder; Esc clears |
| Ctrl+1 / 2 / 3 | ⌘1 / 2 / 3 | sort by name / size / date, again to reverse |
| Ctrl+R | ⌘R | refresh |
| F5, Ctrl+Shift+R | F5, ⌘⇧R | copy the selection to the other side (refresh with one pane) |
| Ctrl+C / Ctrl+V | ⌘C / ⌘V | copy items, paste (transfer) them into the current folder |
| Ctrl+Shift+N / Ctrl+Alt+N | ⌘⇧N / ⌘⌥N | new folder / new file |
| F2 | F2 | rename |
| Delete | ⌘⌫ | delete, after asking |
| Ctrl+D | ⌘D | duplicate |
| Ctrl+Shift+C | ⌘⇧C | copy the address of the selection |
| Ctrl+H | ⌘⇧. | show or hide hidden files |
| Ctrl+I | ⌘I | show or hide the inspector |
| Ctrl+L, Ctrl+Shift+G | ⌘L, ⌘⇧G | go to a folder or address |
| Ctrl+Shift+K | ⌘⇧K | switch a pane between its files and the servers |
| Ctrl+E | ⌘E | disconnect |
| Tab, Ctrl+Alt+← / → | Tab, ⌘⌥← / → | switch between the two sides |
| Ctrl+\\ | ⌘\\ | one or two file browsers |
| Ctrl+J | ⌘J | show or hide transfers |
| Ctrl+T / Ctrl+W | ⌘T / ⌘W | new tab / close tab |
| Ctrl+Tab, Ctrl+PgDn / Ctrl+Shift+Tab, Ctrl+PgUp | Ctrl+Tab, ⌘⇧] / Ctrl+Shift+Tab, ⌘⇧[ | next / previous tab |
| Ctrl+, | ⌘, | settings |

In the server panel, type to search, use ↑ ↓ and press Enter to connect (Ctrl+Enter: new tab).
Favorites and folders are edited from their context menu or the + menu.

## Settings

Settings live in `settings.toml` in the configuration directory (`~/.config/ferry` on Linux,
`~/Library/Application Support/ferry` on macOS). The settings window (Ctrl+, or the gear in the
toolbar) edits the same file, and edits made to the file in any editor take effect as soon as
it is saved. Ferry keeps comments and the order of what is written by hand, and a file that is
a symbolic link (as in a dotfiles repository) stays one. A setting that is left out uses its
default; a value Ferry cannot use is reported and ignored.

```toml
[appearance]
theme = "dark"              # "system" (default), "light" or "dark"
font = "Inter"              # interface font; default: the desktop's
font-size = 14              # in pixels, 6 to 32; default: the desktop's
mono-font = "JetBrains Mono" # fixed-width text; default: the desktop's
```

On Linux, "the desktop's" fonts come from the XDG desktop portal (the GNOME interface settings
that every portal backend provides), including the text scaling factor.



## Development

See [CLAUDE.md](CLAUDE.md) for build, test and benchmark commands. In short:

```sh
cargo test --workspace
./testserver/up.sh && cargo test --workspace -- --include-ignored
cargo build --release && cargo run --release -p ferry-bench
```

The workspace has three crates: `ferry-core` (protocols, listings; no UI), `ferry-app` (GPUI
application) and `ferry-bench` (performance budget checks).
