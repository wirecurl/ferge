//! Human-readable sizes, dates, kinds and permissions for the file list.

use std::sync::OnceLock;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The local time zone, loaded once off the UI thread by [`load_time_zone`].
/// Until then dates are shown in UTC.
static TIME_ZONE: OnceLock<TimeZone> = OnceLock::new();

/// A file name, path or other text from a server as it is shown. Control
/// characters become their visible symbols (a line break `␊`, a tab `␉`), so
/// a name stays on one line and shows what it holds; other control and
/// invisible formatting characters, such as a right-to-left override that
/// makes `gnp.exe` read `exe.png`, appear as `�` instead of acting on the
/// text around them. Only for display; the real name is used otherwise.
pub(crate) fn shown(name: &str) -> String {
    let odd = |c: char| c.is_control() || ferge_core::interaction::is_invisible_format_char(c);
    if !name.chars().any(odd) {
        return name.to_owned();
    }
    name.chars()
        .map(|c| match u32::from(c) {
            code @ 0..=0x1f => char::from_u32(0x2400 + code).unwrap_or('\u{FFFD}'),
            0x7f => '\u{2421}',
            _ if odd(c) => '\u{FFFD}',
            _ => c,
        })
        .collect()
}

/// Loads the system time zone. Reads `/etc/localtime`, so call it from a
/// background thread; formatting afterwards is pure computation.
pub(crate) fn load_time_zone() {
    let _ = TIME_ZONE.set(TimeZone::system());
}

/// Formats a byte count with decimal units, e.g. `1.2 MB`.
pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    // Tenths of the unit, rounded half up, in exact integer arithmetic. Move to
    // the next unit when rounding would print 1000.0.
    let bytes = u128::from(bytes);
    let mut divisor: u128 = 1000;
    let mut unit = 0;
    loop {
        let tenths = (bytes * 10 + divisor / 2) / divisor;
        if tenths < 10_000 || unit + 1 == UNITS.len() {
            return format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit]);
        }
        unit += 1;
        divisor *= 1000;
    }
}

/// Formats a count with thousands separators, e.g. `100,000`.
pub(crate) fn count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Formats a modification time as `YYYY-MM-DD HH:MM` in the local time zone.
pub(crate) fn date(time: Option<SystemTime>) -> String {
    let Some(timestamp) = time.and_then(|time| Timestamp::try_from(time).ok()) else {
        return String::new();
    };
    let local = match TIME_ZONE.get() {
        Some(tz) => tz.to_datetime(timestamp),
        None => TimeZone::UTC.to_datetime(timestamp),
    };
    local.strftime("%Y-%m-%d %H:%M").to_string()
}

/// Permission bits as `rwxr-x---`, including setuid, setgid and sticky.
pub(crate) fn permissions(mode: u32) -> String {
    let bit = |mask: u32, letter: char| if mode & mask != 0 { letter } else { '-' };
    let execute = |mask: u32, special: u32, set: char, unset: char| match (
        mode & mask != 0,
        mode & special != 0,
    ) {
        (true, true) => set,
        (false, true) => unset,
        (true, false) => 'x',
        (false, false) => '-',
    };
    [
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        execute(0o100, 0o4000, 's', 'S'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        execute(0o010, 0o2000, 's', 'S'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        execute(0o001, 0o1000, 't', 'T'),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn names_stay_on_one_line() {
        assert_eq!(shown("line\nbreak.txt"), "line\u{240a}break.txt");
        assert_eq!(shown("tab\tand\u{7f}"), "tab\u{2409}and\u{2421}");
        assert_eq!(shown("ümlaut 日本 🦀"), "ümlaut 日本 🦀");
    }

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1000), "1.0 KB");
        assert_eq!(size(1234), "1.2 KB");
        assert_eq!(size(1250), "1.3 KB");
        assert_eq!(size(999_949), "999.9 KB");
        assert_eq!(size(999_950), "1.0 MB");
        assert_eq!(size(5_500_000_000), "5.5 GB");
        assert_eq!(size(u64::MAX), "18.4 EB");
    }

    #[test]
    fn counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(100_000), "100,000");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn dates() {
        assert_eq!(date(None), "");
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // The test process never loads the zone, so this is UTC.
        assert_eq!(date(Some(time)), "2023-11-14 22:13");
    }

    #[test]
    fn permission_strings() {
        assert_eq!(permissions(0o755), "rwxr-xr-x");
        assert_eq!(permissions(0o640), "rw-r-----");
        assert_eq!(permissions(0o4755), "rwsr-xr-x");
        assert_eq!(permissions(0o1777), "rwxrwxrwt");
        assert_eq!(permissions(0o2644), "rw-r-Sr--");
    }
}

#[cfg(test)]
mod shown_tests {
    use super::shown;

    #[test]
    fn hides_what_would_disguise_a_name() {
        assert_eq!(shown("report.pdf"), "report.pdf");
        assert_eq!(shown("evil\u{202e}gnp.exe"), "evil\u{FFFD}gnp.exe");
        assert_eq!(shown("two\nlines\x1b"), "two\u{240a}lines\u{241b}");
        assert_eq!(shown("c1\u{85}"), "c1\u{FFFD}");
        assert_eq!(shown("zero\u{200b}width"), "zero\u{FFFD}width");
    }
}
