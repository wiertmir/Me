# Me — Calendar service: design

Date: 2026-10-04
Status: implemented

## Context

`auth-service` and `auth-web` exist. `calendar-service` is the next executable
of the suite: a REST API that stores each user's calendars and events.

It has two consumers, neither of which exists yet:

- the Slint desktop client, built after this service. It is the first and main
  consumer, so the API is shaped for it: the server expands recurrence, and one
  changes feed lets the client keep a local copy up to date.
- the CalDAV bridge, built later, which translates iCalendar from phones to
  this API.

## Goals

- Several calendars per user.
- Timed events with an IANA time zone, and all-day events.
- Recurring events (RFC 5545 rules), cancelled occurrences and changed single
  occurrences, expanded by the server for a date range.
- Reminders stored on the event.
- Change tracking good enough for both consumers to synchronise: an `etag` per
  event, a sync token per calendar, tombstones for deleted events.

## Out of scope for this version

- Sharing a calendar with another user.
- Attendees, invitations, free/busy.
- Search, and import or export of `.ics` files.
- Sending reminder notifications. The service stores reminders; clients notify.
- Moving an event to another calendar (delete and create instead).
- Event status, transparency, categories, attachments and other iCalendar
  properties not listed under "Data model". The bridge drops them. If that
  turns out to matter, an opaque passthrough field is added then.
- A new OAuth scope. Every client is our own, so any valid access token is
  accepted.
- Removing a user's data when the account is removed. auth-service only
  disables accounts.

## Layout

```
Me/
  Cargo.toml            workspace members: common, auth-service, calendar-service
  common/               gains what a second service now needs (see "common")
  calendar-service/     Rust binary + library, own config and SQLite file
```

## Deployment shape

`calendar-service` listens on `127.0.0.1:8083`. The Caddyfile gains one rule:

| Path          | Goes to          |
|---------------|------------------|
| `/calendar/*` | calendar-service |

Inside that rule Caddy removes the `X-Service-Secret` and `X-User-Id` request
headers, so only a caller on the internal network can use the service-secret
way in (see "Who is calling").

