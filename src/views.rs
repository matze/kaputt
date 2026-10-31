//! Everything the pages show: aggregation, German formatting and the view
//! structs the templates render.
//!
//! Templates stay dumb on purpose — every number, label and coordinate is
//! computed here, where it can be tested without a browser.

use std::collections::HashMap;

use askama::Template;
use jiff::civil::Date;

use crate::day::{self, at, date_of};
use crate::db::{CheckSummary, DailyStat, HistoryPoint, IncidentSummary, TargetState};

/// Bars on the overview: one per day, including today.
pub const BAR_DAYS: i64 = 90;
/// Horizontal buckets of the latency chart. Raw checks arrive every few
/// minutes, so one bucket per sample would draw a dense, jagged line in the
/// short window and tens of thousands of points in the long one. Averaging
/// into 240 buckets keeps every window to one point per few pixels.
const CHART_BUCKETS: i64 = 240;

const CHART_WIDTH: i64 = 720;
const CHART_HEIGHT: i64 = 150;
const PAD_LEFT: i64 = 40;
const PAD_RIGHT: i64 = 8;
const PAD_TOP: i64 = 10;
const PAD_BOTTOM: i64 = 20;

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
    count(seconds / 86400, "Tag", "Tage")
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

/// "99 %", "100 %", or an em dash when nothing was measured. Rounded down, so
/// 100 % only stands for a window without a single failure.
pub fn percent(ok: u32, total: u32) -> String {
    if total == 0 {
        return "—".to_string();
    }
    format!("{} %", u64::from(ok) * 100 / u64::from(total))
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
    /// Database key of every bar's day (`2026-10-30`), oldest first. The
    /// window ends today, so the last key is today's.
    pub days: Vec<String>,
    /// The same days for the hover text: `30.10.2026`.
    pub labels: Vec<String>,
}

