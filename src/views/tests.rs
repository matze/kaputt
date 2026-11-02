use super::*;

fn berlin(y: i16, m: i8, d: i8, hour: i8) -> i64 {
    day::TZ
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
    let (items, last) = feed_items(&incidents, now);
    assert!(items.is_empty());
    assert_eq!(last, None);
}

#[test]
fn feed_publishes_an_open_incident_when_it_matures() {
    let now = berlin(2026, 10, 30, 12);
    let young = [feed_incident(1, "a", now - FEED_MIN_INCIDENT_S + 1, None)];
    assert!(feed_items(&young, now).0.is_empty());

    let mature = [feed_incident(1, "a", now - FEED_MIN_INCIDENT_S, None)];
    let (items, _) = feed_items(&mature, now);
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
    let (items, _) = feed_items(&incidents, now);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].guid, "kaputt:v1:a:1");
    assert_eq!(items[0].title, "A: 2 Ausfälle");
    assert!(items[0].description.contains("Ausfälle: 2"));
    assert!(items[0].description.contains("Behobene Dauer: 33 Min"));
    assert_eq!(items[0].link, "https://kaputt.matze.lol/t/a#stoerungen");
}

#[test]
fn feed_keeps_incidents_beyond_the_gap_apart() {
    let now = berlin(2026, 10, 30, 12);
    let incidents = [
        feed_incident(1, "a", now - 100_000, Some(now - 90_000)),
        feed_incident(2, "a", now - 60_000, Some(now - 50_000)),
    ];
    let (items, _) = feed_items(&incidents, now);
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
    let (items, _) = feed_items(&incidents, now);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].link, "https://kaputt.matze.lol/t/b#stoerungen");
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
    assert!(feed_items(&incidents, now).0.is_empty());
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
    let (items, _) = feed_items(&incidents, now);
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
    let all = feed_page(None, &[], now);
    assert_eq!(all.self_link, "https://kaputt.matze.lol/feed.xml");
    assert_eq!(all.link, "https://kaputt.matze.lol/");
    assert!(!all.last_build.is_empty());

    let one = feed_page(Some(&state), &[], now);
    assert_eq!(one.self_link, "https://kaputt.matze.lol/t/a/feed.xml");
    assert_eq!(one.link, "https://kaputt.matze.lol/t/a");
    assert_eq!(one.title, "A — kaputt.matze.lol");
}

#[test]
fn feed_xml_escapes_remote_text() {
    let now = berlin(2026, 10, 30, 12);
    let mut incident = feed_incident(1, "a", now - 1200, None);
    incident.target_name = "Stadt <X> & Co".into();
    incident.cause = "status".into();
    incident.detail = Some("<script>alert(1)</script>".into());
    let page = feed_page(None, &[incident], now);
    let xml = page.render().expect("feed renders");
    assert!(xml.contains("&#60;X&#62;"));
    assert!(xml.contains("&#60;script&#62;"));
    assert!(!xml.contains("<X>"));
    assert!(!xml.contains("<script>"));
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
}
