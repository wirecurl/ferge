# ferge – Plan

Name „ferge“ (norwegisch für Fähre, im Deutschen dichterisch für Fährmann). Ein schneller, moderner File-Transfer-Client für Linux und macOS, der sich am Bedienkonzept von Transmit 5 orientiert. Eigenes Produkt mit eigenem Namen und Look, kein 1:1-Nachbau.

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
  ferge-core/   kein UI-Code
    remote/       Trait `Remote`: list (Stream), stat, read, write, rename, delete, mkdir, chmod
                  Backends: local, sftp; später ftp/ftps, s3, webdav
    transfer/     Queue, Connection-Pool pro Host, Parallelität, Pause/Resume, Konfliktregeln
    sync/         Diff-Engine (Größe + mtime, optional Checksum), Dry-Run, Ignore-Regeln
    store/        SQLite: Server, Ordner, History, Queue
    secrets/      keyring mit Fallback
  ferge-app/    GPUI-Oberfläche
  ferge-bench/  Benchmark-Harness gegen Test-Server
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

1. Cargo-Workspace mit `ferge-core`, `ferge-app`, `ferge-bench`; Versionen gepinnt.
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

Direkte Transfers: Daten nehmen den kürzesten Weg statt über diesen Rechner. Der häufigste Fall sind zwei VMs beim selben Hoster, zwischen denen das Netz um ein Vielfaches schneller ist als der Upload zu Hause.

