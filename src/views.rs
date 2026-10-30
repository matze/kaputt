//! Everything the pages show: aggregation, German formatting and the view
//! structs the templates render.
//!
//! Templates stay dumb on purpose — every number, label and coordinate is
//! computed here, where it can be tested without a browser.

use std::sync::LazyLock;

use askama::Template;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

use crate::db::{CheckSummary, HistoryPoint, IncidentSummary, TargetState};

/// Bars on the overview: one per day, including today.
pub const BAR_DAYS: i64 = 90;
/// Window of the latency chart on the detail page.
const CHART_SECONDS: i64 = 24 * 3600;
/// A pause in latency samples longer than this starts a new line segment, so
/// an outage is a gap instead of a misleading straight line.
const CHART_GAP_SECONDS: i64 = 1800;

const CHART_WIDTH: i64 = 720;
const CHART_HEIGHT: i64 = 150;
const PAD_LEFT: i64 = 40;
const PAD_RIGHT: i64 = 8;
const PAD_TOP: i64 = 10;
const PAD_BOTTOM: i64 = 20;

/// Everything lives in Berlin time: the day bars and every timestamp follow
/// the city the page is about, DST included.
static TZ: LazyLock<TimeZone> =
    LazyLock::new(|| TimeZone::get("Europe/Berlin").unwrap_or(TimeZone::UTC));

fn tz() -> &'static TimeZone {
    &TZ
}

fn at(ts: i64) -> jiff::civil::DateTime {
    tz().to_datetime(Timestamp::from_second(ts).unwrap_or(Timestamp::UNIX_EPOCH))
}

fn midnight(date: Date) -> i64 {
    tz().to_zoned(date.at(0, 0, 0, 0))
        .map(|zoned| zoned.timestamp().as_second())
        .unwrap_or(0)
}

fn date_label(date: Date) -> String {
    format!("{:02}.{:02}.{}", date.day(), date.month(), date.year())
}

fn clock(ts: i64) -> String {
    let dt = at(ts);
    format!("{:02}:{:02}", dt.hour(), dt.minute())
}

/// "30.10.2026, 13:56"
fn stamp(ts: i64) -> String {
    format!("{}, {}", date_label(at(ts).date()), clock(ts))
}

/// "30.10. 13:56"
fn short_stamp(ts: i64) -> String {
    let dt = at(ts);
    format!("{:02}.{:02}. {}", dt.day(), dt.month(), clock(ts))
}

/// Same as [`rel`] but abbreviated for the dense overview rows.
pub fn rel_short(now: i64, ts: i64) -> String {
    let diff = now - ts;
    if diff < 2 {
        return "gerade eben".to_string();
    }
    if diff < 60 {
        return format!("vor {diff} s");
    }
    if diff < 3600 {
        return format!("vor {} Min", diff / 60);
    }
    if diff < 86400 {
        return format!("vor {} Std", diff / 3600);
    }
    if diff < 7 * 86400 {
        return format!("vor {} Tagen", diff / 86400);
    }
    date_label(at(ts).date())
}

/// Duration rounded down to one unit, for rows that need no precision.
pub fn span_short(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return format!("{seconds} s");
    }
    if seconds < 3600 {
        return format!("{} Min", seconds / 60);
    }
    if seconds < 86400 {
        return format!("{} Std", seconds / 3600);
    }
    format!("{} Tage", seconds / 86400)
}

fn count(n: i64, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// Compact duration: "45 s", "3 Min 12 s", "2 Std 4 Min", "1 Tag 3 Std".
pub fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return format!("{seconds} s");
    }
    if seconds < 3600 {
        let (minutes, rest) = (seconds / 60, seconds % 60);
        return if rest == 0 {
            format!("{minutes} Min")
        } else {
            format!("{minutes} Min {rest} s")
        };
    }
    if seconds < 86400 {
        let (hours, minutes) = (seconds / 3600, seconds % 3600 / 60);
        return if minutes == 0 {
            format!("{hours} Std")
        } else {
            format!("{hours} Std {minutes} Min")
        };
    }
    let (days, hours) = (seconds / 86400, seconds % 86400 / 3600);
    if hours == 0 {
        count(days, "Tag", "Tage")
    } else {
        format!("{} {hours} Std", count(days, "Tag", "Tage"))
    }
}

