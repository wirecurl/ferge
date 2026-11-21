# Ferry – Plan

Arbeitsname „Ferry“. Ein schneller, moderner File-Transfer-Client für Linux und macOS, der sich am Bedienkonzept von Transmit 5 orientiert. Eigenes Produkt mit eigenem Namen und Look, kein 1:1-Nachbau.

Hauptplattform: Arch Linux mit dem Wayland-Compositor **niri**. macOS ist Zweitplattform.

## Leitprinzip

Es muss absolut performant laufen und sich durchgehend smooth bedienen lassen. Jede Architekturentscheidung wird daran gemessen.

## Technologie

| Bereich | Wahl | Begründung |
|---|---|---|
| Sprache | Rust | kein GC, volle Kontrolle über Streaming, Speicher und Protokollschicht |
| UI | GPUI (Zed-Framework) | GPU-gerendert, Wayland-nativ, keine Webview; unter macOS über Metal |
| Widgets | `gpui-component` | Tabellen, Inputs, Docks, Kontextmenüs, Theme |
| Async | `tokio` | Netzwerk und Dateisystem, getrennt vom UI-Thread |
| SFTP | `russh` + `russh-sftp` | reines Rust, Pipelining möglich |
| SSH-Config | `russh-config` | `~/.ssh/config`, Host-Aliase, ProxyCommand |
| FTP/FTPS | `suppaftp` | Phase 4 |
| S3, WebDAV | `opendal` | Phase 4 |
| Persistenz | SQLite (`rusqlite`) | Server, Ordner, History, persistente Queue |
| Secrets | `keyring` | Secret Service (Linux) / Keychain (macOS), Fallback siehe unten |

Verworfen: Tauri (WebKitGTK unter Linux zu langsam, Wayland-Probleme), GTK4/libadwaita (auf macOS ein Fremdkörper), Qt/QML (C++-Brücke), Iced (zu wenig fertige Widgets für dichte Dateiansichten), Go (nur rclone spricht dafür, UI-Optionen schwach).

### Versionsstand (geprüft am 2026-11-20)

- `gpui-component = "0.10.0"` pinnt `gpui = { package = "gpui-pre", version = "=0.3.12" }` und `gpui_macros = { package = "gpui-pre-macros", version = "=0.3.12" }`. Die App muss exakt diese GPUI-Version verwenden, nicht `gpui 0.2.x` von crates.io.
- `russh = "0.65.2"`, `russh-sftp = "3.0.1"`, `russh-config = "0.58.0"`, `tokio = "1.54"`.
- Alle Versionen fest pinnen. GPUI ist vor 1.0, Updates werden bewusst eingeplant, nicht nebenbei gezogen.

Toolchain lokal: Rust 1.99.1, Docker vorhanden, Wayland-Session unter niri.

## Architektur

```
crates/
  ferry-core/   kein UI-Code
    remote/       Trait `Remote`: list (Stream), stat, read, write, rename, delete, mkdir, chmod
                  Backends: local, sftp; später ftp/ftps, s3, webdav
    transfer/     Queue, Connection-Pool pro Host, Parallelität, Pause/Resume, Konfliktregeln
    sync/         Diff-Engine (Größe + mtime, optional Checksum), Dry-Run, Ignore-Regeln
    store/        SQLite: Server, Ordner, History, Queue
    secrets/      keyring mit Fallback
  ferry-app/    GPUI-Oberfläche
  ferry-bench/  Benchmark-Harness gegen Test-Server
```

Der Core kennt keine UI. Er streamt Events (Listing-Chunks, Fortschritt, Auth-Rückfragen wie Host-Key-Bestätigung oder Passphrase). Die UI bleibt dadurch austauschbar, und der Core ist ohne Fenster testbar.

### Performance-Regeln

- **Kein I/O auf dem UI-Thread.** Alles Netzwerk- und Dateisystem-I/O läuft auf tokio, die UI bekommt nur Ergebnisse.
- **Streaming-Listings.** Verzeichnisinhalte kommen in Chunks, die Liste füllt sich, während der Server noch antwortet.
- **Listing-Cache pro Verbindung.** Bekannte Pfade erscheinen sofort aus dem Cache und werden im Hintergrund revalidiert.
- **Optimistische UI.** Umbenennen, Löschen und Verschieben erscheinen sofort, bei Fehler Rollback mit Hinweis.
- **SFTP-Pipelining.** Mehrere Read/Write-Requests parallel pro Datei, das ist der größte Durchsatz-Hebel.
- **Connection-Pool pro Host.** Transfers laufen über eigene Kanäle, die Browser-Verbindung bleibt frei.
- **Virtualisierte Listen.** Nur sichtbare Zeilen werden gerendert (`uniform_list`), Sortieren und Filtern arbeiten auf Indizes, nicht auf kopierten Daten.