- **Innerhalb eines Servers** kopiert der Server selbst: SFTP-Erweiterung `copy-data` (OpenSSH ab 9.0) in Stücken von 32 MiB auf einem Datenkanal (fortsetzbar, mit Fortschritt), sonst `cp` über einen Exec-Kanal, sonst wie bisher über diesen Rechner. Gilt auch für zwei getrennte Sitzungen desselben Kontos (gleicher Host-Key, Host, Port und Benutzer).
- **Zwischen zwei Servern** schickt die Quelle direkt zum Ziel (Push), oder das Ziel holt bei der Quelle ab (Pull), je nachdem, wer wen erreicht; Push zuerst. Beide Seiten brauchen eine Shell, die aktive zusätzlich `ssh`. Reine SFTP-Zugänge und Root-Sitzungen fallen auf den Weg über diesen Rechner zurück. Hinter einem `ProxyCommand` erreichbare Server nur mit Adressangabe.
- **Rückfrage vorher:** Bevor ferge auf einem Server etwas anlegt, fragt es, ob die beiden Server direkt kopieren dürfen, und sagt, was es dafür ändert. Die Antwort gilt für diesen Transfer, auf Wunsch für dieses Serverpaar bis ferge beendet wird. Nein heißt: über diesen Rechner.
- **Anmeldung** mit einem Ed25519-Schlüssel nur für diesen Auftrag. Der private Teil liegt für die Dauer des Auftrags auf der aktiven Seite (Modus 0600 in einem 0700-Ordner) und wird am Ende gelöscht. Der öffentliche Teil steht auf der passiven Seite mit `restrict`, `expiry-time` und einem festen Befehl in `authorized_keys`. Dieser Befehl kennt nur `probe`, `put`/`get <Nr> <Offset>`, `putb`/`getb <Bündel>` und `cleanup`; Pfade schlägt er in seinen eigenen Tabellen nach. Der Schlüssel kann also nur die Dateien dieses Auftrags schreiben bzw. lesen, nichts sonst. Gegenüber einem weitergeleiteten Agenten hat das den Vorteil, dass der Auftrag ohne ferge weiterlaufen kann. Die aktive Seite vertraut nur dem Host-Key, den ferge für die passive Seite kennt. Eine Verbindung (`ControlMaster`) trägt alle Dateien. Der Schlüssel gilt mindestens 2 h, bei vielen Daten länger (1 MB/s angenommen, höchstens 72 h), in der Ortszeit des Servers (`date +%z`), wie sshd sie liest; solange ferge zusieht, verlängert es ihn. Änderungen an `authorized_keys` laufen unter einer Sperre, damit gleichzeitige Aufträge sich keine Zeilen überschreiben.
- **Kleine Dateien in Bündeln:** Dateien bis 4 MiB, die von vorn beginnen und ihren Namen behalten, gehen ordnerweise in Bündeln (16 bis 512 Dateien, höchstens 64 MiB) als ein `tar`-Strom über eine Sitzung. Die Quelle archiviert mit einem einzigen `tar`-Aufruf, das Ziel entpackt in einen Zwischenordner `<Zielordner>/.ferge-<Auftrag>/<Bündel>`, prüft alle Größen mit einem `wc -c` und legt die Dateien eines Ordners mit einem einzigen `ln` an ihren Platz (ersetzen mit einem `mv -f`). Pro Datei startet so kein eigener Prozess mehr. Symlinks im Strom verwerfen das Bündel, Hardlinks zwischen Dateien werden zu eigenen Kopien, Namen mit Steuerzeichen oder Leerzeichen am Rand gehen einzeln. Ohne `tar` auf beiden Seiten (oder mit `FERGE_DIRECT_BATCH=0`) geht jede Datei einzeln.
- **Ohne diesen Rechner weiterlaufen:** ferge plant vollständig vorab (Listing, alle Konflikte geklärt), legt den Auftrag in `~/.cache/ferge/jobs/<id>` auf beiden Seiten ab und startet den Treiber abgekoppelt (`setsid nohup`). Der Treiber schreibt eine Statuszeile pro Ereignis; ferge liest sie per SFTP mit (25 ms nach jeder Neuigkeit, ohne Neuigkeit seltener bis 250 ms) und schaut alle 5 s, ob der Treiber noch lebt. Verliert ferge die Verbindung oder wird beendet, läuft der Auftrag weiter, entfernt am Ende selbst seinen Schlüssel und den Ordner auf der passiven Seite und wartet 2 min auf ferge; danach räumt er sich bis auf eine kurze Ergebnisdatei selbst ab. Der nächste Auftrag auf diesem Server übernimmt das Ergebnis in seinen Trace und entfernt es. Pause und Abbruch beenden den Treiber über seine Prozessgruppe; Teildateien bleiben, Fortsetzen startet einen neuen Auftrag ab dem letzten Byte. Dateien, die ein gestoppter Auftrag noch fertig bekam, ohne es zu melden, erkennt ferge an Größe und Zeitstempel.
- **Andocken nach einem Neustart:** Vor dem Start schreibt ferge den Auftrag nach `~/.local/state/ferge/jobs/<id>.json`: beide Server so, wie sie verbunden wurden (Host bzw. Alias, Benutzer, Port), jede Datei mit Quelle, Ziel, Größe und Startpunkt, die Bündel und den Zustand beider Server vor dem Auftrag. Beim nächsten Start verbindet sich ferge im Hintergrund mit beiden Servern (Passwörter wie bei jeder Verbindung), liest den Status des Auftrags von vorn, zeigt ihn in der Transferliste, folgt ihm bis zum Ende und räumt auf wie sonst auch, mit der Prüfung „wie vorgefunden“. Ein Auftrag, der inzwischen endete und sich selbst abgeräumt hat, behält dafür seinen Status. Ein so verfolgter Auftrag lässt sich abbrechen, aber nicht pausieren. Sind die Server nicht erreichbar, bleibt der Eintrag für den nächsten Start.
- **Transfers über diesen Rechner** können ohne ferge nicht weiterlaufen. Damit trotzdem nichts verloren geht und nichts liegen bleibt, speichert ferge sie, sobald sie kopieren: den Plan einmal als `~/.local/state/ferge/jobs/t<…>.json`, dazu ein Journal mit jeder Konfliktentscheidung, jeder fertigen Datei und alle 8 MiB den lückenlos bestätigten Stand der laufenden Dateien. Schließen des Fensters während solcher Transfers fragt nach und pausiert sie. Beim nächsten Start stehen gespeicherte Transfers pausiert in der Liste („Paused when ferge quit“), ohne Verbindung; Fortsetzen verbindet beide Seiten (wie ein Tab, mit Passwortfragen) und macht ab dem bestätigten Byte weiter, Abbrechen verbindet und entfernt die Teildateien. Fertige, abgebrochene und aus der Liste entfernte Transfers verschwinden aus dem Speicher.
- **Verbindung verloren** (Zuklappen, Netzwechsel): Ein Transfer schlägt dann nicht fehl, sondern pausiert mit „Connection lost“. ferge verbindet beide Seiten im Hintergrund neu (alle 5 s, bis zu 30 min, solange Netz oder Server fehlen; bei abgelehnter Anmeldung nicht) und macht weiter. Ein direkter Transfer dockt dabei an seinen laufenden Auftrag an, statt einen zweiten zu starten.
- **Server wie vorgefunden:** Vor allem anderen notiert jede Seite den Zustand dessen, was ferge berührt (Login-Ordner, `~/.cache`, `~/.cache/ferge`, `~/.cache/ferge/jobs`, `~/.ssh`, `~/.ssh/authorized_keys`): ob es existiert, was es enthält (Prüfsumme bzw. Einträge) und seine Zeiten. Am Ende entfernt ferge den Auftragsordner, die Ordner, die es dafür angelegt hat, die Schlüsselzeile und was dafür hinzukam (ein fehlender letzter Zeilenumbruch, die Datei, der Ordner `.ssh`), und gibt jedem dieser Pfade seine alten Zeiten zurück, wenn er wieder enthält, was er vorher enthielt. Dann notiert es den Zustand erneut und meldet jeden Unterschied im Trace. Jeder Auftrag räumt außerdem auf, was frühere zurückließen: Schlüssel, deren Auftrag fehlt oder abgelaufen ist, Ordner von Aufträgen, die nicht mehr laufen, eine verwaiste Sperre. `ferge-direct inspect|purge <server>` zeigt bzw. entfernt alle ferge-Spuren auf einem Server.
- **Rückfall:** Scheitert eine Prüfung, läuft der Transfer wie bisher über diesen Rechner. Die Transferliste zeigt den Weg und, solange der Transfer läuft, warum kein direkter Weg ging.
- **Sicherheit:** Pfade gehen nie in eine Kommandozeile. In generierten Skripten stehen sie POSIX-sicher in einfachen Anführungszeichen, für `cp` oktal kodiert über die Standardeingabe. Was ferge außerhalb der Auftragsordner ausführt, geht als Skript über die Standardeingabe von `sh -s`, sodass die Login-Shell (auch fish oder csh) nur diese zwei Wörter sieht. Jede einzeln kopierte Datei landet erst als `<name>.ferge-part`, wird gegen die geplante Größe geprüft und dann ohne Überschreiben umbenannt (`ln`), außer Überschreiben war entschieden; gebündelte genauso aus ihrem Zwischenordner.
- **Umsetzung:** POSIX-Shell-Skripte, die nur voraussetzen, was auf dem Server schon da ist (getestet mit bash und dash lokal und busybox im Container; GNU `tar` bekommt `--no-unquote`, busybox-`ls` taugt wegen ersetzter Zeichen nicht zum Namensvergleich, daher `wc`). Auftragsordner und Statusformat sind so geschnitten, dass später ein kleines ferge-Hilfsprogramm dahinterpassen kann.
- **Tests:** zweiter Container im Testserver, der den ersten erreicht; Integrationstests für Push, Pull (mit Bündeln und Einzeldateien), Rückfall, Ablehnen der Rückfrage, Kopie innerhalb eines Servers, Pause/Fortsetzen und einen Auftrag, der ohne ferge zu Ende läuft und dessen Ergebnis der nächste Auftrag abholt. Jeder prüft, dass beide Server wie vorgefunden bleiben; die Tests laufen dafür nacheinander. Die Skripte selbst laufen in Unit-Tests lokal mit absichtlich bösartigen Dateinamen, darunter Byte-für-Byte- und Zeitstempelvergleiche eines ganzen Login-Ordners vor und nach einem Auftrag. `ferge-bench` vergleicht alle Wege zwischen den Containern.
- **Mitschnitt und Auswertung:** Jeder Transfer schreibt einen Trace (JSON Lines) nach `~/.local/state/ferge/transfers`: Rückfrage, Routenwahl mit allen Proben, jeder Befehl auf den Servern mit Ausgabe und Dauer, jede Datei und jedes Bündel mit Zeiten, Fortschritt pro Sekunde, Phasen, Schlüssel und Verlängerungen, was aufgeräumt wurde, ob jeder Server wie vorgefunden blieb, und am Ende die Logs der Server. `ferge-trace` fasst Traces zusammen. Dazu eine Logdatei `ferge.log` im selben Ordner (`FERGE_LOG`) und Schalter über die Umgebung (`FERGE_ROUTE`, `FERGE_DIRECT_*`, `FERGE_TRACE*`).