/// Thousands separated with dots, as German numbers are written.
pub fn num_de(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push('.');
        }
        out.push(digit);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// "99,8 %", "100 %", or an em dash when nothing was measured.
pub fn percent(ok: u32, total: u32) -> String {
    if total == 0 {
        return "—".to_string();
    }
    let tenths = (u64::from(ok) * 1000 + u64::from(total) / 2) / u64::from(total);
    if tenths % 10 == 0 {
        format!("{} %", tenths / 10)
    } else {
        format!("{},{:01} %", tenths / 10, tenths % 10)
    }
}

/// German label for the `error_kind` vocabulary stored in the database.
pub fn cause_de(kind: &str) -> &'static str {
    match kind {
        "dns" => "DNS-Fehler",
        "connect" => "Keine Verbindung",
        "tls" => "Zertifikat oder TLS",
        "timeout" => "Zeitüberschreitung",
        "status" => "Falscher Status",
        "too_many_redirects" => "Zu viele Umleitungen",
        _ => "Fehler",
    }
}

/// The stored detail is the whole reqwest source chain, generic layers
/// included ("error sending request for url …: client error (Connect): …").
/// Only the last step says anything, so that is what the page shows.
fn detail_tail(detail: &str) -> &str {
    detail.rsplit(": ").next().unwrap_or(detail).trim()
}

/// The part of the detail that adds something beyond the kind label.
fn detail_extra(kind: &str, detail: Option<&str>) -> Option<String> {
    let tail = detail.map(detail_tail).unwrap_or("");
    if tail.is_empty() || tail.to_ascii_lowercase().contains("timed out") {
        // "operation timed out" only repeats what the label already says.
        return None;
    }
    if tail.eq_ignore_ascii_case(cause_de(kind)) {
        return None;
    }
    Some(tail.to_string())
}

/// "Zeitüberschreitung", "Falscher Status: HTTP 503, erwartet 200".
fn error_note(kind: &str, detail: Option<&str>) -> String {
    match detail_extra(kind, detail) {
        Some(tail) => format!("{}: {tail}", cause_de(kind)),
        None => cause_de(kind).to_string(),
    }
}

fn latency(ms: Option<i64>) -> String {
    match ms {
        Some(ms) => format!("{} ms", num_de(ms)),
        None => "—".to_string(),
    }
}

/// The rolling day grid the bars are drawn on, in Berlin local days.
pub struct DayGrid {
    /// Start of every day in the window, plus the start of tomorrow: 91
    /// boundaries for 90 bars.
    pub starts: Vec<i64>,
    pub labels: Vec<String>,
}

