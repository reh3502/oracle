//! Offline parsing of explicit run times and bounded estimated durations.
use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Timelike};
use chrono_tz::Tz;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedStart {
    pub starts_at: i64,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("Enter YYYY-MM-DD HH:MM, an RFC3339 date with an offset, or a Discord timestamp.")]
    InvalidStart,
    #[error("Choose a time zone such as America/New_York for this local date and time.")]
    MissingTimezone,
    #[error("Use an IANA time zone such as America/New_York, Europe/London, or UTC.")]
    InvalidTimezone,
    #[error(
        "This local time occurs twice when clocks change. Enter an RFC3339 date with an explicit offset to choose one."
    )]
    AmbiguousLocalTime,
    #[error(
        "This local time does not exist because clocks move forward. Choose a time before or after the clock change."
    )]
    NonexistentLocalTime,
    #[error("Choose a date between 1970 and 9999.")]
    OutOfRange,
    #[error("Enter an estimated duration from 1 to 1440 minutes, such as 90, 90m, or 1h 30m.")]
    InvalidDuration,
}

/// Parse a complete instant. Local input never uses the machine's time zone.
/// Offset and Discord input are self-contained and return no input time zone.
pub fn parse_start(input: &str, timezone: Option<&str>) -> Result<ParsedStart, ScheduleError> {
    let input = input.trim();
    if input.len() > 100 {
        return Err(ScheduleError::InvalidStart);
    }
    let (seconds, zone) = if input.starts_with("<t:") {
        let value = input
            .strip_prefix("<t:")
            .and_then(|s| s.strip_suffix('>'))
            .ok_or(ScheduleError::InvalidStart)?;
        let mut parts = value.split(':');
        let seconds = parts.next().ok_or(ScheduleError::InvalidStart)?;
        if seconds.is_empty() || !seconds.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ScheduleError::InvalidStart);
        }
        if let Some(style) = parts.next()
            && (!matches!(style, "t" | "T" | "d" | "D" | "f" | "F" | "R") || parts.next().is_some())
        {
            return Err(ScheduleError::InvalidStart);
        }
        (
            seconds
                .parse::<i64>()
                .map_err(|_| ScheduleError::OutOfRange)?,
            None,
        )
    } else if let Ok(date) = DateTime::parse_from_rfc3339(input) {
        // Discord represents whole Unix seconds; do not silently discard precision
        // or map a leap second onto a different instant.
        if date.nanosecond() != 0 {
            return Err(ScheduleError::InvalidStart);
        }
        (date.timestamp(), None)
    } else {
        let date = NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M")
            .map_err(|_| ScheduleError::InvalidStart)?;
        if date.format("%Y-%m-%d %H:%M").to_string() != input {
            return Err(ScheduleError::InvalidStart);
        }
        let zone = timezone
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(ScheduleError::MissingTimezone)?;
        let zone: Tz = zone.parse().map_err(|_| ScheduleError::InvalidTimezone)?;
        let date = match zone.from_local_datetime(&date) {
            LocalResult::Single(date) => date,
            LocalResult::Ambiguous(_, _) => return Err(ScheduleError::AmbiguousLocalTime),
            LocalResult::None => return Err(ScheduleError::NonexistentLocalTime),
        };
        (date.timestamp(), Some(zone.name().to_owned()))
    };
    if !(0..=253_402_300_799).contains(&seconds) {
        return Err(ScheduleError::OutOfRange);
    }
    Ok(ParsedStart {
        starts_at: seconds,
        timezone: zone,
    })
}

