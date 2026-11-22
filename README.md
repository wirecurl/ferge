# ferge

A fast file transfer client for Linux and macOS, written in Rust with
[GPUI](https://www.gpui.rs/). ferge is keyboard-first, renders on the GPU and keeps every
network and disk operation off the UI thread, so folders with 100,000 entries scroll, sort and
filter without dropping a frame.

Status: **pre-release**, working towards 0.1.0 (see [PLAN.md](PLAN.md) and
[CHANGELOG.md](CHANGELOG.md)). Browsing, previews, file operations and transfers between this
computer and SFTP servers work, with favorites, history, saved passwords and working as root;
FTP/FTPS, S3, WebDAV and sync follow in later phases.

The window follows the dual-pane concept of classic file transfer clients: this computer on
the left, a server on the right. Each side switches between its files and a server panel with
favorites, the hosts from `~/.ssh/config`, recent connections and Quick Connect.

## Features

- Transfers between any two panes or tabs: uploads, downloads and server-to-server copies of
  files and whole folders. Copy with Ctrl+C / Ctrl+V, F5 into the other pane, or drag and
  drop, including files dropped from the desktop's file manager. Local files can also be dragged
  out of ferge into other applications.
- A transfer queue with progress, speed and time left; up to four files in parallel. Transfers
  can be paused and resumed in the middle of a file. Files are written as `name.ferge-part` and
  renamed when complete, keeping their modification time; cancelling removes partial files.
- Existing files are not overwritten silently: replace, skip or keep both, optionally for the
  rest of a transfer. A folder that exists already is asked about once: merge into it, skip it
  or keep both. A symbolic link where a folder goes is never followed.
- SFTP reads and writes are pipelined (16 requests in flight, up to 255 KiB each) over separate
  data channels, so transfers never block browsing.
- Two file browsers per tab, side by side (one in narrow windows). A server panel in each:
  favorites in folders, `~/.ssh/config` hosts, recent connections, Quick Connect, and a search
  that connects with Enter. Favorites and history are stored in SQLite in the data directory
  (`~/.local/share/ferge` on Linux).
- Work as root on any SSH server where your account may use sudo, without logging in as root
  and without setting anything up: the shield button in a pane's header (or "Open as root" /
  "Work as root" where the server refused access) switches the pane to root in the same folder.
  ferge finds the server's `sftp-server`, runs it through `sudo` on the existing connection and
  asks for the sudo password only if sudo needs one (a saved login password is tried first; the
  sudo password can be remembered like any other). The pane header is tinted while it lasts.
- File operations: new folder and file, rename in place, duplicate, delete (folders with their
  contents, after asking), copy address; also from a row's context menu.
- An inspector with the details of the selection and editable permissions; optional Kind and
  Permissions columns; hidden files on demand.
- Passwords and key passphrases can be remembered: in the Secret Service (GNOME Keyring,
  KeePassXC, KWallet) on Linux and the Keychain on macOS. Without a credential service, ferge
  uses an encrypted file (Argon2id + XChaCha20-Poly1305) protected by a master password.
  Passwords are only saved after they worked; a saved password the server rejects is asked for
  again. `FERGE_SECRET_STORE=file` forces the encrypted file.
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

