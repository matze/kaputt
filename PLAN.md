# Plan

Research result for implementing kaputt.matze.lol. Order is deliberate: the
probe core and schema are useful without the UI, and the UI without real data
would only produce mock-ups.

## Findings

Checked 2026-10-30.

| Target | DNS | Observed | Notes |
|---|---|---|---|
| `https://www.karlsruhe.de/` | 193.197.165.152 | 200, Typo3, `<title>Karlsruhe: Startseite` | apex `https://karlsruhe.de/` answers 307 → www, `http://www…` answers 301 |
| `https://stadtbibliothek.karlsruhe.de/` | 193.197.165.152 | 200, Typo3, `<title>Karlsruhe: Stadtbibliothek` | `www.stadtbibliothek…` has no DNS record; monitor the apex |
| `https://opac.karlsruhe.de/` | 193.197.165.196 | 200, Koha catalogue, `<title>Stadtbibliothek Karlsruhe - Katalog` | server-rendered, stable title |
| `https://geoportal.karlsruhe.de/` | 193.197.165.182 | 301, JS-heavy | public nodes saw both 301 and connect timeouts; final target must be verified from the production host |

Reachability: this laptop normally reaches the sites (user-confirmed), but at
the time of research all four were unreachable from it: TCP 80/443 to all three
city addresses timed out while other German sites answered. That is the current
outage, not a block. Public multi-node checks simultaneously returned 200/301
from BR/CH/SE/ES/HU/IN/US/NL/IT nodes and timeouts from IR/UA/SG, so the outage
is partial and path-specific rather than total. Consequences:

- Real probes cannot be validated from here while the outage lasts. The probe
  path needs a `probe --once` subcommand for the deployment host, plus local
  stub-server tests.
- A single vantage point shows "kaputt" while most of the world still gets 200.
  The UI must show error kind (DNS, TCP, TLS, timeout, HTTP status), probe
  location and last successful check, not just "down".
- The hosting vantage defines what "down" means. A monitor on a different
  network can stay green during exactly the outage this machine sees. Decide
  later whether the page reports the probe's own view or needs a second
  vantage.

`robots.txt` on karlsruhe.de is a 404; no crawl restrictions found. At the
chosen 5 min interval (≈1.2k requests/day for four targets) the load is
negligible, but requests should carry an identifying User-Agent with a contact
URL.

No existing official Karlsruhe status page was found; incidents are not
published in a machine-readable form.

Toolchain present: rustc/cargo 1.99. No system `sqlite3`, so use the bundled
SQLite build.

## Decisions (review 2026-10-30)

- Hosting: undecided. Plan stays host-agnostic; systemd + reverse proxy is the
  default shape, but nothing depends on a specific provider. The chosen vantage
  still decides whether the page reflects this network's view or a foreign
  one's; that must be an explicit choice at deploy time, not a side effect.
- Targets: the four roots from `DESIGN.md`. The selection came back empty, so
  this is an assumption; changing it only edits `config.toml`.
- Health criterion: expected HTTP status only. No content marker.
- Cadence: every 5 minutes; raw checks kept 90 days; daily rollups kept 2 years.
- Alerting: none in v1; the public page is the notification.
- Targets live in `config.toml` and are upserted at startup; no admin UI.

Accepted tradeoff of status-only checks: a maintenance or error page that
returns 200 counts as healthy. The UI will show status code and latency, so a
human can still spot anomalies, but nothing flags them automatically.

## Stack

- Rust 2024, single binary, one SQLite file (WAL).
- `axum` 0.8, `tokio` 1.54, `reqwest` 0.13 (rustls), `askama` 0.16 templates,
  `tracing`.
- SQLite via `tokio-rusqlite` 0.8 (async wrapper); it pins `rusqlite` 0.40, so
  add `rusqlite = { version = "0.40", features = ["bundled"] }` as a direct
  dependency to enable the bundled build.
- HTMX 2.0.11 vendored into the binary for auto-refreshing status; no CDN, no
  other JavaScript, no fonts.
