//! Reading FTP listings: `MLSD` (RFC 3659), and `LIST` in the Unix `ls -l`
//! and the DOS format for servers without it.

use std::borrow::Cow;
use std::time::SystemTime;

use suppaftp::list::{File, ListParser, PosixPexQuery};

use crate::entry::{Entry, EntryKind, Metadata};
use crate::path::EntryName;

/// The listing command a server answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    Mlsd,
    List,
}

/// The entry `line` describes, or `None` for lines that are not entries
/// (`total 12`, the folder itself, its parent) and names ferge does not
/// use.
pub(super) fn parse_line(format: Format, line: &str) -> Option<Entry> {
    let (name, metadata) = match format {
        Format::Mlsd => mlsd(line).map(|(name, metadata)| (Cow::Borrowed(name), metadata))?,
        Format::List => list(line).map(|(name, metadata)| (Cow::Owned(name), metadata))?,
    };
    if name == "." || name == ".." {
        return None;
    }
    Some(Entry {
        name: EntryName::new(&name).ok()?,
        metadata,
    })
}

/// One line of `MLSD` (or of an `MLST` reply, without its leading space).
pub(super) fn mlsd(line: &str) -> Option<(&str, Metadata)> {
    // `facts SP name`: every fact ends with ";", so the first "; " ends
    // them; the name may contain anything. A line without facts starts
    // with the space.
    let (facts, name) = if let Some(name) = line.strip_prefix(' ') {
        ("", name)
    } else {
        let end = line.find("; ")?;
        (&line[..=end], &line[end + 2..])
    };
    let mut metadata = Metadata {
        kind: EntryKind::Other,
        size: 0,
        modified: None,
        permissions: None,
    };
    for fact in facts.split(';') {
        let Some((key, value)) = fact.split_once('=') else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "type" => {
                let value = value.to_ascii_lowercase();
                metadata.kind = match value.as_str() {
                    "file" => EntryKind::File,
                    "dir" => EntryKind::Directory,
                    // The folder itself and its parent.
                    "cdir" | "pdir" => return None,
                    // `OS.unix=slink:/target` (proftpd), `OS.unix=symlink`.
                    other if other.starts_with("os.unix=slink") || other == "os.unix=symlink" => {
                        EntryKind::Symlink
                    }
                    _ => EntryKind::Other,
                };
            }
            "size" | "sizd" => metadata.size = value.parse().unwrap_or(0),
            "modify" => metadata.modified = time(value),
            "unix.mode" => {
                metadata.permissions = u32::from_str_radix(value, 8).ok().map(|mode| mode & 0o7777);
            }
            _ => {}
        }
    }
    Some((name, metadata))
}

/// A time as `MLSD`, `MDTM` and `MFMT` write it: `YYYYMMDDHHMMSS[.sss]`, UTC.
pub(super) fn time(value: &str) -> Option<SystemTime> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.len() != 14 || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| whole[range].parse::<i16>().ok();
    let date = jiff::civil::DateTime::new(
        number(0..4)?,
        i8::try_from(number(4..6)?).ok()?,
        i8::try_from(number(6..8)?).ok()?,
        i8::try_from(number(8..10)?).ok()?,
        i8::try_from(number(10..12)?).ok()?,
        i8::try_from(number(12..14)?).ok()?,
        0,
    )
    .ok()?;
    let mut timestamp = jiff::tz::TimeZone::UTC.to_timestamp(date).ok()?;
    if !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        let digits = &fraction[..fraction.len().min(9)];
        let nanos = digits.parse::<i64>().ok()? * 10_i64.pow(9 - u32::try_from(digits.len()).ok()?);
        timestamp = timestamp
            .checked_add(jiff::SignedDuration::from_nanos(nanos))
            .ok()?;
    }
    SystemTime::try_from(timestamp).ok()
}

/// `time` in the form `MFMT` and `MDTM` take.
pub(super) fn format_time(time: SystemTime) -> Option<String> {
    let timestamp = jiff::Timestamp::try_from(time).ok()?;
    Some(timestamp.strftime("%Y%m%d%H%M%S").to_string())
}

/// One line of `LIST`.
fn list(line: &str) -> Option<(String, Metadata)> {
    if line.starts_with("total ") {
        return None;
    }
    let (file, unix) = match ListParser::parse_posix(line) {
        Ok(file) => (file, true),
        Err(_) => (ListParser::parse_dos(line).ok()?, false),
    };
    let kind = if file.is_directory() {
        EntryKind::Directory
    } else if file.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    };
    let metadata = Metadata {
        kind,
        size: file.size() as u64,
        modified: Some(file.modified()).filter(|time| *time != SystemTime::UNIX_EPOCH),
        permissions: unix.then(|| mode(&file)),
    };
    Some((file.name().to_owned(), metadata))
}