/// Integer minutes, or an hours/minutes expression with each unit at most once.
pub fn parse_duration(input: &str) -> Result<u16, ScheduleError> {
    let input = input.trim();
    if input.is_empty() || input.len() > 32 {
        return Err(ScheduleError::InvalidDuration);
    }
    let minutes = if input.bytes().all(|b| b.is_ascii_digit()) {
        input
            .parse::<u32>()
            .map_err(|_| ScheduleError::InvalidDuration)?
    } else {
        let mut rest = input;
        let mut total = 0_u32;
        let mut saw_hours = false;
        let mut saw_minutes = false;
        while !rest.is_empty() {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return Err(ScheduleError::InvalidDuration);
            }
            let count = rest[..digits]
                .parse::<u32>()
                .map_err(|_| ScheduleError::InvalidDuration)?;
            rest = rest[digits..].trim_start();
            let multiplier = match rest.as_bytes().first() {
                Some(b'h' | b'H') if !saw_hours && !saw_minutes => {
                    saw_hours = true;
                    60
                }
                Some(b'm' | b'M') if !saw_minutes => {
                    saw_minutes = true;
                    1
                }
                _ => return Err(ScheduleError::InvalidDuration),
            };
            total = count
                .checked_mul(multiplier)
                .and_then(|v| total.checked_add(v))
                .ok_or(ScheduleError::InvalidDuration)?;
            rest = rest[1..].trim_start();
        }
        total
    };
    if !(1..=1440).contains(&minutes) {
        return Err(ScheduleError::InvalidDuration);
    }
    Ok(minutes as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_formats_identify_the_same_instant() {
        let expected = 1_800_000_000;
        for input in [
            "2027-01-15T08:00:00Z",
            "2027-01-15T03:00:00-05:00",
            "<t:1800000000>",
            "<t:1800000000:F>",
        ] {
            assert_eq!(
                parse_start(input, None).unwrap().starts_at,
                expected,
                "{input}"
            );
        }
        let local = parse_start("2027-01-15 03:00", Some(" America/New_York ")).unwrap();
        assert_eq!(local.starts_at, expected);
        assert_eq!(local.timezone.as_deref(), Some("America/New_York"));
    }

    #[test]
    fn local_input_requires_valid_zone_and_calendar() {
        assert_eq!(
            parse_start("2027-01-15 03:00", None),
            Err(ScheduleError::MissingTimezone)
        );
        assert_eq!(
            parse_start("2027-01-15 03:00", Some("Mars/Olympus")),
            Err(ScheduleError::InvalidTimezone)
        );
        for value in [
            "2027-02-29 12:00",
            "2027-1-15 03:00",
            "2027-01-15 24:00",
            "tomorrow",
            "<t:1800000000:Z>",
            "<t:1800000000:F:R>",
            "2027-01-15T08:00:00.5Z",
        ] {
            assert_eq!(
                parse_start(value, Some("UTC")),
                Err(ScheduleError::InvalidStart),
                "{value}"
            );
        }
        assert!(parse_start("2028-02-29 12:00", Some("UTC")).is_ok());
    }

    #[test]
    fn dst_requires_an_unambiguous_real_instant() {
        assert_eq!(
            parse_start("2026-03-08 02:30", Some("America/New_York")),
            Err(ScheduleError::NonexistentLocalTime)
        );
        assert_eq!(
            parse_start("2026-11-01 01:30", Some("America/New_York")),
            Err(ScheduleError::AmbiguousLocalTime)
        );
        let early = parse_start("2026-11-01T01:30:00-04:00", None).unwrap();
        let late = parse_start("2026-11-01T01:30:00-05:00", None).unwrap();
        assert_eq!(late.starts_at - early.starts_at, 3600);
    }

    #[test]
    fn timestamp_bounds_are_checked_without_panics() {
        assert_eq!(parse_start("<t:0>", None).unwrap().starts_at, 0);
        assert_eq!(
            parse_start("<t:253402300799>", None).unwrap().starts_at,
            253_402_300_799
        );
        for value in [
            "<t:253402300800>",
            "<t:999999999999999999999999>",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_start(value, None), Err(ScheduleError::OutOfRange));
        }
    }

    #[test]
    fn duration_forms_and_limits() {
        for (value, minutes) in [
            ("90", 90),
            ("90m", 90),
            ("1h 30m", 90),
            ("1h30m", 90),
            ("1 H 30 M", 90),
            ("24h", 1440),
            ("1", 1),
        ] {
            assert_eq!(parse_duration(value), Ok(minutes));
        }
        for value in [
            "",
            "0",
            "0h",
            "1441",
            "25h",
            "1.5h",
            "-1",
            "1h 1h",
            "30m 1h",
            "90minutes",
            "999999999999999999999999999m",
            "1h junk",
            "１h",
        ] {
            assert_eq!(
                parse_duration(value),
                Err(ScheduleError::InvalidDuration),
                "{value}"
            );
        }
    }
}