Stand und nächste Schritte:

- Gemessen im Loopback zwischen zwei Containern auf einem Rechner (2000 kleine Dateien / 256 MiB): direkt rund 3.600 Dateien/s und 410 MB/s, über diesen Rechner rund 3.400 Dateien/s und 460 MB/s. Über echte Leitungen fällt beim direkten Weg der Upload dieses Rechners weg; eine Messung mit gedrosselter Leitung zu diesem Rechner steht noch aus.
- Weitere Adressen für einen Server (z. B. privates Netz) gibt es bisher nur über `FERGE_DIRECT_ADDRESS`; sie gehören an den Favoriten.
- Die Antwort auf die Rückfrage gilt höchstens bis ferge beendet wird; dauerhaft merken (pro Server in der Library) fehlt noch.
- Endet ein Auftrag ohne ferge so, dass er die Gegenseite nicht mehr erreicht (Netz weg, Neustart), bleibt dort die Schlüsselzeile bis zum Ablauf und bis zum nächsten Auftrag auf diesem Server oder `ferge-direct purge` stehen.

Fertig, wenn: Ein Ordner wandert zwischen den beiden Test-Containern direkt, mit einer Geschwindigkeit nahe `scp` zwischen ihnen, der Transfer läuft weiter, während ferge beendet ist, und nach dem Neustart zeigt ferge den richtigen Stand. Auf dem Ziel bleibt kein Schlüssel zurück.

