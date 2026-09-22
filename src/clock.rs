//! The local wall clock, without starting the `date` program (#35).
//!
//! The seconds and the UTC offset come from libgit2 (`git2::Signature::now`):
//! the same clock and the same offset git writes into every commit, so a
//! date written into a document and the commit that carries it can never
//! disagree about the zone. Turning seconds into a calendar date is done
//! here, in arithmetic, so no date crate enters the build.
//!
//! Output shapes are the ones `date -Iseconds` and `date +%Y-%m-%d` gave,
//! byte for byte, because the values are permanent records:
//! `2026-09-17T02:41:05-07:00` (a valid xsd:dateTime) and `2026-09-17`.

/// Now, ISO-8601 with the machine's UTC offset (`2026-09-17T02:41:05-07:00`).
/// None when no clock can be read: never guess a date into a permanent record.
pub fn now_rfc3339() -> Option<String> {
    let (seconds, offset_minutes) = now()?;
    Some(format_rfc3339(seconds, offset_minutes))
}

/// Today's local calendar date (`2026-09-17`). None when no clock can be read.
pub fn today_ymd() -> Option<String> {
    let (seconds, offset_minutes) = now()?;
    let (y, m, d) = civil_from_days((seconds + i64::from(offset_minutes) * 60).div_euclid(86_400));
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// Seconds since the Unix epoch and the local UTC offset in minutes, as git
/// would sign a commit made this instant. The name and email are not kept.
fn now() -> Option<(i64, i32)> {
    let sig = git2::Signature::now("git-lex", "git-lex@lex.local").ok()?;
    let when = sig.when();
    Some((when.seconds(), when.offset_minutes()))
}

/// `seconds` since the epoch at UTC offset `offset_minutes`, in the shape
/// `date -Iseconds` prints: local time, then `+HH:MM` or `-HH:MM`.
fn format_rfc3339(seconds: i64, offset_minutes: i32) -> String {
    let local = seconds + i64::from(offset_minutes) * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let off = offset_minutes.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{sign}{:02}:{:02}",
        off / 60,
        off % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days`, valid for every day the i64 holds.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of era [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, March-based [0, 365]
    let mp = (5 * doy + 2) / 153; // March-based month [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970_at_utc() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00+00:00");
    }

    /// The shape `date -Iseconds` printed on the machine that wrote the
    /// records already in souls: `2026-09-17T02:41:05-07:00`.
    #[test]
    fn pacific_daylight_time_matches_the_date_program() {
        assert_eq!(format_rfc3339(1_789_638_065, -420), "2026-09-17T02:41:05-07:00");
    }

    #[test]
    fn positive_half_hour_offset_and_a_leap_day() {
        assert_eq!(format_rfc3339(951_848_999, 330), "2000-02-29T23:59:59+05:30");
    }

    #[test]
    fn a_negative_offset_can_cross_midnight_backwards() {
        // 1970-01-01T00:30:00Z is still New Year's Eve one hour to the west.
        assert_eq!(format_rfc3339(1_800, -60), "1969-12-31T23:30:00-01:00");
        assert_eq!(format_rfc3339(-3_600, 0), "1969-12-31T23:00:00+00:00");
    }

    #[test]
    fn civil_dates_round_the_gregorian_corners() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));
    }

    /// The live clock agrees with the platform `date` program on the offset
    /// and, to the minute, on the time — the two shapes must never drift.
    #[test]
    fn live_clock_agrees_with_the_date_program() {
        let ours = now_rfc3339().expect("a clock");
        assert_eq!(ours.len(), 25, "{ours}");
        assert_eq!(&ours[10..11], "T");
        let theirs = std::process::Command::new("date").args(["-Iseconds"]).output();
        let Ok(out) = theirs else { return }; // no date program: nothing to compare against
        let theirs = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if theirs.len() != 25 { return }
        assert_eq!(&ours[19..], &theirs[19..], "offset: ours {ours} theirs {theirs}");
        assert_eq!(&ours[..16], &theirs[..16], "to the minute: ours {ours} theirs {theirs}");
        assert_eq!(today_ymd().unwrap(), &theirs[..10]);
    }
}