pub fn day_grid(now: i64) -> DayGrid {
    let today = at(now).date();
    let mut starts = Vec::with_capacity(BAR_DAYS as usize + 1);
    // The window ends today, so the last boundary is tomorrow's midnight.
    for offset in 0..=BAR_DAYS {
        let date = today
            .checked_add(Span::new().days(offset - (BAR_DAYS - 1)))
            .unwrap_or(today);
        starts.push(midnight(date));
    }
    let labels = starts[..BAR_DAYS as usize]
        .iter()
        .map(|&start| date_label(at(start).date()))
        .collect();
    DayGrid { starts, labels }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayStat {
    pub checks: u32,
    pub ok: u32,
}

/// Sort the checks into the day buckets of `grid`.
pub fn day_stats(history: &[HistoryPoint], grid: &DayGrid) -> Vec<DayStat> {
    let mut stats = vec![DayStat { checks: 0, ok: 0 }; BAR_DAYS as usize];
    for &(ts, ok) in history {
        let index = grid.starts.partition_point(|&start| start <= ts);
        if let Some(bucket) = index.checked_sub(1).and_then(|i| stats.get_mut(i)) {
            bucket.checks += 1;
            if ok {
                bucket.ok += 1;
            }
        }
    }
    stats
}

/// Checks and successes from `since` on.
fn uptime(history: &[HistoryPoint], since: i64) -> (u32, u32) {
    let start = history.partition_point(|&(ts, _)| ts < since);
    let window = &history[start..];
    let ok = window.iter().filter(|(_, ok)| *ok).count();
    (ok as u32, window.len() as u32)
}

#[derive(Clone)]
pub struct Window {
    pub label: &'static str,
    pub value: String,
}

#[derive(Clone)]
pub struct Bar {
    pub tone: &'static str,
    pub title: String,
}

/// One target as the overview lists it.
#[derive(Clone)]
pub struct TargetCard {
    pub name: String,
    pub href: String,
    pub tone: &'static str,
    /// Uptime of the 90-day window shown by the bars.
    pub uptime: String,
    /// Latest check, in one short line (detail page).
    pub meta: String,
    /// Failure detail, when there is one (detail page).
    pub note: Option<String>,
    /// Everything the row knows, for the hover popup.
    pub hover: String,
    pub bars: Vec<Bar>,
    pub bars_note: String,
}

fn check_error(check: &CheckSummary) -> String {
    error_note(
        check.error_kind.as_deref().unwrap_or("other"),
        check.error_detail.as_deref(),
    )
}

/// One row of the overview, and the head of the detail page.
pub fn card(state: &TargetState, history: &[HistoryPoint], grid: &DayGrid, now: i64) -> TargetCard {
    let (tone, meta, note) = match (&state.open_incident, &state.last_check) {
        (Some(incident), _) => {
            let mut meta = format!(
                "nicht erreichbar seit {}",
                span_short(now - incident.started_at)
            );
            if let Some(ok_ts) = state.last_ok_ts {
                meta.push_str(&format!(" · zuletzt erreichbar {}", rel_short(now, ok_ts)));
            }
            (
                "down",
                meta,
                Some(error_note(&incident.cause, incident.detail.as_deref())),
            )
        }
        (None, Some(check)) if !check.ok => (
            "warn",
            format!("{} nicht geantwortet", rel_short(now, check.ts)),
            Some(check_error(check)),
        ),
        (None, Some(check)) => (
            "up",
            format!(
                "{} · {} · {}",
                rel_short(now, check.ts),
                latency(check.latency_ms),
                check
                    .status_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "—".to_string())
            ),
            None,
        ),
        (None, None) => ("unknown", "noch kein Check".to_string(), None),
    };

    let stats = day_stats(history, grid);
    let (uptime_ok, uptime_total) = uptime(history, grid.starts[0]);

    let measured = stats.iter().filter(|stat| stat.checks > 0).count();
    let flawed = stats.iter().filter(|stat| stat.ok < stat.checks).count();
    let mut bars = Vec::with_capacity(stats.len());
    for (stat, day) in stats.iter().zip(&grid.labels) {
        let (tone, title) = if stat.checks == 0 {
            ("gap", format!("{day}: keine Checks"))
        } else {
            let tone = if stat.ok == stat.checks {
                "ok"
            } else if u64::from(stat.ok) * 100 >= u64::from(stat.checks) * 99 {
                "warn"
            } else {
                "bad"
            };
            (
                tone,
                format!(
                    "{day}: {} ({} Checks)",
                    percent(stat.ok, stat.checks),
                    num_de(i64::from(stat.checks))
                ),
            )
        };
        bars.push(Bar { tone, title });
    }

    let bars_note = if measured == 0 {
        "Keine Tagesdaten.".to_string()
    } else if flawed == 0 {
        format!("{measured} Tage ohne Fehlschlag.")
    } else {
        format!("{flawed} von {measured} Tagen mit Fehlschlägen.")
    };

    let hover = match &note {
        Some(note) => format!("{meta} · {note}"),
        None => meta.clone(),
    };

    TargetCard {
        name: state.name.clone(),
        href: format!("/t/{}", state.id),
        tone,
        uptime: percent(uptime_ok, uptime_total),
        meta,
        note,
        hover,
        bars,
        bars_note,
    }
}

/// Uptime of the three windows the detail page lists.
pub fn windows(history: &[HistoryPoint], grid: &DayGrid, now: i64) -> Vec<Window> {
    [
        ("24 h", now - 86400),
        ("7 Tage", now - 7 * 86400),
        ("90 Tage", grid.starts[0]),
    ]
    .into_iter()
    .map(|(label, since)| {
        let (ok, total) = uptime(history, since);
        Window {
            label,
            value: percent(ok, total),
        }
    })
    .collect()
}

pub struct Dot {
    pub x: i64,
    pub y: i64,
}