pub fn day_grid(now: i64) -> DayGrid {
    let today = date_of(now);
    let mut days = Vec::with_capacity(BAR_DAYS as usize);
    let mut labels = Vec::with_capacity(BAR_DAYS as usize);
    for offset in 0..BAR_DAYS {
        let date = day::shift(today, offset - (BAR_DAYS - 1));
        days.push(day::key(date));
        labels.push(date_label(date));
    }
    DayGrid { days, labels }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayStat {
    pub checks: u32,
    pub ok: u32,
    /// Time the target was down on that day, as the rollup measured it.
    pub down_seconds: i64,
}

/// Sort the daily rollups into the buckets of `grid`; a day without a rollup
/// stays an empty bar.
pub fn day_stats(rows: &[DailyStat], grid: &DayGrid) -> Vec<DayStat> {
    let by_day: HashMap<&str, &DailyStat> =
        rows.iter().map(|row| (row.day.as_str(), row)).collect();
    grid.days
        .iter()
        .map(|day| match by_day.get(day.as_str()) {
            Some(row) => DayStat {
                checks: row.checks,
                ok: row.ok_checks,
                down_seconds: row.down_seconds,
            },
            None => DayStat {
                checks: 0,
                ok: 0,
                down_seconds: 0,
            },
        })
        .collect()
}

/// Checks and successes of the checks in `history`, which is already the
/// window the caller loaded.
pub fn totals(history: &[HistoryPoint]) -> (u32, u32) {
    let ok = history.iter().filter(|(_, ok)| *ok).count();
    (ok as u32, history.len() as u32)
}

/// Checks and successes over the last `days` buckets of `stats`.
pub fn recent_totals(stats: &[DayStat], days: usize) -> (u32, u32) {
    let start = stats.len().saturating_sub(days);
    let window = &stats[start..];
    (
        window.iter().map(|stat| stat.ok).sum(),
        window.iter().map(|stat| stat.checks).sum(),
    )
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

/// One row of the overview, and the head of the detail page. The bars, the
/// day note and the 90-day uptime come from the daily rollups; everything
/// about the newest check comes from the check itself.
pub fn card(state: &TargetState, stats: &[DayStat], grid: &DayGrid, now: i64) -> TargetCard {
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
            let mut title = format!(
                "{day}: {} ({} Checks",
                percent(stat.ok, stat.checks),
                num_de(i64::from(stat.checks))
            );
            if stat.down_seconds > 0 {
                title.push_str(&format!(", {} kaputt", span_short(stat.down_seconds)));
            }
            title.push(')');
            (tone, title)
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

    let (uptime_ok, uptime_total) = recent_totals(stats, BAR_DAYS as usize);
    TargetCard {
        name: state.name.clone(),
        href: format!("/t/{}", state.id),
        tone,
        uptime: percent(uptime_ok, uptime_total),
        note,
        hover,
        bars,
        bars_note,
    }
}

/// Uptime of the three windows the detail page lists. The day windows come
/// from the rollups — day buckets, like the bars; the rolling 24 h comes from
/// the raw checks, because no day bucket can express it.
pub fn windows(stats: &[DayStat], last_24h: (u32, u32)) -> Vec<Window> {
    let (week_ok, week_total) = recent_totals(stats, 7);
    let (quarter_ok, quarter_total) = recent_totals(stats, BAR_DAYS as usize);
    vec![
        Window {
            label: "24 h",
            value: percent(last_24h.0, last_24h.1),
        },
        Window {
            label: "7 Tage",
            value: percent(week_ok, week_total),
        },
        Window {
            label: "90 Tage",
            value: percent(quarter_ok, quarter_total),
        },
    ]
}

/// Windows the detail chart can show. The parameter strings are the public
/// `?range=` values; everything else is German UI text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChartRange {
    #[default]
    Day,
    Week,
    Quarter,
}

impl ChartRange {
    pub const ALL: [ChartRange; 3] = [Self::Day, Self::Week, Self::Quarter];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            "90d" => Some(Self::Quarter),
            _ => None,
        }
    }

    pub fn param(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Quarter => "90d",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Day => "24 h",
            Self::Week => "7 Tage",
            Self::Quarter => "90 Tage",
        }
    }

    pub fn seconds(self) -> i64 {
        match self {
            Self::Day => 24 * 3600,
            Self::Week => 7 * 86400,
            Self::Quarter => BAR_DAYS * 86400,
        }
    }

    /// Genitive for the chart alt text: "der letzten …".
    fn alt_period(self) -> &'static str {
        match self {
            Self::Day => "24 Stunden",
            Self::Week => "7 Tage",
            Self::Quarter => "90 Tage",
        }
    }

    /// Dative after "in den letzten …" for the empty caption.
    fn caption_period(self) -> &'static str {
        match self {
            Self::Day => "24 Stunden",
            Self::Week => "7 Tagen",
            Self::Quarter => "90 Tagen",
        }
    }
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

#[derive(Clone, Copy, Default)]
struct Bucket {
    ts_sum: i64,
    ms_sum: i64,
    count: i64,
}

/// Axis stamps: time of day in the short window, dates in the long ones.
fn chart_stamp(range: ChartRange, ts: i64) -> String {
    match range {
        ChartRange::Day => short_stamp(ts),
        ChartRange::Week | ChartRange::Quarter => date_label(date_of(ts)),
    }
}