`/api/docs` and `/api/openapi.json` are served by calendar-service on its
internal address; the Caddyfile already answers `/api/*` with 404. While the
project is in development the Caddyfile also publishes them, without sign-in,
as `/calendar/docs` and `/calendar/openapi.json` (added 2026-10-04 at the
owner's request, to read the documentation from other machines).

## common

The auth spec said "nothing else until a second service needs it". It does now.
These move from `auth-service` to `common` with unchanged behaviour, and
auth-service uses them from there:

- `logging`: console logging (pretty or JSON, local time), the request layer
  (span per request, `X-Request-Id`, route template, no query string) and the
  optional OTLP trace export. The service name becomes a parameter. `LogConfig`
  and `LogFormat` move with it.
- `ApiJson` and `PathId` extractors.
- The config loader: read a TOML file, apply `<PREFIX>__KEY` environment
  overrides. The prefix becomes a parameter.
- The service-secret rules: the constant-time header check, the minimum length
  of 16, and the refusal of the published example secret on a non-loopback
  listener.

One addition: `TokenVerifier` accepts an explicit keys URL. The `iss` claim is
the public origin, but a service on the same machine should fetch the keys from
auth-service's internal address rather than through the proxy and its
certificate.

## calendar-service

**Stack:** as auth-service — tokio, axum, SQLite (rusqlite, bundled), serde,
tracing, utoipa. New: `rrule` (recurrence expansion) and `chrono-tz` (IANA
zones).

### Who is calling

Every `/calendar/v1/*` request is made on behalf of one user, identified in one
of two ways:

- **Bearer access token** (desktop client). Verified through `common`: ES256,
  issuer, audience, expiry. The user is the `sub` claim.
- **Service secret** (CalDAV bridge). Headers `X-Service-Secret` and
  `X-User-Id` (a UUID). The bridge has already checked the user's app password
  with auth-service.

A request with an `X-Service-Secret` header is judged by that header alone: a
wrong secret or a missing or malformed `X-User-Id` is 401, with no fallback to
a bearer token. A request with neither is 401.

Every query is filtered by the caller's user id. A calendar or event that
belongs to someone else answers 404 `not_found`, the same as one that does not
exist.

The service keeps no user table. It trusts the user id it is given.

### Data model

`calendars`

| Column       | Meaning |
|--------------|---------|
| `id`         | UUID |
| `user_id`    | Owner (auth-service user id) |
| `name`       | 1 to 100 characters |
| `color`      | `#rrggbb` |
| `sync_token` | Integer, starts at 0, increased by one on every event write in the calendar |
| `created_at`, `updated_at` | |

`events`

| Column               | Meaning |
|----------------------|---------|
| `id`                 | UUID |
| `calendar_id`        | Deleting a calendar deletes its events |
| `uid`                | iCalendar UID. Given by the caller or, by default, the event id. Unique per calendar among series and single events; an override carries its series' uid |
| `summary`            | Up to 500 characters; may be empty |
| `description`        | Up to 10,000 characters |
| `location`           | Up to 500 characters |
| `all_day`            | Boolean |
| `start`, `end`       | See "Time" |
| `tz`                 | IANA zone name; null for all-day events |
| `rrule`              | One RFC 5545 rule without the `RRULE:` prefix, or null |
| `exdates`            | JSON list of cancelled occurrence starts, at most 1,000 |
| `reminders`          | JSON list of minutes before the start, each 0 to 40,320, at most 5 |
| `recurring_event_id` | Set on an override: the series it belongs to |
| `original_start`     | Set on an override: the start of the occurrence it replaces |
| `revision`           | The calendar's `sync_token` value at this event's last write |
| `deleted`            | Tombstone flag |
| `start_utc`, `end_utc` | Derived instants, for range queries. For a series, `end_utc` is null |
| `created_at`, `updated_at` | |

`(recurring_event_id, original_start)` is unique.

A calendar holds at most 10,000 events that are not deleted, of which at most
1,000 are series (events with an `rrule`). Together with the year window (see
"Time") this bounds what one range query can cost.

A new user has no rows. The first `GET /calendars` for a user who has no
calendar creates one named "Personal".

### Time

- **Timed event:** `start` and `end` are wall-clock times without offset
  (`2026-10-05T09:00:00`), read in the event's `tz`. `end` is after `start`.
  A weekly 09:00 event therefore stays at 09:00 across a daylight-saving
  change.
- **All-day event:** `start` and `end` are dates (`2026-10-05`); `end` is
  exclusive and after `start`; `tz` is null. The event belongs to those
  calendar days wherever the viewer is.
- **Wall-clock times that do not map to one instant:** a time skipped by a
  daylight-saving change is moved forward by the length of the gap; a time that
  occurs twice takes the earlier instant.
- `exdates` and `original_start` use the same form as the series' `start` and
  are read in the series' zone.
- **Year window:** every date and time a client sends — `start`, `end`,
  `exdates`, `original_start`, and `from` and `to` of a range query — is in
  the years 1900 to 2200; anything else is 422.

### Recurrence

- `rrule` must parse, and its frequency must be daily, weekly, monthly or
  yearly; anything else is 422. `RDATE` and `EXRULE` are not supported.
- One occurrence of a series is at most 366 days long (`end` minus `start`,
  as instants), and its `end` is after its `start` on the wall clock too.
- **Cancelled occurrence:** its start is added to the series' `exdates`.
- **Changed occurrence (override):** a separate event with
  `recurring_event_id` and `original_start`. It replaces that occurrence in
  range queries and may have any times, texts and reminders of its own. It
  cannot have an `rrule`, and its series must be in the same calendar.
- **"This and following":** not a server feature. The client ends the old
  series (an `UNTIL` in its rule) and creates a new one.
- When a series is edited so that an `exdates` entry or an override no longer
  matches one of its occurrences, that entry or override is kept but has no
  effect: it does not appear in range queries. Nothing is deleted implicitly.
- Deleting a series deletes its overrides.

### Endpoints

All under `/calendar/v1`. Bodies are JSON.

| Route | What it does |
|---|---|
| `GET /calendars` | The caller's calendars, each with its `sync_token` |
| `POST /calendars` | Create (`name`, `color`). At most 100 per user |
| `GET /calendars/{id}` | Read |
| `PATCH /calendars/{id}` | Change `name` and/or `color` |
| `DELETE /calendars/{id}` | Delete the calendar and its events, for good |
| `GET /calendars/{id}/events?from=&to=&tz=` | Occurrences in the range |
| `GET /calendars/{id}/changes?since=` | Stored events changed since a sync token |
| `POST /calendars/{id}/events` | Create an event, a series or an override |
| `GET /events/{id}` | Read one stored event |
| `PUT /events/{id}` | Replace it |
| `DELETE /events/{id}` | Delete it (leaves a tombstone) |

Outside that prefix: `GET /health`, `GET /api/openapi.json`, `GET /api/docs`.

**Stored event** (what create, read, replace and the changes feed carry): the
fields of the data model except the derived instants, plus `etag`. `PUT` takes
the whole event; `calendar_id`, `uid`, `recurring_event_id` and
`original_start` cannot change after creation.

**Range query.** `from` and `to` are instants (RFC 3339 with offset), `to`
after `from`, at most 366 days apart. `tz` (IANA name, default `UTC`) is the
zone in which all-day dates are placed on the timeline. The answer is a list of
occurrences that overlap the range, ordered by start: single events, each
occurrence of each series minus `exdates`, with overrides in place of the
occurrences they replace. A timed occurrence keeps the series' wall-clock
length: it ends at its wall-clock start plus `end` minus `start` of the series,
read in the event's zone, so a 22:00 to 06:00 series ends at 06:00 on a
daylight-saving night too. An occurrence is the stored event with:

- `start` and `end` set to that occurrence's times,
- `start_utc` and `end_utc`, the same as instants,
- `original_start`, the scheduled start of that occurrence — the value the
  client sends back to cancel or override it (null for a single event),
- `id`, the stored event to read or write: the series, or the override.

More than 5,000 occurrences in one answer is 422 `too_many_occurrences`; the
client asks for a shorter range.

**Changes feed.** Without `since`: every stored event of the calendar that is
not deleted (series and overrides as stored, not expanded) and the calendar's
current `sync_token`. With `since`: every event whose `revision` is greater,
including tombstones (`id`, `uid`, `deleted: true`), and the current token. A
`since` greater than the current token is 410 `sync_token_invalid`; the client
starts again without it. The desktop client compares the `sync_token` values
from `GET /calendars` with the ones it holds and asks for changes only where
they differ.

**Concurrency.** Every stored event has an `etag`, also sent as the `ETag`
header, which changes on every write. `PUT` and `DELETE` accept `If-Match`;
when it does not equal the current `etag` the answer is 412 `etag_mismatch` and
nothing is written. Without `If-Match` the write goes through.

### Errors

The shared `{code, message}` shape from `common`.

| Status | Code | When |
|---|---|---|
| 401 | `unauthorized` | No valid token or service secret |
| 404 | `not_found` | Unknown id, or someone else's |
| 409 | `conflict` | `uid` already used in the calendar; an override for that occurrence already exists; calendar limit reached; the calendar already holds 10,000 events or 1,000 series |
| 410 | `sync_token_invalid` | `since` is ahead of the calendar |
| 412 | `etag_mismatch` | Stale `If-Match` |
| 422 | `validation` | Malformed body, bad zone, rule, colour or time order, length caps, range too long, a year outside 1900 to 2200, a series occurrence longer than 366 days |
| 422 | `too_many_occurrences` | Range query over the cap |
| 503 | `unavailable` | Signing keys cannot be fetched |

### Logging and tracing

As auth-service, through `common`: a span per request with method, route
template, status, duration and request id; OTLP export when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. Writes are logged at info with `user_id`,
`calendar_id` and `event_id`. Event texts (summary, description, location) are
never logged.

### OpenAPI

Generated from the code with `utoipa`, served at `/api/openapi.json` and
`/api/docs`, with both security schemes. A test fails if a route is missing
from the document.

### Configuration

One TOML file plus `ME_CALENDAR__<KEY>` overrides.

| Key              | Meaning |
|------------------|---------|
| `listen`         | Address and port (`127.0.0.1:8083`) |
| `data_dir`       | Directory for `calendar.db` |
| `issuer`         | Expected `iss` of access tokens: auth-service's public URL |
| `jwks_url`       | Where to fetch the signing keys; default `{issuer}/.well-known/jwks.json` |
| `audience`       | Expected `aud`; default `me-api` |
| `service_secret` | Same rules as auth-service: at least 16 characters, the example value refused off loopback |
| `[log]`          | `format`, `level` |

### Database

SQLite in WAL mode, tables created at start-up, no migrations — the same
convention, and the same upgrade limitation, as auth-service.

## Running it

- `3-run-local-calendar.sh` beside the existing scripts (Caddy's becomes
  `4-run-caddy.sh`, so the numbers are the start order), with
  `calendar-service/config.example.toml`.
- `calendar-service/Dockerfile`; a fourth application container in
  `k8s/me.yaml`; `run-kubernetes.sh` builds and loads its image.
- A `calendar-service` resource in the Aspire app host; Caddy waits for it.
- README: the project table, configuration, the Caddy table, backup
  (`calendar.db`), and the checks.

## Testing

Integration tests start the service in-process on a temporary database and use
the real HTTP API. Access tokens are signed with a test key whose public half
is served by a stub keys endpoint.

- Calendars: default calendar on first use, create, rename, delete, the limit.
- Both ways of authenticating; a wrong secret; no fallback from a bad secret to
  a token.
- Isolation: one user cannot read, change or list into another's calendar or
  event.
- Events: create, read, replace, delete; validation of zone, time order,
  lengths, reminders.
- Recurrence: a weekly series across a daylight-saving change keeps its
  wall-clock time; all-day series; `exdates`; an override replaces its
  occurrence and can move it out of or into a range; a rule with `UNTIL` and
  one with `COUNT`; refused frequencies; the 366-day and 5,000-occurrence caps;
  a nightly series across a daylight-saving change keeps its wall-clock end.
- Limits: years outside 1900 to 2200, a series occurrence over 366 days, the
  event and series caps per calendar.
- Changes feed: full listing, incremental listing, tombstones, a token from the
  future.
- `If-Match`: accepted, stale, absent.
- Every route is in the OpenAPI document.

auth-service's existing tests keep passing after the move into `common`.

## Build order

1. Move the shared pieces into `common`; auth-service uses them from there.
2. `calendar-service` skeleton: config, database, health, logging, OpenAPI,
   both ways of authenticating.
3. Calendars.
4. Single events, `etag`, `If-Match`.
5. Range query with recurrence, `exdates`, overrides.
6. Changes feed.
7. Scripts, Dockerfile, Kubernetes, Aspire, Caddyfile, README.
