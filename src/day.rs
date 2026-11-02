//! Berlin local days: one place for the timezone and the day boundaries that
//! both the pages and the daily rollups need.
//!
//! Everything lives in Berlin time — the day bars and every timestamp follow
//! the city the page is about, DST included. A local day is therefore 23, 24
//! or 25 hours long, and only [`bounds`] knows the difference.

use std::sync::LazyLock;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

static TZ: LazyLock<TimeZone> =
    LazyLock::new(|| TimeZone::get("Europe/Berlin").unwrap_or(TimeZone::UTC));

pub fn tz() -> &'static TimeZone {
    &TZ
}

pub fn at(ts: i64) -> jiff::civil::DateTime {
    tz().to_datetime(Timestamp::from_second(ts).unwrap_or(Timestamp::UNIX_EPOCH))
}

/// The local day a timestamp falls into.
pub fn date_of(ts: i64) -> Date {
    at(ts).date()
}

/// Unix second that starts `date` in local time.
pub fn midnight(date: Date) -> i64 {
    tz().to_zoned(date.at(0, 0, 0, 0))
        .map(|zoned| zoned.timestamp().as_second())
        .unwrap_or(0)
}

/// `date` moved by whole local days, so DST days stay days and not 86400 s.
pub fn shift(date: Date, days: i64) -> Date {
    date.checked_add(Span::new().days(days)).unwrap_or(date)
}

/// `date` moved by whole local years.
pub fn shift_years(date: Date, years: i64) -> Date {
    date.checked_sub(Span::new().years(years)).unwrap_or(date)
}

/// Bounds of a local day in Unix seconds: `[start, end)`.
pub fn bounds(date: Date) -> (i64, i64) {
    (midnight(date), midnight(shift(date, 1)))
}

/// RFC 2822 date for feed timestamps, in Berlin time with the numeric offset
/// the day actually has (DST-aware).
pub fn rfc2822(ts: i64) -> String {
    let timestamp = Timestamp::from_second(ts).unwrap_or(Timestamp::UNIX_EPOCH);
    let zoned = timestamp.to_zoned(tz().clone());
    jiff::fmt::rfc2822::to_string(&zoned).unwrap_or_default()
}

/// Stable key of a day in the database: `2026-10-30`, which also sorts
/// chronologically as text.
pub fn key(date: Date) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// Read a key written by [`key`] back.
pub fn parse_key(key: &str) -> Option<Date> {
    let mut parts = key.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::new(year, month, day).ok()
}

#[cfg(test)]
mod tests;