pub fn chart(samples: &[(i64, i64)], now: i64, range: ChartRange) -> Chart {
    let window = range.seconds();
    let start = now - window;
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
            caption: format!(
                "Keine erfolgreichen Checks in den letzten {}.",
                range.caption_period()
            ),
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
    let span = window.max(1);
    let bucket_span = (span + CHART_BUCKETS - 1) / CHART_BUCKETS;
    let x_of = |ts: i64| left + (ts - start) * (right - left) / span;
    let y_of = |ms: i64| base - ms * (base - top) / scale;

    // Average every bucket so long windows stay smooth instead of drawing
    // tens of thousands of individual samples.
    let mut buckets = vec![Bucket::default(); CHART_BUCKETS as usize];
    for &(ts, ms) in &samples {
        let index = ((ts - start) / bucket_span).clamp(0, CHART_BUCKETS - 1) as usize;
        let bucket = &mut buckets[index];
        bucket.ts_sum += ts;
        bucket.ms_sum += ms;
        bucket.count += 1;
    }

    // An empty bucket is a stretch without a successful check: finish the
    // current line there so an outage is a gap, not a straight interpolation.
    let mut segments: Vec<Vec<(i64, i64)>> = Vec::new();
    let mut current: Vec<(i64, i64)> = Vec::new();
    for bucket in buckets {
        if bucket.count == 0 {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            continue;
        }
        let ts = bucket.ts_sum / bucket.count;
        let ms = bucket.ms_sum / bucket.count;
        current.push((x_of(ts), y_of(ms)));
    }
    if !current.is_empty() {
        segments.push(current);
    }

    let mut dots = Vec::new();
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
            "Antwortzeiten der letzten {} zwischen {} und {}",
            range.alt_period(),
            latency(Some(lowest)),
            latency(Some(highest))
        ),
        caption: format!(
            "{} Messungen, Median = {}, Max = {}",
            num_de(samples.len() as i64),
            latency(Some(median)),
            latency(Some(highest))
        ),
        max_label: latency(Some(scale)),
        zero_label: "0".to_string(),
        start_label: chart_stamp(range, start),
        end_label: chart_stamp(range, now),
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

#[derive(Clone)]
pub struct RangeLink {
    pub label: &'static str,
    pub href: String,
    pub active: bool,
}