- CSS handwritten and embedded; no framework.

Rationale: probe volume is tiny, so a single SQLite writer keeps the design
boring; rusqlite avoids compile-time query macros and a database server;
askama gives compile-checked server-rendered HTML while keeping markup plain.

## Data model

```sql
targets(id TEXT PK, name TEXT, url TEXT,
        expect_status INTEGER DEFAULT 200, interval_s INTEGER,
        timeout_s INTEGER, fail_threshold INTEGER DEFAULT 2,
        ok_threshold INTEGER DEFAULT 2, enabled INTEGER, sort INTEGER)

checks(id INTEGER PK, target_id TEXT, ts INTEGER, ok INTEGER,
       status_code INTEGER NULL, latency_ms INTEGER NULL,
       error_kind TEXT NULL, error_detail TEXT NULL)
  -- index (target_id, ts); raw retention 90 days

incidents(id INTEGER PK, target_id TEXT, started_at INTEGER,
          ended_at INTEGER NULL, cause TEXT, detail TEXT)
  -- unique partial index on (target_id) WHERE ended_at IS NULL

daily_stats(target_id TEXT, day TEXT, checks INTEGER, ok_checks INTEGER,
            down_seconds INTEGER, avg_latency_ms INTEGER,
            max_latency_ms INTEGER, PRIMARY KEY(target_id, day))
  -- derived; kept 2 years
```

Migrations: `PRAGMA user_version` plus an ordered list of SQL steps.

## Probe semantics

- GET, follow up to 5 redirects, 10 s timeout, gzip, no cookies, no body read
  (drop the response after headers), User-Agent
  `kaputt.matze.lol/0.1 (+https://kaputt.matze.lol/about)`.
- Success = final status equals the target's `expect_status` (default 200).
- Error kinds: `dns`, `connect`, `tls`, `timeout`, `status`,
  `too_many_redirects`, `other`.
- The geoportal's redirect target is unresolved from here; its `probe --once`
  output on the deployment host determines the configured URL/expected status.
- Hysteresis: DOWN after N consecutive failures, UP after M consecutive
  successes (default 2/2) to avoid flapping. An incident starts at the first
  failed check of the run and ends at the first successful check; the raw
  failed checks provide the exact windows.
- The incident state machine keeps no memory of its own: every check re-derives
  the trailing run and the open incident from the stored rows. A restart
  resumes the same counters, and `probe --once` moves the same state.
- Latency recorded on every check.

## Web UI (German, read-only)

- `/` — overall banner, one row per target: state, uptime for 24 h / 7 d / 90 d,
  90 daily bars, last check age, latency. HTMX polls `/partials/status` every
  30 s.
- `/t/{id}` — details: latency as inline SVG (server-rendered, no JS), incident
  list, recent checks with status codes, URL/interval.
- `/about` — what is checked, how often, from where, what "kaputt" means,
  disclaimer that this is not an official city service, privacy note (no
  cookies, no tracking).
- `/healthz` — liveness, no target requests.
- State is not colour-only: text + shape + `aria-live` on the status region.
- Whimsy: compact hand-written CSS, German microcopy ("Alles läuft",
  "Gerade kaputt", "seit 3 Minuten"), small hand-drawn SVG marks, no emoji
  spam, no AI-sounding filler. No external requests.
- No admin UI and no user reports in v1; targets come from `config.toml`.

## Deployment

- systemd unit, dedicated user, `StateDirectory=kaputt`, binary + config.
- Caddy (or existing reverse proxy) terminates TLS for kaputt.matze.lol.
- Nightly backup via a `kaputt backup <file>` subcommand (`VACUUM INTO`), file
  rotation; SQLite WAL checkpoint on shutdown.
- Retention job: delete raw checks older than 90 days after the daily rollup;
  prune daily stats older than 2 years.
