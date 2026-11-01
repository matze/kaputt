//! Everything the pages show: aggregation, German formatting and the view
//! structs the templates render.
//!
//! Templates stay dumb on purpose — every number, label and coordinate is
//! computed here, where it can be tested without a browser.

use std::collections::HashMap;

use askama::Template;
use jiff::civil::Date;

use crate::day::{self, at, date_of};
use crate::db::{
    ChartCheck, CheckSummary, DailyStat, FeedIncident, HistoryPoint, IncidentSummary, TargetState,
};

/// Bars on the overview: one per day, including today.
pub const BAR_DAYS: i64 = 90;
/// Feeds publish incidents, not checks. A blip below this never reaches a
/// subscriber; a still-running incident becomes feed-worthy at this age.
pub const FEED_MIN_INCIDENT_S: i64 = 15 * 60;
/// Incidents of one target closer than this collapse into one item, so a
/// flapping target cannot add an entry per bounce.
pub const FEED_BATCH_GAP_S: i64 = 6 * 3600;
/// Feed history, in local days.
pub const FEED_WINDOW_DAYS: i64 = 30;
/// Hard cap on items in one feed, newest first.
pub const FEED_MAX_ITEMS: usize = 50;
/// Remote error details are untrusted and can be long; feeds stay small.
const FEED_DETAIL_MAX: usize = 300;
/// Horizontal buckets of the latency chart. Raw checks arrive every few
/// minutes, so one bucket per sample would draw a dense, jagged line in the
/// short window and tens of thousands of points in the long one. Averaging
/// into 240 buckets keeps every window to one point per few pixels.
const CHART_BUCKETS: i64 = 240;

const CHART_WIDTH: i64 = 720;
/// Tall enough that a phone, where the SVG scales to the viewport width,
/// still gets a readable plot instead of a squashed strip.
const CHART_HEIGHT: i64 = 240;
const PAD_RIGHT: i64 = 8;
const PAD_TOP: i64 = 18;
const PAD_BOTTOM: i64 = 34;

/// A failed check that lasted seconds would be sub-pixel in the long windows;
/// the band still gets this width so the outage stays visible.
const MIN_BAND_WIDTH: i64 = 2;

/// Only a missing run of two probe intervals counts as a data gap: one late
/// check is jitter. The gap starts at the first missed interval.
const GAP_INTERVALS: i64 = 2;

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

/// Answer times read in whole milliseconds below a second, then in seconds
/// with one decimal: "2,3 s" instead of "2.250 ms". No thousands separator,
/// so long values stay short.
fn latency_text(ms: i64) -> String {
    if ms < 1000 {
        return format!("{} ms", ms.max(0));
    }
    let tenths = (ms + 50) / 100;
    format!("{},{} s", tenths / 10, tenths % 10)
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
    ms.map_or_else(|| "—".to_string(), latency_text)
}