/// The permission bits of a Unix `LIST` line.
fn mode(file: &File) -> u32 {
    let mut mode = 0;
    for (shift, who) in [
        (6, PosixPexQuery::Owner),
        (3, PosixPexQuery::Group),
        (0, PosixPexQuery::Others),
    ] {
        let bits = u32::from(file.can_read(who)) << 2
            | u32::from(file.can_write(who)) << 1
            | u32::from(file.can_execute(who));
        mode |= bits << shift;
    }
    mode
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn entry(format: Format, line: &str) -> Entry {
        parse_line(format, line).unwrap_or_else(|| panic!("{line:?} was not read"))
    }

    #[test]
    fn reads_mlsd() {
        let file = entry(
            Format::Mlsd,
            "type=file;size=34;modify=20261123113127.250;UNIX.mode=0644;UNIX.uid=1000; hello; world.txt",
        );
        assert_eq!(file.name.as_str(), "hello; world.txt");
        assert_eq!(file.kind(), EntryKind::File);
        assert_eq!(file.metadata.size, 34);
        assert_eq!(file.metadata.permissions, Some(0o644));
        assert_eq!(
            file.metadata.modified,
            Some(UNIX_EPOCH + Duration::from_millis(1_795_433_487_250))
        );
        let folder = entry(Format::Mlsd, "Type=dir;Modify=20190101000000; nested");
        assert_eq!(folder.kind(), EntryKind::Directory);
        assert_eq!(folder.metadata.permissions, None, "not given, not guessed");
        assert_eq!(
            entry(
                Format::Mlsd,
                "type=OS.unix=slink:/tmp;modify=20190101000000; link"
            )
            .kind(),
            EntryKind::Symlink
        );
        assert_eq!(entry(Format::Mlsd, " no facts").name.as_str(), "no facts");
        for skipped in [
            "type=cdir;modify=20190101000000; /home/ferge",
            "type=pdir;modify=20190101000000; ..",
            "type=file;size=1; a/b",
            "garbage",
        ] {
            assert_eq!(parse_line(Format::Mlsd, skipped), None, "{skipped:?}");
        }
    }

    #[test]
    fn reads_unix_list_lines() {
        let file = entry(
            Format::List,
            "-rw-r--r--    1 1000     1000           13 Nov 23 11:31 Ünïcödé ✓.txt",
        );
        assert_eq!(file.name.as_str(), "Ünïcödé ✓.txt");
        assert_eq!(file.metadata.size, 13);
        assert_eq!(file.metadata.permissions, Some(0o644));
        assert!(file.metadata.modified.is_some());
        let link = entry(
            Format::List,
            "lrwxrwxrwx    1 1000     1000            9 Nov 23 11:31 link-to-hello -> hello.txt",
        );
        assert_eq!(link.name.as_str(), "link-to-hello");
        assert_eq!(link.kind(), EntryKind::Symlink);
        let folder = entry(
            Format::List,
            "drwxr-sr-x    1 1000     1000            0 Nov 20  2019 with space ",
        );
        assert_eq!(folder.kind(), EntryKind::Directory);
        assert_eq!(folder.metadata.permissions, Some(0o755));
        for skipped in [
            "total 12",
            "drwxr-sr-x    1 1000     1000          200 Nov 23 11:31 .",
            "drwxr-sr-x    1 1000     1000           60 Nov 23 11:31 ..",
        ] {
            assert_eq!(parse_line(Format::List, skipped), None, "{skipped:?}");
        }
    }

    #[test]
    fn reads_dos_list_lines() {
        let folder = entry(Format::List, "10-19-20  03:19PM       <DIR>          pub");
        assert_eq!(folder.kind(), EntryKind::Directory);
        let file = entry(
            Format::List,
            "04-08-14  03:09PM                  403 readme.txt",
        );
        assert_eq!(file.metadata.size, 403);
        assert_eq!(file.metadata.permissions, None);
    }

    #[test]
    fn times_go_both_ways() {
        let time = UNIX_EPOCH + Duration::from_secs(1_795_433_487);
        assert_eq!(format_time(time).as_deref(), Some("20261123113127"));
        assert_eq!(super::time("20261123113127"), Some(time));
        assert_eq!(super::time("2026112311312"), None);
        assert_eq!(super::time("20261323113127"), None);
    }
}
