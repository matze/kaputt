# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Pages and feeds show only the German cause label, not the raw probe error
  detail.
- The latency chart bounds an outage with red lines at its start and end
  instead of filling the whole stretch, so a long outage no longer hides the
  curve.

## [0.7.0]

### Changed

- **Breaking**: drop the command line probing.
- **Breaking**: drop the `base_url` config key. Feed links always use the public
  origin `https://kaputt.matze.lol`.

## [0.6.0]

### Changed

- Compress HTML pages, the HTMX status partial, RSS feeds and embedded text
  assets with Brotli or gzip.

## [0.5.0]

### Added

- RSS feeds for all enabled targets at `/feed.xml` and for one target at
  `/t/{id}/feed.xml`. Incidents are batched by target and a six-hour gap; a
  batch appears after fifteen minutes and updates in place instead of adding
  an entry per check or bounce.

## [0.3.0]

### Added

- The detail latency chart marks failed checks as red outage bands and
  stretches without any check as neutral gray "keine Daten" bands, so an
  outage stays distinct from a prober gap. A failure covers at most one probe
  interval, matching the uptime rollups.

## [0.2.0]

### Added

- Detail page latency chart switches between 24 hours, 7 days and 90 days;
  longer windows are bucketed and averaged for a smooth line. Axis labels use
  seconds for whole seconds, stay integer milliseconds otherwise, show a
  midpoint tick and stay visible on phones.

## [0.1.0]

### Added

- Status page for Karlsruhe city sites on <https://kaputt.matze.lol>.
