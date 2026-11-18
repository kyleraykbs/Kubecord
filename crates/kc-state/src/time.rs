//! Minimal ISO 8601 handling without a date-time dependency.

use std::time::{SystemTime, UNIX_EPOCH};

use kc_core::Snowflake;

const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Unix milliseconds of a UTC ISO 8601 timestamp such as
/// `2026-11-12T08:15:30.123000+00:00`. Discord always sends UTC, so the
/// offset suffix is not interpreted.
pub fn iso_to_unix_millis(timestamp: &str) -> Option<u64> {
    let bytes = timestamp.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let number = |range: std::ops::Range<usize>| timestamp.get(range)?.parse::<i64>().ok();
    let days = days_from_civil(number(0..4)?, number(5..7)?, number(8..10)?);
    let seconds = days * 86_400 + number(11..13)? * 3600 + number(14..16)? * 60 + number(17..19)?;
    let millis = if bytes.get(19) == Some(&b'.') {
        let digits: String = timestamp[20..]
            .chars()
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect();
        format!("{digits:0<3}").parse::<i64>().unwrap_or(0)
    } else {
        0
    };
    u64::try_from(seconds * 1000 + millis).ok()
}

/// The smallest snowflake minted at or after the given timestamp.
pub fn iso_to_snowflake(timestamp: &str) -> Option<Snowflake> {
    let millis = iso_to_unix_millis(timestamp)?;
    Some(Snowflake(millis.checked_sub(DISCORD_EPOCH_MS)? << 22))
}

pub fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SS`.
pub fn now_iso() -> String {
    millis_to_iso(unix_millis_now())
}

pub fn millis_to_iso(millis: u64) -> String {
    let seconds = (millis / 1000) as i64;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let second_of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_round_trip() {
        let millis = iso_to_unix_millis("2026-11-12T08:15:30.123000+00:00").unwrap();
        assert_eq!(millis_to_iso(millis), "2026-11-12T08:15:30");
        assert_eq!(millis % 1000, 123);
        assert_eq!(iso_to_unix_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso_to_unix_millis("2000-03-01T00:00:00Z"),
            Some(951_868_800_000)
        );
    }

    #[test]
    fn snowflake_of_a_timestamp_matches_discord_epoch() {
        let id = iso_to_snowflake("2015-01-01T00:00:01.000Z").unwrap();
        assert_eq!(id.get() >> 22, 1000);
        assert_eq!(id.timestamp(), 1_420_070_401_000);
    }
}