### Performance-Budgets

Ab Phase 1 automatisiert gemessen:

| Budget | Ziel |
|---|---|
| Kaltstart bis bedienbares Fenster | < 150 ms |
| Tastendruck bis Frame | 1 Frame (8 ms bei 120 Hz) |
| Ordner mit 100.000 Einträgen | Scrollen und Sortieren ohne Frame-Drops |
| Ordnerwechsel auf bekanntem Pfad | sofort aus dem Cache |
| Erste Zeilen eines Remote-Listings | sichtbar, bevor das Listing komplett ist |

## niri und Wayland

- **Beliebige Spaltenbreiten.** Responsives Layout: bei schmaler Spalte ein Pane mit Umschalter statt Dual-Pane, Transfer-Panel klappt ein.
- **Tastatur zuerst.** Alles ohne Maus erreichbar: Command Palette, Type-to-Filter, optional vim-artige Navigation.
- **Dekorationen.** `xdg-decoration` respektieren, keine eigene Titelleiste erzwingen (niri läuft oft mit `prefer-no-csd`).
- **Kein Tray, kein globales Menü.** Transferstatus im Fenster plus Desktop-Notifications.
- **Secrets.** Secret Service erkennen (gnome-keyring, KeePassXC). Fehlt er, Fallback auf eine verschlüsselte lokale Datei mit Master-Passwort.
- **Portale.** Dateidialoge über `xdg-desktop-portal`, Drag & Drop über das Wayland-Protokoll.

## Phasen

### Phase 1 – Fundament (jetzt)

1. Cargo-Workspace mit `ferry-core`, `ferry-app`, `ferry-bench`; Versionen gepinnt.
2. GPUI-Hello-World unter niri bauen und starten: prüfen, dass Wayland, Fractional Scaling und Dekorationen sauber funktionieren.
3. `Remote`-Trait mit Streaming-Listing; Backends `local` und `sftp`.
4. SFTP-Auth: Key, ssh-agent, Passwort, `~/.ssh/config`-Aliase, `known_hosts`-Prüfung mit Rückfrage bei unbekanntem Host-Key.
5. Virtualisierte Dateiliste: Tastaturnavigation, Sortierung (Name, Größe, Datum), Type-to-Filter, Ordner betreten und zurück.
6. Test-Server per Docker: OpenSSH-Container mit einem Ordner aus 100.000 Dateien.
7. Benchmark-Harness für die Budgets oben.

Fertig, wenn: Die App verbindet sich per SFTP mit dem Test-Container, zeigt den 100.000er-Ordner flüssig an, und die Benchmarks halten die Budgets.

### Phase 2 – Server und History

Favoriten mit Ordnern, Quick Connect, zuletzt verbundene Server, Secrets im Keyring, Tabs.

### Phase 3 – Transfers

Queue-Panel, parallele Transfers, Pause/Resume, Konfliktdialog (überschreiben, überspringen, umbenennen, „für alle“), Drag & Drop aus dem Dateimanager.

### Phase 4 – Weitere Protokolle

FTP/FTPS, S3-kompatibel (AWS, R2, B2, MinIO), WebDAV. Integrationstests gegen vsftpd und MinIO in Docker.

### Phase 5 – Sync

Richtungen Upload, Download, Mirror. Vorschau als Dry-Run vor der Ausführung, Löschregeln, Ignore-Muster.

### Phase 6 – Komfort und Auslieferung

Remote-Datei im externen Editor öffnen mit Auto-Upload beim Speichern, Info- und Rechte-Inspector, Paketierung (AUR, Flatpak, signiertes macOS-Bundle).

Als root arbeiten: Ein Pane wechselt per Klick auf root, ohne Root-Login und ohne Einrichtung auf dem Server. Ferry startet über die bestehende SSH-Verbindung zusätzliche SFTP-Kanäle mit `sudo sftp-server` (Pfad automatisch ermittelt, Passwort nur wenn sudo es verlangt, vorher mit `sudo true` geprüft) und hält das Passwort danach nicht im Speicher.

## Risiken

- **GPUI-API vor 1.0.** Doku dünn, oft hilft nur der Quellcode von Zed und `gpui-component`. Versionen pinnen, Upgrades als eigene Aufgabe.
- **Drag-out von Remote-Dateien** in den Dateimanager: Datei existiert lokal noch nicht, Mechanismus pro Plattform verschieden. Früh als Spike prototypen.
- **Resume und Fehlerfälle.** Verbindungsabbruch mitten im Transfer, halbe Dateien, FTP-Server-Eigenheiten. Integrationstests gegen echte Server in Containern von Anfang an.
- **macOS-Feeling.** Native Menüs und Shortcuts gezielt nachziehen, sobald Linux steht.
