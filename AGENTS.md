# Agent instructions

## High-level design

This jj repo contains the source for kaputt.matze.lol, a status page in the
style of uptime robot or [the unofficial github status
page](https://mrshu.github.io/github-statuses). It monitors pages served by the
city of Karlsruhe, including karlsruhe.de, stadtbibliothek.karlsruhe.de,
opac.karlsruhe.de and geoportal.karlsruhe.de. Add further targets when they
make sense.

## Tech stack

- Rust backend
- sqlite database
- HTMX if necessary
- Simple, self-contained CSS
- Self-hosted Geist font

## Commands

```sh
cargo test                    # all tests are #[cfg(test)] module tests
cargo run -- serve            # web UI, scheduler and rollups (default command)
cargo run -- probe --once     # one round of checks, print and store
cargo run -- --config PATH probe --once
```

`--config` comes before the command. The default is `config.toml`. Default
rustfmt and clippy settings apply.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI parsing, startup, one-shot probe mode |
| `src/config.rs` | `config.toml` schema and validation, targets upserted at startup |
| `src/db.rs` | sqlite store, ordered migrations, queries |
| `src/probe.rs` | header-only HTTP probe, `ErrorKind` vocabulary |
| `src/scheduler.rs` | per-target loop and incident state machine |
| `src/rollup.rs` | `daily_stats` rollups and retention |
| `src/views.rs` | aggregation, German formatting, view structs, RSS batching |
| `src/day.rs` | Europe/Berlin local days and timestamps |
| `src/server.rs` | axum routes and embedded assets |
| `src/<module>/tests.rs` | unit tests of `src/<module>.rs`, declared as `#[cfg(test)] mod tests;` |
| `templates/`, `assets/` | askama templates, CSS, vendored htmx, Geist woff2 |

## Rules

- Manage targets only through `config.toml` and provide no UI or API for
  editing them. Removing a target disables it at the next startup and keeps its
  history.
- Append schema changes as a new `SCHEMA_Vn` and extend `MIGRATIONS`. Never edit
  a shipped migration. Step `n` sets `PRAGMA user_version` to `n + 1`.
- Store Unix seconds in UTC. Format timestamps and bucket days through `day.rs`
  (Europe/Berlin, DST-aware). Never hardcode an offset or do date math on
  86400-second days.
- Templates only render values computed and tested in `views.rs`. The UI uses
  German text, dates as `TT.MM.JJJJ` and decimal commas.
- Vendor frontend files into `assets/` and embed them with `include_bytes!`. The
  frontend loads no CDN, external font or JS framework. HTMX polls
  `/partials/status` every 30 s and swaps `#status` with `outerHTML`.
- Probe only the response head (status and headers) and drop the body. Error
  kinds are the stored strings in `probe::ErrorKind`, so keep
  `checks.error_kind` values stable.
- Rebuild incident state from stored rows on every check. A restart therefore
  resumes the last stored check and cannot duplicate an incident. The partial
  unique index `incidents_open_per_target` allows at most one open incident per
  target.
- `daily_stats` holds one row per target and local day. Raw checks live 90 days,
  rollups 2 years. A data gap never counts as downtime.
- RSS feeds are read-only projections of `incidents`, never a second event
  stream. `/feed.xml` covers enabled targets, `/t/{id}/feed.xml` one target even
  after it is disabled. One item per target and disturbance batch: incidents
  less than 6 h apart merge, a batch appears once one incident lasted 15 min,
  the window is 30 local days and the feed is capped at 50 items. The GUID is
  `kaputt:v1:<target>:<first incident id>` and later incidents update that item
  in place. Never emit per-check or per-recovery items, and never change the
  incident thresholds or stored rows for the feed. Absolute links use
  `base_url` from `config.toml`, not the request `Host`.
- Serve pages with `Cache-Control: no-store`. `/healthz` is liveness only: it
  queries no database and contacts no target.

## Verification

`cargo test` covers config parsing, rollups, incidents, feed batching and view
rendering. For a smoke test, run `cargo run -- probe --once` against real
targets. Non-zero exit means at least one target is down. Then run
`cargo run -- serve` and check `curl -s localhost:8080/healthz`,
`curl -s localhost:8080/` and `curl -s localhost:8080/feed.xml`. `kaputt.db` is
generated and gitignored. Delete it to start with an empty history.