- Prerequisite: run `probe --once` from the host and record the actual final
  URL/status of the geoportal check. If the host cannot reach the targets, or
  sees different results than this network, that is a vantage decision to make
  before accepting the deployment.

## Work order

Rule: tick a box only when its acceptance sentence holds. Update the boxes in
the same jj commit as the work they describe.

- [x] **1. Bootstrap** — `jj git init --colocate` (the directory is not a jj
  repo yet), cargo project, config parsing, schema/migrations, `/healthz`,
  `probe --once`. Acceptance: `cargo run` serves health, probe prints results,
  unit tests for config parsing.
- [x] **2. Probe + storage** — HTTP status check, redirect following, error
  classification, insert into `checks`; local test server cases for 200, 500,
  timeout, redirect chain. Acceptance: tests green; `probe --once` usable on
  the host.
- [x] **3. Scheduler + incidents** — per-target loop, default 300 s interval,
  jitter, hysteresis, incident transitions. Acceptance: simulated failure runs
  produce one incident with correct start/end and no duplicates; restart does
  not duplicate incidents.
- [ ] **4. Overview UI** — Askama templates, embedded CSS/HTMX, status partial,
  responsive layout. Acceptance: real browser check of live updates and
  narrow viewport.
- [ ] **5. Detail page** — latency SVG, incident history, recent checks.
  Acceptance: browser check of chart and incident list for a target with
  synthetic data.
- [ ] **6. Aggregates** — daily rollup job, uptime math, 90-day bars, retention
  (raw 90 days, rollups 2 years), Europe/Berlin day boundaries incl. DST.
  Acceptance: fixture day with known failures yields exact percentages.
- [ ] **7. Deploy** — hosting must be chosen first; then systemd, proxy, backup,
  `probe --once` from the host, run for 24 h and compare against manual curls.
  Acceptance: page reflects real checks; 24 h comparison matches.
- [ ] **8. Later, only if wanted** — alerting (mail/webhook), multi-vantage
  checks, user reports, more targets, content markers for status-200 error
  pages, public JSON/Atom feed.

## Implementation sessions

Not one session per work-order step. Boundaries follow working-binary
milestones and shared files, so a session never ends mid-refactor:

Work-order boxes are the source of truth. A session box below is ticked only
once all of its work-order boxes are ticked.

- [x] **Session 1 — steps 1–2** — bootstrap + probe/storage (same
  schema/config surface). Exit: `cargo test` green, `/healthz` serves,
  `probe --once` exercises a local stub target.
- [x] **Session 2 — step 3** — scheduler + incidents. Entry contract: probe
  outcome type, `Store::insert_check`, recent-check query. Exit: hysteresis
  and restart-without-duplicate-incident tests green.
- [ ] **Session 3 — steps 4–5** — overview + detail (same templates, CSS,
  layout). Exit: real browser smoke of live HTMX refresh and a 320 px
  viewport.
- [ ] **Session 4 — step 6** — aggregates, rollups, retention. Exit:
  fixture-day percentages exact, DST boundary test green.
- [ ] **Session 5 — step 7** — deploy, in its own session once hosting exists.
  Needs host access and is blocked on the vantage decision; keep it out of
  feature work.

Each session starts by reading `PLAN.md` and `jj log`, and ends with green
tests plus a descriptive jj commit message. No handoff file: code, tests and
history are the state.

## Assumptions

- Targets are exactly `www.karlsruhe.de`, `stadtbibliothek.karlsruhe.de`,
  `opac.karlsruhe.de`, `geoportal.karlsruhe.de` (roots).
- UI language is German; this is a private, unofficial project, so there is no
  imprint/legal review beyond the disclaimer.
- Outages may be partial: this network can lose all four targets while other
  networks still get 200. The page reports the probe's view and says where the
  probe runs; it does not claim global truth.
- Node/package tooling is not needed; assets are vendored.

## Remaining decisions

- Hosting (deployment step only; code is unaffected).
- Whether a 200-but-broken page should later be caught by content markers
  (explicitly deferred, would need a schema migration).