Building needs [rustup](https://rustup.rs/), which installs the compiler version pinned in
`rust-toolchain.toml` by itself. On Linux, ferge also needs a Vulkan driver and these libraries:

```sh
# Arch
sudo pacman -S --needed libxkbcommon libxkbcommon-x11 wayland fontconfig freetype2 libxcb \
    vulkan-icd-loader
# Debian, Ubuntu
sudo apt install pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
    libfontconfig-dev libfreetype-dev libx11-xcb-dev libxcb1-dev libvulkan1
```

Then:

```sh
cargo install --locked --path crates/ferge-app
install -Dm644 packaging/linux/com.wirecurl.ferge.desktop ~/.local/share/applications/com.wirecurl.ferge.desktop
# If ~/.cargo/bin is not on the PATH of your desktop session, use an absolute Exec= path:
sed -i "s|^Exec=ferge|Exec=$HOME/.cargo/bin/ferge|" ~/.local/share/applications/com.wirecurl.ferge.desktop
```

## Usage

```sh
ferge                                   # this computer on the left, the servers on the right
ferge myserver                          # an ssh_config alias, opened on the right
ferge sftp://alice@example.org:2222/srv # user, port and start directory
ferge /var/log                          # local folder
ferge -F ./ssh_config myserver          # use another SSH client configuration
```

| Key (Linux) | Key (macOS) | Action |
|---|---|---|
| ↑ ↓ PgUp PgDn Home End | same | move the selection |
| Shift with ↑ ↓ PgUp PgDn Home End, Shift-click, Ctrl-click, Ctrl+A | same with Shift, ⇧-click, ⌘-click, ⌘A | select several items |
| Insert, Ctrl+Space | Insert | mark or unmark the item and move down |
| Enter, →, Alt+↓ | Enter, →, ⌘↓ | open folder; on a file: preview |
| Backspace, ←, Alt+↑ | Backspace, ←, ⌘↑ | parent folder (Backspace edits the filter while one is active) |
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
| Alt+1 … Alt+8, Alt+9 | ⌥1 … ⌥8, ⌥9 | go to that tab, to the last tab |
| Ctrl+Tab, Ctrl+PgDn / Ctrl+Shift+Tab, Ctrl+PgUp | Ctrl+Tab, ⌘⇧] / Ctrl+Shift+Tab, ⌘⇧[ | next / previous tab |
| Ctrl+K | ⌘K | command palette: commands, servers, tabs and the files of open folders |
| Space | Space | preview the selected file (while no filter is typed) |
| Ctrl+, | ⌘, | settings |

In the server panel, type to search, use ↑ ↓ and press Enter to connect (Ctrl+Enter: new tab).
Favorites and folders are edited from their context menu or the + menu.

## Preview

Space shows the selected file over the panes; the arrow keys keep moving through the folder.
ferge shows text and images, spreadsheets (`xlsx`, `xlsm`, `xlsb`, `ods`, `csv`, `tsv`) as a
grid with a tab per sheet, Word, PowerPoint and OpenDocument files (`docx`, `pptx`, `odt`, `odp`)
as their text with headings, lists and tables, and PDF pages. All of it is built in and works
the same for files on servers. On macOS, Quick Look renders office files, including older
formats and Pages, Numbers and Keynote, as their programs would. Text is read only from its
start; documents up to 64 MB are read whole. Documents and pictures are read in a separate
process with a memory and time limit, so a damaged or hostile file cannot take ferge down. Text in the preview can be selected with the mouse
and copied with Ctrl+C (⌘C); without a selection, Ctrl+C copies the selected files as always.

## Settings

Settings live in `settings.toml` in the configuration directory (`~/.config/ferge` on Linux,
`~/Library/Application Support/ferge` on macOS). The settings window (Ctrl+, or the gear in the
toolbar) edits the same file, and edits made to the file in any editor take effect as soon as
it is saved. ferge keeps comments and the order of what is written by hand, and a file that is
a symbolic link (as in a dotfiles repository) stays one. A setting that is left out uses its
default; a value ferge cannot use is reported and ignored.

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
cargo deny check                 # licenses and security advisories of all dependencies
cargo build --release && cargo run --release -p ferge-bench
./packaging/build.sh             # binary for packages and releases, without this machine's paths
```

CI runs formatting, clippy, the tests, the SFTP integration tests against the test server and
`cargo deny` on every push to `main` and every pull request, and the macOS build on `main`.

The workspace has three crates: `ferge-core` (protocols, listings; no UI), `ferge-app` (GPUI
application) and `ferge-bench` (performance budget checks).

## License

ferge is free software: you can redistribute it and/or modify it under the terms of the
[GNU General Public License](LICENSE) as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version.
