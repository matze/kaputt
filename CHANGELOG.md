# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