### Phase 4 – Weitere Protokolle

FTP/FTPS, S3-kompatibel (AWS, R2, B2, MinIO), WebDAV. Integrationstests gegen vsftpd, MinIO und einen WebDAV-Server in Docker.

Reihenfolge: erst die Grundlagen, die alle drei brauchen, dann FTP/FTPS, S3 und WebDAV, jedes mit Testserver, Integrationstests und Oberfläche, bevor das nächste anfängt.

**Grundlagen**

- **Adressen:** `Destination` bekommt je Protokoll eine Variante, mit Adressen, die `parse` wieder liest:
  - `ftp://[user@]host[:port][/pfad]` (explizites TLS, Port 21) und `ftps://…` (implizites TLS, Port 990);
  - `s3://[access-key@]endpunkt[:port]/[bucket[/pfad]]`, `s3+http://…` nur für lokale Server wie MinIO;
  - `davs://[user@]host[:port][/pfad]` (HTTPS) und `dav://…` (HTTP), wie bei GNOME und KDE.

  Favoriten, Verlauf, Quick Connect und das Adressfeld nehmen alle Formen an. Die bestehende Tabelle `bookmarks` speichert sie schon als Adresse; Zusatzfelder wie Region oder „unverschlüsselt erlaubt“ kommen in eine Migration.