/// The three chart-range links of one target, with the current one marked.
pub fn range_links(target_id: &str, active: ChartRange) -> Vec<RangeLink> {
    ChartRange::ALL
        .into_iter()
        .map(|range| RangeLink {
            label: range.label(),
            href: format!("/t/{target_id}?range={}#chart", range.param()),
            active: range == active,
        })
        .collect()
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
    pub windows: Vec<Window>,
    pub range_links: Vec<RangeLink>,
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
        day::tz()
            .to_zoned(Date::new(y, m, d).unwrap().at(hour, 0, 0, 0))
            .expect("Berlin time")
            .timestamp()
            .as_second()
    }

    fn stat(checks: u32, ok: u32, down_seconds: i64) -> DayStat {
        DayStat {
            checks,
            ok,
            down_seconds,
        }
    }

    #[test]
    fn percent_is_a_whole_number_rounded_down() {
        assert_eq!(percent(0, 0), "—");
        assert_eq!(percent(288, 288), "100 %");
        assert_eq!(percent(1, 288), "0 %");
        assert_eq!(percent(287, 288), "99 %");
        assert_eq!(percent(1, 2), "50 %");
        assert_eq!(percent(2, 3), "66 %");
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
    fn day_grid_is_ninety_consecutive_berlin_days_ending_today() {
        let now = berlin(2026, 10, 30, 12);
        let grid = day_grid(now);
        assert_eq!(grid.days.len(), BAR_DAYS as usize);
        assert_eq!(grid.labels.len(), BAR_DAYS as usize);
        assert_eq!(grid.days.last().map(String::as_str), Some("2026-10-30"));
        assert_eq!(grid.labels.last().map(String::as_str), Some("30.10.2026"));

        // Every bar is the local day its key names, with no day missing: the
        // window crosses the DST switch on 2026-10-25 and stays a day grid.
        let today = date_of(now);
        for (offset, (day, label)) in grid.days.iter().zip(&grid.labels).enumerate() {
            let date = day::shift(today, offset as i64 - (BAR_DAYS - 1));
            assert_eq!(day, &day::key(date), "{label}");
            assert_eq!(label, &date_label(date), "{day}");
        }
        assert!(grid.days.iter().any(|day| day == "2026-10-25"));

        // The spring switch pulls the first bar of the window back a day too.
        let grid = day_grid(berlin(2026, 3, 29, 12));
        assert_eq!(grid.days.last().map(String::as_str), Some("2026-03-29"));
        assert_eq!(grid.labels.first().map(String::as_str), Some("30.12.2025"));
    }

    #[test]
    fn bars_follow_the_rollup_rows_by_day_key() {
        let now = berlin(2026, 10, 30, 12);
        let grid = day_grid(now);
        assert_eq!(grid.days.len(), BAR_DAYS as usize);
        assert_eq!(grid.days.last().map(String::as_str), Some("2026-10-30"));
        let last = grid.days.len() - 1;
        let rows = vec![
            DailyStat {
                day: grid.days[last - 1].clone(),
                checks: 288,
                ok_checks: 288,
                down_seconds: 0,
                avg_latency_ms: Some(90),
                max_latency_ms: Some(400),
            },
            DailyStat {
                day: grid.days[last].clone(),
                checks: 100,
                ok_checks: 95,
                down_seconds: 1500,
                avg_latency_ms: Some(120),
                max_latency_ms: Some(900),
            },
            // Outside the window: must not land in any bar.
            DailyStat {
                day: "2000-01-01".to_string(),
                checks: 5,
                ok_checks: 0,
                down_seconds: 1500,
                avg_latency_ms: None,
                max_latency_ms: None,
            },
        ];
        let stats = day_stats(&rows, &grid);
        assert_eq!(stats.len(), BAR_DAYS as usize);
        assert_eq!(stats[last - 1], stat(288, 288, 0));
        assert_eq!(stats[last], stat(100, 95, 1500));
        assert_eq!(stats.iter().map(|day| day.checks).sum::<u32>(), 388);
        // Days without a rollup stay empty, so the bar renders as a gap.
        assert_eq!(stats[0], stat(0, 0, 0));
    }

    #[test]
    fn windows_take_the_day_windows_from_rollups_and_24h_from_checks() {
        let now = berlin(2026, 10, 30, 12);
        let grid = day_grid(now);
        let last = grid.days.len() - 1;
        let rows: Vec<DailyStat> = (0..3)
            .map(|offset| DailyStat {
                day: grid.days[last - offset].clone(),
                checks: 100,
                ok_checks: 90,
                down_seconds: 3000,
                avg_latency_ms: Some(100),
                max_latency_ms: Some(100),
            })
            .collect();
        let listed = windows(&day_stats(&rows, &grid), (9, 10));
        let labels: Vec<&str> = listed.iter().map(|window| window.label).collect();
        assert_eq!(labels, vec!["24 h", "7 Tage", "90 Tage"]);
        assert_eq!(listed[0].value, "90 %");
        assert_eq!(listed[1].value, "90 %");
        assert_eq!(listed[2].value, "90 %");

        let empty = windows(&day_stats(&[], &grid), (0, 0));
        assert_eq!(
            empty
                .iter()
                .map(|window| window.value.as_str())
                .collect::<Vec<_>>(),
            vec!["—", "—", "—"]
        );
    }

    #[test]
    fn card_reads_uptime_and_downtime_from_the_rollup() {
        let now = berlin(2026, 10, 30, 12);
        let grid = day_grid(now);
        let last = grid.days.len() - 1;
        let rows = vec![DailyStat {
            day: grid.days[last].clone(),
            checks: 288,
            ok_checks: 286,
            down_seconds: 600,
            avg_latency_ms: Some(100),
            max_latency_ms: Some(200),
        }];
        let state = TargetState {
            id: "opac".into(),
            name: "OPAC".into(),
            url: "https://opac.example.org/".into(),
            expect_status: 200,
            interval_s: 300,
            last_check: Some(CheckSummary {
                ts: now - 30,
                ok: true,
                status_code: Some(200),
                latency_ms: Some(120),
                error_kind: None,
                error_detail: None,
            }),
            last_ok_ts: Some(now - 30),
            open_incident: None,
        };
        let card = card(&state, &day_stats(&rows, &grid), &grid, now);
        assert_eq!(card.tone, "up");
        assert_eq!(card.uptime, "99 %");
        assert_eq!(card.bars[last].tone, "warn");
        assert_eq!(
            card.bars[last].title,
            "30.10.2026: 99 % (288 Checks, 10 Min kaputt)"
        );
        assert_eq!(card.bars_note, "1 von 1 Tagen mit Fehlschlägen.");
        assert_eq!(card.bars[0].tone, "gap");
        assert_eq!(
            card.bars[0].title,
            format!("{}: keine Checks", grid.labels[0])
        );
    }

    #[test]
    fn chart_maps_values_into_the_plot_area() {
        let now = 1_000_000;
        // Two samples far apart: one point per segment, both drawn as dots.
        let samples = vec![(now - 24 * 3600, 100), (now, 200)];
        let chart = chart(&samples, now, ChartRange::Day);
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
        let chart = chart(&samples, now, ChartRange::Day);
        assert_eq!(chart.segments.len(), 1);
        // Buckets average neighboring samples, so the line gets fewer points
        // than the raw checks but stays one continuous segment.
        assert!(chart.segments[0].split(' ').count() <= samples.len());
        assert!(chart.dots.is_empty());
    }

    #[test]
    fn chart_averages_samples_inside_a_bucket() {
        let now = 1_000_000;
        // Both samples land in the last bucket of the 24 h window.
        let chart = chart(&[(now - 100, 100), (now - 50, 200)], now, ChartRange::Day);
        assert_eq!(chart.dots.len(), 1);
        assert_eq!(chart.max_label, "200 ms");
        let span = chart.y_base - chart.y_top;
        assert_eq!(chart.dots[0].y, chart.y_base - 150 * span / 200);
    }

    #[test]
    fn chart_uses_the_selected_window() {
        let now = 1_000_000;
        let samples = vec![(now - 8 * 86400, 120), (now - 3600, 130)];

        let points = |chart: &Chart| -> usize {
            chart
                .segments
                .iter()
                .map(|segment| segment.split(' ').count())
                .sum()
        };

        // The sample eight days back is outside the week, but both samples
        // are inside the quarter.
        let week = chart(&samples, now, ChartRange::Week);
        assert_eq!(points(&week), 1);
        assert!(!week.start_label.contains(':'));

        let day = chart(&samples, now, ChartRange::Day);
        assert_eq!(points(&day), 1);
        assert!(day.start_label.contains(':'));

        let quarter = chart(&samples, now, ChartRange::Quarter);
        assert_eq!(points(&quarter), 2);
        assert!(!quarter.start_label.contains(':'));
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
        let chart = chart(&samples, now, ChartRange::Day);
        assert_eq!(chart.segments.len(), 2);
    }

    #[test]
    fn chart_marks_a_single_sample_with_a_dot() {
        let now = 1_000_000;
        let chart = chart(&[(now - 10, 120)], now, ChartRange::Day);
        assert_eq!(chart.segments.len(), 1);
        assert_eq!(chart.dots.len(), 1);
    }

    #[test]
    fn chart_is_empty_without_samples() {
        let day = chart(&[], 1_000_000, ChartRange::Day);
        assert!(day.empty);
        assert!(day.segments.is_empty());
        assert_eq!(
            day.caption,
            "Keine erfolgreichen Checks in den letzten 24 Stunden."
        );

        let week = chart(&[], 1_000_000, ChartRange::Week);
        assert_eq!(
            week.caption,
            "Keine erfolgreichen Checks in den letzten 7 Tagen."
        );
    }

    #[test]
    fn chart_ranges_round_trip_and_mark_the_active_link() {
        assert_eq!(ChartRange::default(), ChartRange::Day);
        for range in ChartRange::ALL {
            assert_eq!(ChartRange::parse(range.param()), Some(range));
        }
        assert_eq!(ChartRange::parse("1h"), None);

        let links = range_links("opac", ChartRange::Week);
        let values: Vec<&str> = links.iter().map(|link| link.href.as_str()).collect();
        assert_eq!(
            values,
            vec![
                "/t/opac?range=24h#chart",
                "/t/opac?range=7d#chart",
                "/t/opac?range=90d#chart"
            ]
        );
        assert_eq!(
            links.iter().map(|link| link.label).collect::<Vec<_>>(),
            vec!["24 h", "7 Tage", "90 Tage"]
        );
        assert_eq!(
            links.iter().map(|link| link.active).collect::<Vec<_>>(),
            vec![false, true, false]
        );
    }
}
