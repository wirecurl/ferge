# Changelog

All notable changes to ferge are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

The first release, 0.1.0, is in preparation.

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

[Unreleased]: https://github.com/wirecurl/ferge/commits/main