- **Fähigkeiten statt Annahmen:** `Remote::capabilities()` sagt, was ein Backend kann: an einem Offset weiterschreiben, atomar schreiben (das Objekt erscheint erst, wenn es vollständig ist), billig umbenennen, Zeitstempel setzen, Rechte setzen, Symlinks. Die Transfer-Engine richtet sich danach. Wo atomar geschrieben wird (S3), entfällt die `.ferge-part`-Datei: ferge schreibt direkt unter den Zielnamen, ohne Überschreiben mit `If-None-Match: *`, wo der Server es kennt, sonst nach der Prüfung beim Planen. Wo nicht weitergeschrieben werden kann (S3-Upload, WebDAV), beginnt ein fortgesetzter Upload die Datei von vorn; Downloads setzen überall per Range bzw. `REST` fort. Der Inspector bietet Rechte nur an, wo sie gesetzt werden können.
- **Wiederverbinden:** `Remote::destination()` beschreibt, wie man diese Verbindung wieder herstellt. Gespeicherte Transfers und „Connection lost“ nutzen das, ebenso ein gemeinsamer Verbindungsaufbau (`connect::connect`) für Tabs und Transfers, statt SFTP-Endpunkte vorauszusetzen. Direkte Transfers und „als root arbeiten“ bleiben bei SFTP.
- **Zugangsdaten:** `SecretId` unterscheidet nach Protokoll (FTP-Passwort, S3-Secret-Key zum Access Key und Endpunkt, WebDAV-Passwort). Die bestehenden SFTP-Einträge behalten ihren Namen. Gefragt wird wie bei SFTP über `Interaction::password`, mit dem Protokoll im `Account`.
- **TLS:** rustls mit den Wurzelzertifikaten des Systems, eine Konfiguration für FTPS, S3 und WebDAV. Ein Zertifikat, dem das System nicht traut (selbst signiert, abgelaufen, falscher Name), führt zu einer Rückfrage wie bei unbekannten Host-Keys: Aussteller, Gültigkeit, Namen und SHA-256-Fingerabdruck, „einmal“ oder „merken“. Gemerkte Zertifikate liegen pro Host und Port in der Library; ändert sich ein gemerktes Zertifikat, wird abgelehnt und gesagt, warum. Unverschlüsseltes FTP und `dav://` gehen nur nach ausdrücklicher Zustimmung pro Server, weil Passwort und Daten dann im Klartext laufen.

Stand: Fähigkeiten, `Remote::destination()` und der gemeinsame Verbindungsaufbau sind da. Zugangsdaten nach Protokoll und TLS kommen mit FTP/FTPS, weil sie erst dort gebraucht und gegen einen echten Server geprüft werden.

**FTP/FTPS** (`suppaftp` mit tokio und rustls)

- Listing über `MLSD`, sonst `LIST` mit dem Unix- und dem DOS-Format, Zeitstempel über `MLSD` oder `MDTM`. Das Listing wird beim Lesen in Stücken an die UI gereicht.
- Ein Steuerkanal für den Browser, eigene Verbindungen für Transfers (FTP kann pro Verbindung nur eine Datei übertragen); Passiv-Modus (`EPSV`, sonst `PASV`), TLS-Session-Wiederverwendung auf dem Datenkanal, weil vsftpd und andere sie verlangen.
- Fortsetzen mit `REST` + `STOR` bzw. `RETR`, Umbenennen mit `RNFR`/`RNTO` (ohne Überschreiben vorher geprüft), Zeitstempel mit `MFMT`, wo angeboten, Rechte mit `SITE CHMOD`, Keepalive mit `NOOP`.
- Testserver: vsftpd mit explizitem und implizitem TLS und einem selbst signierten Zertifikat, Passivports fest gemappt, dieselben Fixtures `many/` und `tree/` wie beim SSH-Server.

**S3** (`opendal`, Dienst `s3`)

- Endpunkte für AWS, R2, B2 und MinIO; Region aus der Adresse oder vom Server ermittelt. Pfad-Stil, für AWS virtuelle Hosts.
- `/` zeigt die Buckets (`ListBuckets`, falls der Schlüssel das darf; sonst muss der Bucket in der Adresse stehen), darunter Präfixe als Ordner. Listings kommen seitenweise (1000 Einträge) und damit gestreamt.
- Ordner anlegen legt ein leeres `prefix/`-Objekt an, Löschen eines Ordners entfernt es. Umbenennen ist serverseitiges Kopieren und Löschen; Ordner umbenennen geht nur als Transfer mit Fortschritt, nicht als stilles „Rename“.
- Große Dateien als Multipart-Upload mit mehreren Teilen gleichzeitig. Pause bricht den Multipart-Upload ab (`AbortMultipartUpload`), damit keine unsichtbaren Teile Speicher kosten.
- Änderungszeit: Listings zeigen `LastModified` (die Upload-Zeit). Beim Hochladen schreibt ferge die Änderungszeit der Quelle als `x-amz-meta-mtime` (wie rclone); der Inspector zeigt sie.
- Testserver: MinIO mit einem Bucket, der `many/` und `tree/` enthält, und einem zweiten, leeren.

