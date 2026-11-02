use super::*;

#[test]
fn keys_round_trip_and_sort_as_text() {
    let date = Date::new(2026, 10, 30).expect("date");
    assert_eq!(key(date), "2026-10-30");
    assert_eq!(parse_key("2026-10-30"), Some(date));
    assert_eq!(parse_key("2026-01-05"), Date::new(2026, 1, 5).ok());
    assert_eq!(parse_key("2026-10"), None);
    assert_eq!(parse_key("2026-10-30-1"), None);
    assert_eq!(parse_key("nope"), None);
    assert!(key(Date::new(2026, 1, 5).expect("date")) < key(date));
}

#[test]
fn a_day_is_that_days_local_span() {
    // Fall back: 2026-10-25 has 25 hours, spring forward: 2026-03-29 has 23.
    let (start, end) = bounds(Date::new(2026, 10, 25).expect("date"));
    assert_eq!(end - start, 25 * 3600);
    let (start, end) = bounds(Date::new(2026, 3, 29).expect("date"));
    assert_eq!(end - start, 23 * 3600);
    let (start, end) = bounds(Date::new(2026, 6, 15).expect("date"));
    assert_eq!(end - start, 24 * 3600);
    // The day of a timestamp is the day whose bounds contain it.
    let (start, end) = bounds(date_of(start + 60));
    assert_eq!(start, bounds(Date::new(2026, 6, 15).expect("date")).0);
    assert!(end > start);
}

#[test]
fn shifting_by_days_crosses_dst_without_losing_the_date() {
    let date = Date::new(2026, 10, 24).expect("date");
    assert_eq!(shift(date, 1), Date::new(2026, 10, 25).expect("date"));
    assert_eq!(shift(date, -1), Date::new(2026, 10, 23).expect("date"));
    assert_eq!(
        shift_years(Date::new(2026, 10, 30).expect("date"), 2),
        Date::new(2024, 10, 30).expect("date")
    );
}

#[test]
fn rfc2822_carries_the_local_offset() {
    let summer = midnight(Date::new(2026, 7, 1).expect("date"));
    let winter = midnight(Date::new(2026, 1, 1).expect("date"));
    assert!(rfc2822(summer).ends_with("+0200"));
    assert!(rfc2822(winter).ends_with("+0100"));
    assert_eq!(
        jiff::fmt::rfc2822::parse(&rfc2822(summer))
            .expect("round trip")
            .timestamp()
            .as_second(),
        summer
    );
}