/// Axis ticks drop a zero decimal, so whole seconds read "10 s" and "5 s";
/// fractional ticks keep their digit ("2,5 s").
fn axis_latency(ms: i64) -> String {
    let text = latency_text(ms);
    if text.ends_with(",0 s") {
        format!("{} s", &text[..text.len() - 4])
    } else {
        text
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
                stat.checks
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

/// A stretch of the x axis without a usable measurement, drawn as a band.
pub struct Band {
    pub x: i64,
    pub width: i64,
}

/// Server-rendered latency chart: plain SVG, no JavaScript.
pub struct Chart {
    pub empty: bool,
    pub width: i64,
    pub height: i64,
    pub segments: Vec<String>,
    pub dots: Vec<Dot>,
    pub bands: Vec<Band>,
    /// Stretches with no check at all, drawn in the neutral gap tone.
    pub gaps: Vec<Band>,
    /// Height of the plot area, for a band that spans it.
    pub plot_height: i64,
    pub alt: String,
    pub caption: String,
    pub max_label: String,
    pub mid_label: String,
    pub mid_visible: bool,
    pub zero_label: String,
    pub start_label: String,
    pub end_label: String,
    pub y_top: i64,
    pub y_mid: i64,
    pub y_base: i64,
    pub x_left: i64,
    pub x_right: i64,
    /// Vertical positions for the HTML y labels, as CSS percentages.
    pub y_top_pct: String,
    pub y_mid_pct: String,
    pub y_base_pct: String,
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

fn chart_pct(y: i64) -> String {
    format!("{:.3}%", y as f64 * 100.0 / CHART_HEIGHT as f64)
}

/// Append one band, merging it into the previous one when they touch. The x
/// limits keep a minimum-width band inside the plot.
fn push_band(bands: &mut Vec<Band>, from: i64, until: i64, x_of: &impl Fn(i64) -> i64, right: i64) {
    if until <= from {
        return;
    }
    let x = x_of(from);
    let width = (x_of(until) - x).max(MIN_BAND_WIDTH);
    let x = x.min(right - width).max(0);
    match bands.last_mut() {
        Some(last) if x <= last.x + last.width => {
            last.width = (x + width - last.x).max(last.width);
        }
        _ => bands.push(Band { x, width }),
    }
}

/// Failed checks mark the stretches without a usable measurement. A failure
/// covers one probe interval at most — the same rule the rollup uses for
/// `down_seconds` — so a gap between checks never becomes downtime. Bands that
/// touch are merged into one rectangle.
fn outage_bands(
    checks: &[ChartCheck],
    interval_s: i64,
    start: i64,
    now: i64,
    x_of: &impl Fn(i64) -> i64,
    right: i64,
) -> Vec<Band> {
    let interval = interval_s.max(1);
    let mut bands: Vec<Band> = Vec::new();
    for (index, &(ts, ok, _)) in checks.iter().enumerate() {
        if ok || ts < start || ts > now {
            continue;
        }
        let next = checks
            .get(index + 1)
            .map_or(ts + interval, |&(next, ..)| next.min(ts + interval));
        push_band(&mut bands, ts, next.max(ts + 1).min(now), &x_of, right);
    }
    bands
}

/// Stretches where the prober produced no check at all. The leading edge of
/// the window stays blank: checks before it are never loaded, so an empty
/// beginning is the start of monitoring, not a known gap.
fn data_gaps(
    checks: &[ChartCheck],
    interval_s: i64,
    start: i64,
    now: i64,
    x_of: &impl Fn(i64) -> i64,
    right: i64,
) -> Vec<Band> {
    let interval = interval_s.max(1);
    let mut gaps: Vec<Band> = Vec::new();
    let mut previous: Option<i64> = None;
    for &(ts, _, _) in checks {
        if ts < start || ts > now {
            continue;
        }
        if let Some(prev) = previous.filter(|&prev| ts - prev > interval * GAP_INTERVALS) {
            push_band(&mut gaps, prev + interval, ts, &x_of, right);
        }
        previous = Some(ts);
    }
    if let Some(last) = previous.filter(|&last| now - last > interval * GAP_INTERVALS) {
        push_band(&mut gaps, last + interval, now, &x_of, right);
    }
    gaps
}

/// Axis stamps: time of day in the short window, dates in the long ones.
fn chart_stamp(range: ChartRange, ts: i64) -> String {
    match range {
        ChartRange::Day => short_stamp(ts),
        ChartRange::Week | ChartRange::Quarter => date_label(date_of(ts)),
    }
}

pub fn chart(checks: &[ChartCheck], interval_s: i64, now: i64, range: ChartRange) -> Chart {
    let window = range.seconds();
    let start = now - window;
    // Only a successful check produced an answer time worth plotting. A failed
    // check is an outage marker even when the probe still measured a latency.
    let samples: Vec<(i64, i64)> = checks
        .iter()
        .filter_map(|&(ts, ok, latency)| {
            if ok && ts >= start && ts <= now {
                latency.map(|ms| (ts, ms))
            } else {
                None
            }
        })
        .collect();

    let base = CHART_HEIGHT - PAD_BOTTOM;
    let top = PAD_TOP;
    // The y labels live outside the SVG, so the plot fills its full width.
    let left = 0;
    let right = CHART_WIDTH - PAD_RIGHT;
    let span = window.max(1);
    let x_of = |ts: i64| left + (ts - start) * (right - left) / span;
    let bands = outage_bands(checks, interval_s, start, now, &x_of, right);
    let gaps = data_gaps(checks, interval_s, start, now, &x_of, right);

    if samples.is_empty() && bands.is_empty() {
        return Chart {
            empty: true,
            width: CHART_WIDTH,
            height: CHART_HEIGHT,
            segments: Vec::new(),
            dots: Vec::new(),
            bands: Vec::new(),
            gaps: Vec::new(),
            plot_height: base - top,
            alt: "Keine Messwerte".to_string(),
            caption: format!(
                "Keine erfolgreichen Checks in den letzten {}.",
                range.caption_period()
            ),
            max_label: String::new(),
            mid_label: String::new(),
            mid_visible: false,
            zero_label: String::new(),
            start_label: String::new(),
            end_label: String::new(),
            y_top: top,
            y_mid: (top + base) / 2,
            y_base: base,
            x_left: left,
            x_right: right,
            y_top_pct: chart_pct(top),
            y_mid_pct: chart_pct((top + base) / 2),
            y_base_pct: chart_pct(base),
            label_time: CHART_HEIGHT - 4,
            end_anchor_x: right,
        };
    }

    // A window without a single success still draws the outages; there is no
    // latency scale to label, so the axis stays blank.
    if samples.is_empty() {
        return Chart {
            empty: false,
            width: CHART_WIDTH,
            height: CHART_HEIGHT,
            segments: Vec::new(),
            dots: Vec::new(),
            bands,
            gaps,
            plot_height: base - top,
            alt: format!(
                "Keine Antwortzeiten in den letzten {}; Ausfälle markiert",
                range.alt_period()
            ),
            caption: format!(
                "Keine erfolgreichen Checks in den letzten {}.",
                range.caption_period()
            ),
            max_label: String::new(),
            mid_label: String::new(),
            mid_visible: false,
            zero_label: String::new(),
            start_label: chart_stamp(range, start),
            end_label: chart_stamp(range, now),
            y_top: top,
            y_mid: (top + base) / 2,
            y_base: base,
            x_left: left,
            x_right: right,
            y_top_pct: chart_pct(top),
            y_mid_pct: chart_pct((top + base) / 2),
            y_base_pct: chart_pct(base),
            label_time: CHART_HEIGHT - 4,
            end_anchor_x: right,
        };
    }

    let highest = samples.iter().map(|&(_, ms)| ms).max().unwrap_or(0);
    let scale = nice_max(highest);
    let bucket_span = (span + CHART_BUCKETS - 1) / CHART_BUCKETS;
    let y_of = |ms: i64| base - ms * (base - top) / scale;
    // A midpoint tick only makes sense on an even scale; the nice values are
    // even from 2 ms upward.
    let mid_visible = scale >= 2 && scale % 2 == 0;
    let y_mid = y_of(scale / 2);

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
        bands,
        gaps,
        plot_height: base - top,
        alt: format!(
            "Antwortzeiten der letzten {} zwischen {} und {}",
            range.alt_period(),
            latency(Some(lowest)),
            latency(Some(highest))
        ),
        caption: format!(
            "Median = {}, Max = {}",
            latency(Some(median)),
            latency(Some(highest))
        ),
        max_label: axis_latency(scale),
        mid_label: if mid_visible {
            axis_latency(scale / 2)
        } else {
            String::new()
        },
        mid_visible,
        zero_label: "0".to_string(),
        start_label: chart_stamp(range, start),
        end_label: chart_stamp(range, now),
        y_top: top,
        y_mid,
        y_base: base,
        x_left: left,
        x_right: right,
        y_top_pct: chart_pct(top),
        y_mid_pct: chart_pct(y_mid),
        y_base_pct: chart_pct(base),
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

/// Start of the feed window: local midnight 30 days back, so the window does
/// not drift with DST.
pub fn feed_since(now: i64) -> i64 {
    let start = day::shift(date_of(now), -FEED_WINDOW_DAYS);
    day::bounds(start).0
}

/// Relative path of a target's own feed.
pub fn feed_href(target_id: &str) -> String {
    format!("/t/{target_id}/feed.xml")
}

/// One feed entry: a batch of incidents of a single target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    pub guid: String,
    pub title: String,
    pub link: String,
    pub pub_date: String,
    pub description: String,
    pub category: String,
}

#[derive(Debug, Clone)]
struct Batch<'a> {
    incidents: Vec<&'a FeedIncident>,
}

impl<'a> Batch<'a> {
    fn new(incident: &'a FeedIncident) -> Self {
        Self {
            incidents: vec![incident],
        }
    }

    fn target_id(&self) -> &str {
        &self.incidents[0].target_id
    }

    fn started_at(&self) -> i64 {
        self.incidents[0].started_at
    }

    /// When the batch last changed: the end of its newest incident, or now
    /// while one is still running.
    fn last_activity(&self, now: i64) -> i64 {
        self.incidents
            .last()
            .and_then(|incident| incident.ended_at)
            .unwrap_or(now)
    }

    fn joinable(&self, incident: &FeedIncident, now: i64) -> bool {
        incident.target_id == self.target_id()
            && incident.started_at - self.last_activity(now) <= FEED_BATCH_GAP_S
    }

    /// A batch is feed-worthy once one incident in it lasted the minimum
    /// duration. A closed blip never ages into eligibility; an open incident
    /// does at `started_at + FEED_MIN_INCIDENT_S`.
    fn publishable(&self, now: i64) -> bool {
        self.incidents.iter().any(|incident| {
            incident.ended_at.unwrap_or(now) - incident.started_at >= FEED_MIN_INCIDENT_S
        })
    }

    fn item(&self, base_url: &str) -> FeedItem {
        let first = self.incidents[0];
        let count = self.incidents.len();
        let open = self
            .incidents
            .iter()
            .any(|incident| incident.ended_at.is_none());
        let end = self
            .incidents
            .iter()
            .filter_map(|incident| incident.ended_at)
            .max();
        let recovered: i64 = self
            .incidents
            .iter()
            .filter_map(|incident| {
                incident
                    .ended_at
                    .map(|ended_at| ended_at - incident.started_at)
            })
            .sum();

        let title = match (count, open) {
            (1, true) => format!(
                "{}: Ausfall seit {}",
                first.target_name,
                stamp(first.started_at)
            ),
            (1, false) => format!(
                "{}: Ausfall am {}",
                first.target_name,
                date_label(at(first.started_at).date())
            ),
            (_, true) => format!("{}: {count} Ausfälle, aktuell gestört", first.target_name),
            (_, false) => format!("{}: {count} Ausfälle", first.target_name),
        };

        let mut lines = vec![
            format!("Beginn: {}", stamp(first.started_at)),
            match end {
                Some(ended_at) => format!("Ende: {}", stamp(ended_at)),
                None => "Ende: andauernd".to_string(),
            },
        ];
        if count == 1 {
            if let Some(ended_at) = end {
                lines.push(format!("Dauer: {}", duration(ended_at - first.started_at)));
            }
        } else {
            lines.push(format!("Ausfälle: {count}"));
            if recovered > 0 {
                lines.push(format!("Behobene Dauer: {}", duration(recovered)));
            }
        }

        let mut causes = Vec::new();
        for incident in &self.incidents {
            let cause = cause_de(&incident.cause);
            if !causes.contains(&cause) {
                causes.push(cause);
            }
        }
        lines.push(format!("Ursachen: {}", causes.join(", ")));

        if let Some(detail) = self
            .incidents
            .iter()
            .filter_map(|incident| detail_extra(&incident.cause, incident.detail.as_deref()))
            .next()
        {
            lines.push(format!("Details: {}", truncate_detail(&detail)));
        }

        FeedItem {
            guid: format!("kaputt:v1:{}:{}", first.target_id, first.id),
            title,
            link: format!("{base_url}/t/{}#stoerungen", first.target_id),
            pub_date: day::rfc2822(first.started_at),
            description: lines.join("\n"),
            category: first.target_name.clone(),
        }
    }
}

fn truncate_detail(text: &str) -> String {
    if text.chars().count() <= FEED_DETAIL_MAX {
        return text.to_string();
    }
    let mut short: String = text.chars().take(FEED_DETAIL_MAX - 1).collect();
    short.push('…');
    short
}

/// Collapse incidents into feed items: mature batches only, newest first.
/// Returns the items and the newest activity, which is the feed's
/// `lastBuildDate`.
pub fn feed_items(
    base_url: &str,
    incidents: &[FeedIncident],
    now: i64,
) -> (Vec<FeedItem>, Option<i64>) {
    let mut incidents: Vec<&FeedIncident> = incidents.iter().collect();
    incidents.sort_by_key(|incident| {
        (
            incident.target_id.as_str(),
            incident.started_at,
            incident.id,
        )
    });

    let mut batches: Vec<Batch> = Vec::new();
    for incident in incidents {
        match batches.last_mut() {
            Some(batch) if batch.joinable(incident, now) => batch.incidents.push(incident),
            _ => batches.push(Batch::new(incident)),
        }
    }

    let window_start = feed_since(now);
    let mut items: Vec<(i64, i64, FeedItem)> = batches
        .into_iter()
        .filter(|batch| batch.last_activity(now) >= window_start)
        .filter(|batch| batch.publishable(now))
        .map(|batch| {
            let started_at = batch.started_at();
            let last_activity = batch.last_activity(now);
            (started_at, last_activity, batch.item(base_url))
        })
        .collect();
    items.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    items.truncate(FEED_MAX_ITEMS);
    let last_activity = items.iter().map(|(_, activity, _)| *activity).max();
    (
        items.into_iter().map(|(_, _, item)| item).collect(),
        last_activity,
    )
}

#[derive(Template)]
#[template(path = "feed.xml")]
pub struct FeedPage {
    pub title: String,
    pub link: String,
    pub self_link: String,
    pub description: String,
    pub last_build: String,
    pub items: Vec<FeedItem>,
}

/// Head and items of one feed. `target` selects the single-target feed; the
/// absolute links come from the configured public origin.
pub fn feed_page(
    base_url: &str,
    target: Option<&TargetState>,
    incidents: &[FeedIncident],
    now: i64,
) -> FeedPage {
    let base = base_url.trim_end_matches('/');
    let (title, link, self_link) = match target {
        Some(target) => (
            format!("{} — kaputt.matze.lol", target.name),
            format!("{base}/t/{}", target.id),
            format!("{base}{}", feed_href(&target.id)),
        ),
        None => (
            "kaputt.matze.lol — Status".to_string(),
            format!("{base}/"),
            format!("{base}/feed.xml"),
        ),
    };
    let (items, last_activity) = feed_items(base, incidents, now);
    FeedPage {
        title,
        link,
        self_link,
        description: "Antworten Karlsruher Stadtseiten, privat gemessen.".to_string(),
        last_build: day::rfc2822(last_activity.unwrap_or(now)),
        items,
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
    pub feed_href: String,
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

    /// Successful chart checks with a measured latency, for the chart tests.
    fn measured(samples: &[(i64, i64)]) -> Vec<ChartCheck> {
        samples
            .iter()
            .map(|&(ts, ms)| (ts, true, Some(ms)))
            .collect()
    }

    fn stat(checks: u32, ok: u32, down_seconds: i64) -> DayStat {
        DayStat {
            checks,
            ok,
            down_seconds,
        }
    }

    fn feed_incident(id: i64, target: &str, start: i64, end: Option<i64>) -> FeedIncident {
        FeedIncident {
            id,
            target_id: target.into(),
            target_name: target.to_ascii_uppercase(),
            started_at: start,
            ended_at: end,
            cause: "timeout".into(),
            detail: Some("operation timed out".into()),
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
    fn answer_times_switch_to_seconds_with_one_decimal() {
        assert_eq!(latency(None), "—");
        assert_eq!(latency(Some(0)), "0 ms");
        assert_eq!(latency(Some(999)), "999 ms");
        assert_eq!(latency(Some(1_000)), "1,0 s");
        assert_eq!(latency(Some(1_400)), "1,4 s");
        assert_eq!(latency(Some(2_250)), "2,3 s");
        assert_eq!(latency(Some(25_920)), "25,9 s");
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
        let chart = chart(&measured(&samples), 300, now, ChartRange::Day);
        assert!(!chart.empty);
        assert_eq!(chart.segments.len(), 2);
        assert_eq!(chart.dots.len(), 2);
        assert_eq!(chart.max_label, "200 ms");
        assert_eq!(chart.mid_label, "100 ms");
        assert!(chart.mid_visible);
        let scale = 200;
        let span = chart.y_base - chart.y_top;
        assert_eq!(chart.y_top_pct, "7.500%");
        assert_eq!(chart.y_mid_pct, "46.667%");
        assert_eq!(chart.y_base_pct, "85.833%");
        assert_eq!(chart.dots[0].x, chart.x_left);
        assert_eq!(chart.dots[0].y, chart.y_base - 100 * span / scale);
        assert_eq!(chart.dots[1].x, chart.x_right);
        assert_eq!(chart.dots[1].y, chart.y_base - 200 * span / scale);
        assert_eq!(chart.y_mid, chart.y_base - 100 * span / scale);
    }

    #[test]
    fn chart_axis_uses_seconds_and_adds_the_half_tick() {
        let now = 1_000_000;
        let slow = chart(&measured(&[(now - 10, 10_000)]), 300, now, ChartRange::Day);
        assert_eq!(slow.max_label, "10 s");
        assert_eq!(slow.mid_label, "5 s");
        assert!(slow.mid_visible);
        assert_eq!(slow.zero_label, "0");

        let fractional = chart(&measured(&[(now - 10, 2_500)]), 300, now, ChartRange::Day);
        assert_eq!(fractional.max_label, "5 s");
        assert_eq!(fractional.mid_label, "2,5 s");
        assert_eq!(fractional.caption, "Median = 2,5 s, Max = 2,5 s");
    }

    #[test]
    fn axis_labels_drop_a_zero_decimal() {
        assert_eq!(axis_latency(999), "999 ms");
        assert_eq!(axis_latency(1_000), "1 s");
        assert_eq!(axis_latency(1_400), "1,4 s");
        assert_eq!(axis_latency(2_500), "2,5 s");
        assert_eq!(axis_latency(10_000), "10 s");
    }

    #[test]
    fn chart_keeps_dense_samples_in_one_segment() {
        let now = 1_000_000;
        let samples: Vec<(i64, i64)> = (0..10)
            .map(|index| (now - 3600 + index * 300, 120 + index))
            .collect();
        let chart = chart(&measured(&samples), 300, now, ChartRange::Day);
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
        let chart = chart(
            &measured(&[(now - 100, 100), (now - 50, 200)]),
            300,
            now,
            ChartRange::Day,
        );
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
        let week = chart(&measured(&samples), 300, now, ChartRange::Week);
        assert_eq!(points(&week), 1);
        assert!(!week.start_label.contains(':'));

        let day = chart(&measured(&samples), 300, now, ChartRange::Day);
        assert_eq!(points(&day), 1);
        assert!(day.start_label.contains(':'));

        let quarter = chart(&measured(&samples), 300, now, ChartRange::Quarter);
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
        let chart = chart(&measured(&samples), 300, now, ChartRange::Day);
        assert_eq!(chart.segments.len(), 2);
    }

    #[test]
    fn chart_marks_a_single_sample_with_a_dot() {
        let now = 1_000_000;
        let chart = chart(&measured(&[(now - 10, 120)]), 300, now, ChartRange::Day);
        assert_eq!(chart.segments.len(), 1);
        assert_eq!(chart.dots.len(), 1);
    }

    #[test]
    fn chart_marks_failed_checks_as_outages() {
        let now = 1_000_000;
        let checks = vec![
            (now - 5000, true, Some(100)),
            (now - 4000, false, None),
            // Recovery 300 s after the failure, inside the interval cap.
            (now - 3700, true, Some(120)),
        ];
        let chart = chart(&checks, 300, now, ChartRange::Day);
        assert_eq!(chart.bands.len(), 1);
        let band = &chart.bands[0];
        assert!(band.x >= chart.x_left);
        assert!(band.x + band.width <= chart.x_right);
        // The failed check is not a latency sample.
        let points: usize = chart
            .segments
            .iter()
            .map(|segment| segment.split(' ').count())
            .sum();
        assert_eq!(points, 2);
    }

    #[test]
    fn chart_caps_failures_at_one_interval_and_merges_the_run() {
        let now = 1_000_000;
        let checks = vec![
            (now - 5000, false, None),
            (now - 4700, false, None),
            (now - 4400, false, None),
            // A long gap to the next success must not turn into downtime.
            (now - 600, true, Some(100)),
        ];
        let chart = chart(&checks, 300, now, ChartRange::Day);
        assert_eq!(chart.bands.len(), 1);
        // Three intervals are 900 s; the plot is 712 px wide over 24 h, so
        // the band stays below ten pixels instead of stretching to recovery.
        assert!(chart.bands[0].width > MIN_BAND_WIDTH);
        assert!(chart.bands[0].width < 10);
    }

    #[test]
    fn chart_draws_outages_even_without_a_successful_check() {
        let now = 1_000_000;
        let checks = vec![
            // A status failure still carries a latency; it is an outage, not
            // an answer time.
            (now - 1000, false, Some(500)),
            (now - 700, false, None),
        ];
        let chart = chart(&checks, 300, now, ChartRange::Day);
        assert!(!chart.empty);
        assert!(chart.segments.is_empty());
        assert!(chart.dots.is_empty());
        assert_eq!(chart.bands.len(), 1);
        assert!(chart.max_label.is_empty());
        assert!(!chart.mid_visible);
        assert_eq!(
            chart.caption,
            "Keine erfolgreichen Checks in den letzten 24 Stunden."
        );
    }

    #[test]
    fn chart_clamps_an_ongoing_outage_to_now() {
        let now = 1_000_000;
        let chart = chart(&[(now - 30, false, None)], 300, now, ChartRange::Day);
        assert_eq!(chart.bands.len(), 1);
        let band = &chart.bands[0];
        assert!(band.width >= MIN_BAND_WIDTH);
        assert_eq!(band.x + band.width, chart.x_right);
    }

    #[test]
    fn chart_marks_long_missing_runs_as_data_gaps() {
        let now = 1_000_000;
        let checks = vec![
            (now - 1000, true, Some(100)),
            // No check for 700 s: more than the two intervals of jitter.
            (now - 300, true, Some(110)),
            (now, true, Some(120)),
        ];
        let marked = chart(&checks, 300, now, ChartRange::Day);
        assert_eq!(marked.gaps.len(), 1);
        let gap = &marked.gaps[0];
        assert!(gap.x > marked.x_left);
        assert!(gap.x + gap.width < marked.x_right);

        // One late check is jitter, not a gap.
        let jitter = vec![
            (now - 1000, true, Some(100)),
            (now - 500, true, Some(110)),
            (now, true, Some(120)),
        ];
        assert!(chart(&jitter, 300, now, ChartRange::Day).gaps.is_empty());
    }

    #[test]
    fn chart_keeps_trailing_gaps_but_not_the_leading_edge() {
        let now = 1_000_000;
        let checks = vec![
            (now - 1000, true, Some(100)),
            (now - 300, true, Some(110)),
            (now, true, Some(120)),
        ];
        let leading = chart(&checks, 300, now, ChartRange::Day);
        // The 1,000 s before the first check cannot be judged without a
        // check before the window, so it stays blank.
        assert_eq!(leading.gaps.len(), 1);
        assert!(leading.gaps[0].x > leading.x_left);

        let trailing = chart(&[(now - 5_000, true, Some(100))], 300, now, ChartRange::Day);
        assert_eq!(trailing.gaps.len(), 1);
        let gap = &trailing.gaps[0];
        assert_eq!(gap.x + gap.width, leading.x_right);
    }

    #[test]
    fn chart_puts_the_data_gap_after_the_confirmed_outage() {
        let now = 1_000_000;
        let checks = vec![
            (now - 1000, false, None),
            // Recovery after a long prober gap: red ends at the interval cap,
            // the unobserved rest is the neutral gap.
            (now - 300, true, Some(100)),
            (now, true, Some(120)),
        ];
        let chart = chart(&checks, 300, now, ChartRange::Day);
        assert_eq!(chart.bands.len(), 1);
        assert_eq!(chart.gaps.len(), 1);
        let band = &chart.bands[0];
        let gap = &chart.gaps[0];
        assert!(gap.x >= band.x + band.width);
    }

    #[test]
    fn chart_is_empty_without_checks() {
        let day = chart(&[], 300, 1_000_000, ChartRange::Day);
        assert!(day.empty);
        assert!(day.segments.is_empty());
        assert!(day.bands.is_empty());
        assert_eq!(
            day.caption,
            "Keine erfolgreichen Checks in den letzten 24 Stunden."
        );

        let week = chart(&[], 300, 1_000_000, ChartRange::Week);
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
        assert_eq!(feed_href("opac"), "/t/opac/feed.xml");
    }

    #[test]
    fn feed_drops_a_lone_blip() {
        let now = berlin(2026, 10, 30, 12);
        let incidents = [feed_incident(1, "a", now - 3600, Some(now - 3300))];
        let (items, last) = feed_items("https://example.org", &incidents, now);
        assert!(items.is_empty());
        assert_eq!(last, None);
    }

    #[test]
    fn feed_publishes_an_open_incident_when_it_matures() {
        let now = berlin(2026, 10, 30, 12);
        let young = [feed_incident(1, "a", now - FEED_MIN_INCIDENT_S + 1, None)];
        assert!(feed_items("https://example.org", &young, now).0.is_empty());

        let mature = [feed_incident(1, "a", now - FEED_MIN_INCIDENT_S, None)];
        let (items, _) = feed_items("https://example.org", &mature, now);
        assert_eq!(items.len(), 1);
        assert!(items[0].title.contains("Ausfall seit"));
        assert!(items[0].description.contains("Ende: andauernd"));
        assert!(!items[0].description.contains("Dauer:"));
    }

    #[test]
    fn feed_batches_nearby_incidents_and_keeps_the_first_guid() {
        let now = berlin(2026, 10, 30, 12);
        let incidents = [
            feed_incident(1, "a", now - 10_000, Some(now - 9_000)),
            feed_incident(2, "a", now - 8_000, Some(now - 7_000)),
        ];
        let (items, _) = feed_items("https://example.org", &incidents, now);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].guid, "kaputt:v1:a:1");
        assert_eq!(items[0].title, "A: 2 Ausfälle");
        assert!(items[0].description.contains("Ausfälle: 2"));
        assert!(items[0].description.contains("Behobene Dauer: 33 Min"));
        assert_eq!(items[0].link, "https://example.org/t/a#stoerungen");
    }

    #[test]
    fn feed_keeps_incidents_beyond_the_gap_apart() {
        let now = berlin(2026, 10, 30, 12);
        let incidents = [
            feed_incident(1, "a", now - 100_000, Some(now - 90_000)),
            feed_incident(2, "a", now - 60_000, Some(now - 50_000)),
        ];
        let (items, _) = feed_items("https://example.org", &incidents, now);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].guid, "kaputt:v1:a:2");
        assert_eq!(items[1].guid, "kaputt:v1:a:1");
    }

    #[test]
    fn feed_never_batches_across_targets() {
        let now = berlin(2026, 10, 30, 12);
        let incidents = [
            feed_incident(1, "a", now - 10_000, Some(now - 9_000)),
            feed_incident(2, "b", now - 8_500, Some(now - 7_500)),
        ];
        let (items, _) = feed_items("https://example.org", &incidents, now);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].link, "https://example.org/t/b#stoerungen");
    }

    #[test]
    fn feed_hides_incidents_that_ended_before_the_window() {
        let now = berlin(2026, 10, 30, 12);
        let incidents = [feed_incident(
            1,
            "a",
            now - 40 * 86400,
            Some(now - 40 * 86400 + 3600),
        )];
        assert!(
            feed_items("https://example.org", &incidents, now)
                .0
                .is_empty()
        );
    }

    #[test]
    fn feed_caps_items_newest_first() {
        let now = berlin(2026, 10, 30, 12);
        let incidents: Vec<FeedIncident> = (0..60)
            .map(|index| {
                let start = now - index * 13 * 3600;
                feed_incident(index, "a", start, Some(start + 1200))
            })
            .collect();
        let (items, _) = feed_items("https://example.org", &incidents, now);
        assert_eq!(items.len(), FEED_MAX_ITEMS);
        assert_eq!(items[0].guid, "kaputt:v1:a:0");
    }

    #[test]
    fn feed_page_builds_absolute_links() {
        let now = berlin(2026, 10, 30, 12);
        let state = TargetState {
            id: "a".into(),
            name: "A".into(),
            url: "https://a.example.org/".into(),
            expect_status: 200,
            interval_s: 300,
            last_check: None,
            last_ok_ts: None,
            open_incident: None,
        };
        let all = feed_page("https://status.example.org/", None, &[], now);
        assert_eq!(all.self_link, "https://status.example.org/feed.xml");
        assert_eq!(all.link, "https://status.example.org/");
        assert!(!all.last_build.is_empty());

        let one = feed_page("https://status.example.org/", Some(&state), &[], now);
        assert_eq!(one.self_link, "https://status.example.org/t/a/feed.xml");
        assert_eq!(one.link, "https://status.example.org/t/a");
        assert_eq!(one.title, "A — kaputt.matze.lol");
    }

    #[test]
    fn feed_xml_escapes_remote_text() {
        let now = berlin(2026, 10, 30, 12);
        let mut incident = feed_incident(1, "a", now - 1200, None);
        incident.target_name = "Stadt <X> & Co".into();
        incident.cause = "status".into();
        incident.detail = Some("<script>alert(1)</script>".into());
        let page = feed_page("https://example.org", None, &[incident], now);
        let xml = page.render().expect("feed renders");
        assert!(xml.contains("&#60;X&#62;"));
        assert!(xml.contains("&#60;script&#62;"));
        assert!(!xml.contains("<X>"));
        assert!(!xml.contains("<script>"));
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    }
}