**WebDAV** (`opendal`, Dienst `webdav`)

- `PROPFIND` mit `Depth: 1`, Umbenennen mit `MOVE` (`Overwrite: F` bzw. `T`), Ordner mit `MKCOL`. Zeitstempel nur, wo der Server sie annimmt (Nextcloud und ownCloud über `X-OC-Mtime`).
- Ob `opendal` große `PROPFIND`-Antworten in Stücken liefert, ist zu prüfen. Wenn nicht, liest ferge sie selbst mit `quick-xml` als Strom, damit die ersten Zeilen erscheinen, bevor die Antwort vollständig ist.
- Testserver: Apache mit `mod_dav` über HTTP und HTTPS (selbst signiert), mit Basic- und Digest-Anmeldung.

**Oberfläche**

- Quick Connect und der Favoriten-Dialog bekommen eine Protokollwahl (`style::segmented`: SFTP, FTP, S3, WebDAV); die Felder folgen dem Protokoll (bei S3 Endpunkt, Access Key, Secret Key, Bucket, Region). Eine eingetippte Adresse wählt das Protokoll selbst.
- Listen und Tabs zeigen das Protokoll mit einem Symbol; der Verbindungsfehler sagt, was auf welcher Ebene scheiterte (DNS, TCP, TLS, Anmeldung, Rechte).

**Tests und Messung:** Jedes Backend läuft durch dieselbe Testsammlung für `Remote` (Listing, Lesen ab Offset, Schreiben, Konflikte, Umbenennen, Löschen, böse Namen) und durch die Transfertests gegen die Container; sie sind `#[ignore]`d wie die SFTP-Tests und laufen in CI. `ferge-bench` misst für jedes Protokoll die ersten Zeilen und das ganze Listing von `many/` sowie den Durchsatz (256 MiB hoch und runter, verglichen mit `curl` bzw. `lftp`).

Fertig, wenn: ferge mit allen vier Protokollen gegen die Testcontainer verbindet, `many/` überall flüssig zeigt, Dateien und Ordner zwischen beliebigen zwei Seiten kopiert (auch FTP nach S3), Transfers nach einer verlorenen Verbindung und nach einem Neustart weiterlaufen, und unbekannte Zertifikate genauso sorgfältig behandelt werden wie unbekannte Host-Keys.

### Phase 5 – Sync

Richtungen Upload, Download, Mirror. Vorschau als Dry-Run vor der Ausführung, Löschregeln, Ignore-Muster.

### Phase 6 – Komfort und Auslieferung

Remote-Datei im externen Editor öffnen mit Auto-Upload beim Speichern, Info- und Rechte-Inspector, Paketierung (AUR, Flatpak, signiertes macOS-Bundle).

Als root arbeiten: Ein Pane wechselt per Klick auf root, ohne Root-Login und ohne Einrichtung auf dem Server. ferge startet über die bestehende SSH-Verbindung zusätzliche SFTP-Kanäle mit `sudo sftp-server` (Pfad automatisch ermittelt, Passwort nur wenn sudo es verlangt, vorher mit `sudo true` geprüft) und hält das Passwort danach nicht im Speicher.

## Risiken

- **GPUI-API vor 1.0.** Doku dünn, oft hilft nur der Quellcode von Zed und `gpui-component`. Versionen pinnen, Upgrades als eigene Aufgabe.
- **Drag-out von Remote-Dateien** in den Dateimanager: Datei existiert lokal noch nicht, Mechanismus pro Plattform verschieden. Früh als Spike prototypen.
- **Resume und Fehlerfälle.** Verbindungsabbruch mitten im Transfer, halbe Dateien, FTP-Server-Eigenheiten. Integrationstests gegen echte Server in Containern von Anfang an.
- **Abgekoppelte Aufträge auf fremden Servern.** Manche Systeme beenden Prozesse beim Abmelden (systemd `KillUserProcesses`, Container ohne Init). Früh auf den gängigen Distributionen prüfen; wenn es nicht überlebt, zeigt ferge vorher an, dass der Transfer das Zuklappen nicht übersteht.
- **macOS-Feeling.** Native Menüs und Shortcuts gezielt nachziehen, sobald Linux steht.
