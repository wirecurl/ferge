# Changelog

All notable changes to ferge are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- FTP with TLS: `ftp://` with `AUTH TLS`, `ftps://` with implicit TLS. Listings stream with
  `MLSD` or `LIST`, uploads and downloads resume (an upload the server will not resume starts
  again), several transfers share a small pool of connections, and times and permissions are
  kept where the server allows it. Tested against vsftpd and ProFTPD. ferge offers FTP servers
  TLS 1.2, so that every data connection can resume the control connection's session as the
  servers demand, and TLS 1.3 only to servers that know nothing older.
- Certificates the system does not trust are shown with their names, validity and fingerprint
  before ferge connects, and can be trusted once or for good; a remembered certificate that
  changes is refused. A server without encryption is used only after asking.
- Quick Connect and the favorite editor offer SFTP, FTP and FTPS; an address typed with a
  scheme picks the protocol itself.

### Fixed

- Started without a display, as in an SSH session, ferge says so and exits instead of running
  without a window.
- A certificate the system does not trust no longer shows up as an error in the log; ferge
  notes it and asks.
- Enter answers the question about copying directly between two servers with "Copy directly",
  the button it highlights.
- ferge asks whether two servers may copy directly before it makes any folder on the target;
  until now the folders of the transfer were there already while the question was open.

## [0.1.0] - 2026-11-22

The first release.

### Added

- Dual-pane file browser: this computer on the left, an SFTP server on the right, in tabs.
  Narrow windows show one side with a switch.
- SFTP via russh: SSH agent, key files with passphrase prompt, password and
  keyboard-interactive authentication; `~/.ssh/config` with host aliases, `ProxyCommand`,
  `Include` and `Match all`.
- Strict host key checking against `known_hosts` with OpenSSH semantics; unknown keys are
  confirmed, changed keys are refused.
- Streaming, cached listings; folders with 100,000 entries scroll, sort and filter without
  dropped frames. Natural sorting and type-to-filter.
- Transfers between any two panes: uploads, downloads and server-to-server copies of files and
  folders, with a queue, four files in parallel, pause and resume within a file, and a conflict
  dialog. Pipelined SFTP reads and writes on separate data channels.
- Direct transfers between two servers, after asking: the source pushes or the target pulls
  over their own SSH connection, with a key that only works for the job's files and expires;
  small files travel in `tar` batches. The copy goes on when ferge quits, ferge follows it
  again after a restart, and it falls back to this computer when the servers cannot reach each
  other. Each server is left as it was found, down to file times, and checked for it. Copies
  within one server use `copy-data` or `cp` and never leave it.
- Transfers go on after a lost connection once the servers are back, and transfers through this
  computer are saved: closing ferge pauses them, and they resume where they stopped after a
  restart.
- Transfer traces and a log file for debugging, summarized by `ferge-trace`; `ferge-direct`
  shows or removes what direct transfers left on a server.
- Drag and drop from and to the desktop's file manager.
- File operations: new folder and file, rename, duplicate, delete, copy address; an inspector
  with editable permissions.
- Favorites in folders, recent connections and Quick Connect, stored in SQLite.
- Saved passwords in the Secret Service or the macOS Keychain, or in a file encrypted with
  Argon2id and XChaCha20-Poly1305 when no credential service is available.
- Working as root on SSH servers through sudo, without a root login and without setup on the
  server.
- Previews of text, images, spreadsheets, Word, PowerPoint and OpenDocument files and PDF
  pages, with text selection.
- Command palette, keyboard navigation for everything, and a settings window backed by a
  hand-editable, watched `settings.toml`.
- Follows the desktop's fonts and light or dark preference.
- An app icon: the f of ferge crossing a wave, for the Linux icon theme (with a symbolic
  variant) and X11 windows.
- Packages: a tarball, `.deb` and `.rpm` for x86_64 and aarch64 Linux (glibc 2.35 and newer),
  `ferge.app` for macOS on Apple silicon and Intel, AUR recipes and a Homebrew cask.

### Security

Hardening from a review of the code before the first release:

- Host names that known_hosts would read as patterns (`*`, `?`, `,` and the like) are refused,
  so a confirmed host key can never be trusted for other hosts.
- Spreadsheets, office documents, PDF pages and pictures are decoded in a separate worker
  process with a memory and time limit; the readers themselves are bounded too, and SVG
  drawings cannot read files.
- Partial files of transfers are created exclusively and never written through a symlink;
  a symlink in place of a folder is never followed, and existing folders are merged only
  after asking.
- Listings, transfer plans and copies are bounded against servers that send without end;
  connecting times out on a silent server.
- Working as root asks sudo about the very sftp-server it runs, so no password can reach
  sftp-server; sessions as root close once they are left.
- The encrypted password file is locked against concurrent writers, the settings file is
  followed through a symlink only in a private folder, and names with hidden characters are
  shown so that they cannot disguise themselves.

[Unreleased]: https://github.com/wirecurl/ferge/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/wirecurl/ferge/releases/tag/v0.1.0
