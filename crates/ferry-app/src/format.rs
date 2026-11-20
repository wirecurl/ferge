//! Human-readable sizes and dates for the file list.

use std::sync::OnceLock;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The local time zone, loaded once off the UI thread by [`load_time_zone`].
/// Until then dates are shown in UTC.
static TIME_ZONE: OnceLock<TimeZone> = OnceLock::new();

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

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
    fn dates() {
        assert_eq!(date(None), "");
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // The test process never loads the zone, so this is UTC.
        assert_eq!(date(Some(time)), "2023-11-14 22:13");
    }
}