/// Server-rendered latency chart: plain SVG, no JavaScript.
pub struct Chart {
    pub empty: bool,
    pub width: i64,
    pub height: i64,
    pub segments: Vec<String>,
    pub dots: Vec<Dot>,
    pub alt: String,
    pub caption: String,
    pub max_label: String,
    pub zero_label: String,
    pub start_label: String,
    pub end_label: String,
    pub y_top: i64,
    pub y_base: i64,
    pub x_left: i64,
    pub x_right: i64,
    pub label_top: i64,
    pub label_base: i64,
    pub label_time: i64,
    pub end_anchor_x: i64,
}

/// Round the axis up to a readable value: 1, 2 or 5 times a power of ten.
fn nice_max(value: i64) -> i64 {
    if value <= 0 {
        return 100;
    }
    let magnitude = 10i64.pow((value as f64).log10().floor() as u32);
    for factor in [1, 2, 5, 10] {
        let candidate = magnitude * factor;
        if candidate >= value {
            return candidate;
        }
    }
    magnitude * 10
}

pub fn chart(samples: &[(i64, i64)], now: i64) -> Chart {
    let start = now - CHART_SECONDS;
    let samples: Vec<(i64, i64)> = samples
        .iter()
        .copied()
        .filter(|&(ts, _)| ts >= start && ts <= now)
        .collect();

    let base = CHART_HEIGHT - PAD_BOTTOM;
    let top = PAD_TOP;
    let left = PAD_LEFT;
    let right = CHART_WIDTH - PAD_RIGHT;

    if samples.is_empty() {
        return Chart {
            empty: true,
            width: CHART_WIDTH,
            height: CHART_HEIGHT,
            segments: Vec::new(),
            dots: Vec::new(),
            alt: "Keine Messwerte".to_string(),
            caption: "Keine erfolgreichen Checks in den letzten 24 Stunden.".to_string(),
            max_label: String::new(),
            zero_label: String::new(),
            start_label: String::new(),
            end_label: String::new(),
            y_top: top,
            y_base: base,
            x_left: left,
            x_right: right,
            label_top: top + 4,
            label_base: base + 4,
            label_time: CHART_HEIGHT - 4,
            end_anchor_x: right,
        };
    }

    let highest = samples.iter().map(|&(_, ms)| ms).max().unwrap_or(0);
    let scale = nice_max(highest);
    let span = (now - start).max(1);
    let x_of = |ts: i64| left + (ts - start) * (right - left) / span;
    let y_of = |ms: i64| base - ms * (base - top) / scale;

    let mut segments = Vec::new();
    let mut dots = Vec::new();
    let mut current: Vec<(i64, i64)> = Vec::new();
    let mut previous: Option<i64> = None;
    for &(ts, ms) in &samples {
        let jump = previous.is_some_and(|last| ts - last > CHART_GAP_SECONDS);
        if jump && !current.is_empty() {
            segments.push(current);
            current = Vec::new();
        }
        current.push((x_of(ts), y_of(ms)));
        previous = Some(ts);
    }
    if !current.is_empty() {
        segments.push(current);
    }

    let segments = segments
        .into_iter()
        .map(|points| {
            if points.len() == 1 {
                let (x, y) = points[0];
                dots.push(Dot { x, y });
            }
            points
                .iter()
                .map(|(x, y)| format!("{x},{y}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();

    let mut sorted: Vec<i64> = samples.iter().map(|&(_, ms)| ms).collect();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let lowest = sorted[0];

    Chart {
        empty: false,
        width: CHART_WIDTH,
        height: CHART_HEIGHT,
        segments,
        dots,
        alt: format!(
            "Antwortzeiten der letzten 24 Stunden zwischen {} und {}",
            latency(Some(lowest)),
            latency(Some(highest))
        ),
        caption: format!(
            "{} Checks · Median {} · max {}",
            num_de(samples.len() as i64),
            latency(Some(median)),
            latency(Some(highest))
        ),
        max_label: latency(Some(scale)),
        zero_label: "0".to_string(),
        start_label: short_stamp(start),
        end_label: short_stamp(now),
        y_top: top,
        y_base: base,
        x_left: left,
        x_right: right,
        label_top: top + 4,
        label_base: base + 4,
        label_time: CHART_HEIGHT - 4,
        end_anchor_x: right,
    }
}

pub struct IncidentView {
    pub when: String,
    pub span: String,
    pub cause: String,
    pub detail: Option<String>,
}

pub fn incident_view(incident: &IncidentSummary, now: i64) -> IncidentView {
    let until = incident.ended_at.unwrap_or(now);
    IncidentView {
        when: stamp(incident.started_at),
        span: match incident.ended_at {
            Some(_) => span_short(until - incident.started_at),
            None => format!("seit {}", span_short(now - incident.started_at)),
        },
        cause: cause_de(&incident.cause).to_string(),
        detail: detail_extra(&incident.cause, incident.detail.as_deref()),
    }
}

pub struct TargetLink {
    pub name: String,
    pub url: String,
    pub interval: String,
}

#[derive(Template)]
#[template(path = "overview.html")]
pub struct OverviewPage {
    pub targets: Vec<TargetCard>,
}

#[derive(Template)]
#[template(path = "status.html")]
pub struct StatusPartial {
    pub targets: Vec<TargetCard>,
}

#[derive(Template)]
#[template(path = "detail.html")]
pub struct DetailPage {
    pub target: TargetCard,
    pub url: String,
    pub interval_s: u32,
    pub expect_status: u16,
    pub windows: Vec<Window>,
    pub chart: Chart,
    pub incidents: Vec<IncidentView>,
}

#[derive(Template)]
#[template(path = "about.html")]
pub struct AboutPage {
    pub targets: Vec<TargetLink>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin(y: i16, m: i8, d: i8, hour: i8) -> i64 {
        tz().to_zoned(Date::new(y, m, d).unwrap().at(hour, 0, 0, 0))
            .expect("Berlin time")
            .timestamp()
            .as_second()
    }

    #[test]
    fn percent_rounds_and_keeps_german_decimals() {
        assert_eq!(percent(0, 0), "—");
        assert_eq!(percent(288, 288), "100 %");
        assert_eq!(percent(1, 288), "0,3 %");
        assert_eq!(percent(287, 288), "99,7 %");
        assert_eq!(percent(1, 2), "50 %");
    }

    #[test]
    fn numbers_use_dots_as_thousands_separator() {
        assert_eq!(num_de(0), "0");
        assert_eq!(num_de(999), "999");
        assert_eq!(num_de(1000), "1.000");
        assert_eq!(num_de(25_920), "25.920");
        assert_eq!(num_de(-4321), "-4.321");
    }

    #[test]
    fn short_forms_stay_within_one_unit() {
        let now = 1_000_000;
        assert_eq!(rel_short(now, now - 12), "vor 12 s");
        assert_eq!(rel_short(now, now - 300), "vor 5 Min");
        assert_eq!(rel_short(now, now - 7200), "vor 2 Std");
        assert_eq!(rel_short(now, now - 3 * 86400), "vor 3 Tagen");
        assert_eq!(span_short(42), "42 s");
        assert_eq!(span_short(2521), "42 Min");
        assert_eq!(span_short(7300), "2 Std");
        assert_eq!(span_short(3 * 86400), "3 Tage");
    }

    #[test]
    fn duration_drops_zero_parts() {
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(180), "3 Min");
        assert_eq!(duration(192), "3 Min 12 s");
        assert_eq!(duration(7200), "2 Std");
        assert_eq!(duration(86400), "1 Tag");
        assert_eq!(duration(86400 + 3600), "1 Tag 1 Std");
    }

    #[test]
    fn error_text_keeps_only_the_specific_tail() {
        assert_eq!(
            error_note(
                "connect",
                Some(
                    "error sending request for url (http://127.0.0.1:8124/): \
                     client error (Connect): tcp connect error: \
                     Connection refused (os error 111)"
                )
            ),
            "Keine Verbindung: Connection refused (os error 111)"
        );
        assert_eq!(
            error_note(
                "timeout",
                Some("error sending request for url (http://x/): operation timed out")
            ),
            "Zeitüberschreitung"
        );
        assert_eq!(
            error_note("status", Some("HTTP 503, erwartet 200")),
            "Falscher Status: HTTP 503, erwartet 200"
        );
        assert_eq!(error_note("connect", None), "Keine Verbindung");
    }

    #[test]
    fn day_grid_spans_ninety_days_and_follows_berlin_dst() {
        // Spring forward: 2026-03-29 is the last day of the window.
        let grid = day_grid(berlin(2026, 3, 29, 12));
        assert_eq!(grid.starts.len(), BAR_DAYS as usize + 1);
        assert_eq!(grid.labels.len(), BAR_DAYS as usize);
        assert_eq!(grid.labels.last().map(String::as_str), Some("29.03.2026"));
        let index = grid.labels.len() - 1;
        assert_eq!(grid.starts[index + 1] - grid.starts[index], 23 * 3600);

        // Fall back: 2026-10-25 has 25 hours.
        let grid = day_grid(berlin(2026, 10, 30, 12));
        assert_eq!(grid.labels.last().map(String::as_str), Some("30.10.2026"));
        let index = grid
            .labels
            .iter()
            .position(|label| label == "25.10.2026")
            .expect("autumn day in window");
        assert_eq!(grid.starts[index + 1] - grid.starts[index], 25 * 3600);
        // Every other day in that window is a plain 24 hours.
        for (position, labels) in grid.labels.iter().enumerate() {
            if labels == "25.10.2026" {
                continue;
            }
            assert_eq!(
                grid.starts[position + 1] - grid.starts[position],
                24 * 3600,
                "{labels}"
            );
        }
    }

    #[test]
    fn day_stats_use_day_boundaries_not_fixed_offsets() {
        let now = berlin(2026, 10, 30, 12);
        let grid = day_grid(now);
        let last = grid.labels.len() - 1;
        // 23:30 yesterday, then 00:30 today: two buckets.
        let late = grid.starts[last] - 1800;
        let early = grid.starts[last] + 1800;
        let history = vec![(late, true), (early, false)];
        let stats = day_stats(&history, &grid);
        assert_eq!(stats[last - 1], DayStat { checks: 1, ok: 1 });
        assert_eq!(stats[last], DayStat { checks: 1, ok: 0 });
        // Checks older than the window and inside tomorrow fall outside.
        let stats = day_stats(
            &[(grid.starts[0] - 1, false), (grid.starts[0] + 1, true)],
            &grid,
        );
        assert_eq!(stats[0], DayStat { checks: 1, ok: 1 });
        assert_eq!(stats.iter().map(|stat| stat.checks).sum::<u32>(), 1);
    }

    #[test]
    fn chart_maps_values_into_the_plot_area() {
        let now = 1_000_000;
        // Two samples far apart: one point per segment, both drawn as dots.
        let samples = vec![(now - CHART_SECONDS, 100), (now, 200)];
        let chart = chart(&samples, now);
        assert!(!chart.empty);
        assert_eq!(chart.segments.len(), 2);
        assert_eq!(chart.dots.len(), 2);
        assert_eq!(chart.max_label, "200 ms");
        let scale = 200;
        let span = chart.y_base - chart.y_top;
        assert_eq!(chart.dots[0].x, chart.x_left);
        assert_eq!(chart.dots[0].y, chart.y_base - 100 * span / scale);
        assert_eq!(chart.dots[1].x, chart.x_right);
        assert_eq!(chart.dots[1].y, chart.y_base - 200 * span / scale);
    }

    #[test]
    fn chart_keeps_dense_samples_in_one_segment() {
        let now = 1_000_000;
        let samples: Vec<(i64, i64)> = (0..10)
            .map(|index| (now - 3600 + index * 300, 120 + index))
            .collect();
        let chart = chart(&samples, now);
        assert_eq!(chart.segments.len(), 1);
        assert_eq!(chart.segments[0].split(' ').count(), 10);
        assert!(chart.dots.is_empty());
    }

    #[test]
    fn chart_breaks_lines_across_a_gap() {
        let now = 1_000_000;
        let samples = vec![
            (now - 7200, 120),
            (now - 7000, 130),
            (now - 60, 140),
            (now, 150),
        ];
        let chart = chart(&samples, now);
        assert_eq!(chart.segments.len(), 2);
    }

    #[test]
    fn chart_marks_a_single_sample_with_a_dot() {
        let now = 1_000_000;
        let chart = chart(&[(now - 10, 120)], now);
        assert_eq!(chart.segments.len(), 1);
        assert_eq!(chart.dots.len(), 1);
    }

    #[test]
    fn chart_is_empty_without_samples() {
        let chart = chart(&[], 1_000_000);
        assert!(chart.empty);
        assert!(chart.segments.is_empty());
    }
}
