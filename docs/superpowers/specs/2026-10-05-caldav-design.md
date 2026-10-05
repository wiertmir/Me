# Me — CalDAV bridge: design

Date: 2026-10-05
Status: approved, not yet implemented

## Context

`auth-service`, `auth-web`, `calendar-service` and `tasks-service` exist.
`caldav-bridge` is the next executable: it lets a CalDAV client (Thunderbird
first, phones later) read and write the calendars and task lists that the two
services hold.

It is built before the desktop client so that the services can be used and
tested with an existing program.

## Goals

- A CalDAV client configured with a host, a username and an app password finds
  the user's calendars and task lists by itself.
- Events and to-dos can be read, created, changed and deleted from the client,
  and changes made elsewhere arrive at the next synchronisation.
- Recurring events, cancelled occurrences and changed single occurrences
  survive the round trip.
- The bridge holds no data. Everything is in the two services.

## Out of scope for this version

- Creating, renaming, recolouring or deleting calendars and task lists from a
  CalDAV client (`MKCALENDAR`, `PROPPATCH`, `DELETE` of a collection).
- Invitations and scheduling (iTIP), free/busy.
- The incremental `sync-collection` report. Clients compare the collection's
  change marker and the items' etags instead.
- `VTIMEZONE` components in what the bridge sends. Zones are named by their
  IANA name only.
- Filters in `calendar-query`. The bridge answers with every item of the
  collection.
- `VJOURNAL`, attachments, `MOVE`, `COPY`, CardDAV.
- Showing or editing a to-do's repeat rule over CalDAV.
- Testing with phones. The bridge follows the standards, so they are expected
  to work; only Thunderbird is checked.
- The third item of issue #25 (naming the provider in auth-web's
  link-confirmation panel). It is unrelated to the bridge.

## Layout

```
Me/
  Cargo.toml            workspace members gain caldav-bridge
  caldav-bridge/        Rust binary + library, own config, no database
  calendar-service/     gains a lookup by uid
  tasks-service/        gains a lookup by uid
  auth-service/         verify endpoint: rate limit by address only
```

## Deployment shape

`caldav-bridge` listens on `127.0.0.1:8085`. The Caddyfile gains two rules:

| Path                  | Goes to |
|-----------------------|---------|
| `/.well-known/caldav` | redirect (301) to `/dav/`; listed before the `/.well-known/*` rule of auth-service |
| `/dav/*`              | caldav-bridge |

Caddy sets `X-Forwarded-For`; the bridge passes the client's address on to
auth-service (see "Who is calling").

The bridge has no `/api/*` routes and no OpenAPI document: its interface is
CalDAV. It serves `GET /health`.

## Changes to the existing services

**calendar-service:** `GET /calendar/v1/calendars/{id}/by-uid?uid=` — the
stored events of the calendar that carry that uid and are not deleted: a single
event, or a series followed by its overrides. An empty list when there is none.

**tasks-service:** `GET /tasks/v1/lists/{id}/by-uid?uid=` — the same for tasks:
a list with one task, or empty.

Both are documented in OpenAPI and follow the services' rules for who is
calling and for other users' data (404).

**auth-service** (`POST /api/app-passwords/verify`, from issue #25):

- The per-username limit is removed, so that someone who knows a username
  cannot stall that user's devices by sending wrong passwords. The per-address
  limit stays.
- The OpenAPI text says that the caller must send the device's address in
  `X-Forwarded-For`; without it every device shares one limit.

## caldav-bridge

**Stack:** tokio, axum, reqwest, serde, tracing — as the other services,
through `common` for configuration, logging and the error type of its JSON
calls. New: `roxmltree` (reading the XML of requests) and `icalendar` (parsing
and writing iCalendar). XML answers are written by hand with one escaping
function.

### Who is calling

Every `/dav/*` request carries HTTP Basic credentials: a username (or email)
and an app password. The bridge sends them to auth-service's verify endpoint
with the service secret and the client's address in `X-Forwarded-For`, and gets
the user id and username. Nothing is cached: a deleted app password stops
working at the next request.

- No or malformed credentials, or a 401 from auth-service: 401 with
  `WWW-Authenticate: Basic realm="Me"`.
- A 429 from auth-service: 429 with its `Retry-After`.
- auth-service unreachable: 503.

The bridge then calls calendar-service and tasks-service with their service
secrets and `X-User-Id`. A path that names another user's name answers 404.

### Collections and addresses

```
/dav/                                  entry point
/dav/principals/{username}/            the user
/dav/calendars/{username}/             the user's collections
/dav/calendars/{username}/c-{id}/      a calendar  (VEVENT)
/dav/calendars/{username}/t-{id}/      a task list (VTODO)
/dav/calendars/{username}/c-{id}/{uid}.ics
```

`{username}` is the user's username as auth-service returns it, also when the
client signed in with an email address. `{id}` is the calendar's or list's UUID. An item's
name is its iCalendar UID, percent-encoded, plus `.ics`.

A `PUT` to a name that is not the UID of the item in the body plus `.ics` is
refused (403, `CALDAV:valid-calendar-object-resource`). Clients name new items
that way; it lets the bridge find an item without storing anything.

### Methods

| Method | On | What it does |
|---|---|---|
| `OPTIONS` | anything | `DAV: 1, calendar-access`, and the allowed methods |
| `PROPFIND` | any path, depth 0 or 1 | The properties below; depth 1 on a collection lists its items with their etags |
| `REPORT` `calendar-multiget` | a collection | The named items with their data |
| `REPORT` `calendar-query` | a collection | Every item of the collection with the requested properties |
| `GET` | an item | The iCalendar text, with `ETag` |
| `PUT` | an item | Create or replace; honours `If-Match` and `If-None-Match: *` |
| `DELETE` | an item | Delete; honours `If-Match` |

Anything else is 405. `PROPFIND` with depth `infinity` is 403. Request bodies
are limited to 1 MB.

Properties answered (others are reported as not found):

| Where | Properties |
|---|---|
| everywhere | `resourcetype`, `current-user-principal`, `displayname` |
| principal | `calendar-home-set` |
| calendar or task list | `supported-calendar-component-set` (`VEVENT` or `VTODO`), `getctag` (the service's `sync_token`), `calendar-color` (Apple's, from the colour), `supported-report-set`, `current-user-privilege-set` (read and write), `owner` |
| item | `getetag`, `getcontenttype`, and `calendar-data` in reports |

### Events

One item is everything stored under one uid in one calendar: a single event, or
a series with its overrides, as one `VCALENDAR`.

| Stored | iCalendar |
|---|---|
| `uid` | `UID` |
| `summary`, `description`, `location` | `SUMMARY`, `DESCRIPTION`, `LOCATION` |
| all-day `start`, `end` | `DTSTART;VALUE=DATE`, `DTEND;VALUE=DATE` |
| timed `start`, `end`, `tz` | `DTSTART;TZID=…`, `DTEND;TZID=…`, local time |
| `rrule` | `RRULE` |
| `exdates` | `EXDATE`, in the form of `DTSTART` |
| `reminders` | one `VALARM` each: `ACTION:DISPLAY`, `TRIGGER:-PT{n}M` |
| an override's `original_start` | `RECURRENCE-ID`, in the form of the series' `DTSTART` |
| `updated_at` | `DTSTAMP`, `LAST-MODIFIED` |

Reading what a client sends:

- `DTSTART` and `DTEND` with a `Z` are stored with `tz` `UTC`.
- A `DURATION` instead of `DTEND` is added to the start. An all-day event
  without either lasts one day.
- A `VALARM` with a `TRIGGER` relative to the start, zero or negative, in whole
  minutes, becomes a reminder; any other alarm is dropped.
- Dropped without an error: attendees, organiser, categories, status,
  transparency, class, sequence, `X-` properties, `VTIMEZONE` components, and
  every other property not in the table.
- Refused, because dropping them would change when the event happens: a `TZID`
  that is not an IANA zone name; a time without `Z` and without `TZID`
  (floating); `RDATE`; more than one `RRULE`; an `EXRULE`.

**Writing an item** is several calls to calendar-service:

1. Look the uid up.
2. Create or replace the single event or series.
3. For each override in the body: create it, or replace it when one with that
   `RECURRENCE-ID` is stored. Delete stored overrides the body no longer has.

This is not atomic. When a call fails, the bridge stops and answers with that
error; what was written stays, and the client, which did not get a new etag,
sends the item again.

**Etag of an item:** the highest revision among the stored events it is made
of, and their number. Every write takes a new, higher revision and a removed
part changes the number, so it changes when any of them does.

**Deleting an item** deletes the single event or the series (the service
deletes its overrides).

### To-dos

One item is one task.

| Stored | iCalendar |
|---|---|
| `uid` | `UID` |
| `summary`, `description` | `SUMMARY`, `DESCRIPTION` |
| date `due` | `DUE;VALUE=DATE` |
| timed `due`, `tz` | `DUE;TZID=…`, local time |
| `priority` | `PRIORITY` |
| open / completed | `STATUS:NEEDS-ACTION` / `STATUS:COMPLETED`, with `PERCENT-COMPLETE:100` |
| `completed_at` | `COMPLETED` |
| `reminders` | `VALARM`, as for events, relative to `DUE` |
| `parent_id` | `RELATED-TO` with the parent's uid |
| `updated_at` | `DTSTAMP`, `LAST-MODIFIED` |

Reading what a client sends:

- A to-do is completed when it has `STATUS:COMPLETED`, a `COMPLETED` time or
  `PERCENT-COMPLETE:100`. The time the service records is its own.
- Any other status is stored as open. `DTSTART` and the properties dropped for
  events are dropped here too.
- The same refusals as for events apply to `DUE`.

**Repeat rules** are never sent to the client: every task appears as a plain
to-do, and the next one of a series appears at the synchronisation after the
server has created it. A `RRULE` in a to-do the client creates is stored with
the task. When the client changes an existing to-do, the bridge sends the
stored rule back to the service unchanged, whatever the body says.

**Subtasks.** `RELATED-TO` in a to-do the client creates makes it a subtask
when a task with that uid is in the same list and the service accepts it;
otherwise the to-do is created without a parent. On a change the stored parent
is kept, whatever the body says.

### Errors

| From | Answer to the client |
|---|---|
| Body that is not one iCalendar object of the collection's kind, or one of the refusals above | 403 with `CALDAV:valid-calendar-data` or `CALDAV:supported-calendar-data`, and the reason as text |
| Service 404 | 404 |
| Service 409 | 409 |
| Service 412, or `If-None-Match: *` on an existing item | 412 |
| Service 422 | 403 with `CALDAV:valid-calendar-data` and the service's message |
| Service unreachable or 5xx | 502 |

### Logging

As the other services, through `common`: one line group per request with the
method, the path with ids but without the item name, status and duration.
Writes are logged at info with `user_id` and the calendar or list id. The
`Authorization` header, item names (they are uids chosen by the user's program)
and item bodies are never logged.

### Configuration

One TOML file plus `ME_CALDAV__<KEY>` overrides.

| Key               | Meaning |
|-------------------|---------|
| `listen`          | Address and port (`127.0.0.1:8085`) |
| `auth_url`        | auth-service's internal address (`http://127.0.0.1:8081`) |
| `calendar_url`    | calendar-service's internal address (`http://127.0.0.1:8083`) |
| `tasks_url`       | tasks-service's internal address (`http://127.0.0.1:8084`) |
| `auth_secret`     | auth-service's service secret |
| `calendar_secret` | calendar-service's service secret |
| `tasks_secret`    | tasks-service's service secret |
| `[log]`           | `format`, `level`, `dir` |

The run script takes the three secrets from the variables `.env` already has
(`ME_AUTH__SERVICE_SECRET`, `ME_CALENDAR__SERVICE_SECRET`,
`ME_TASKS__SERVICE_SECRET`), so nothing new is added to `.env`.

## Running it

- `5-run-local-caldav.sh` beside the existing scripts; Caddy's becomes
  `6-run-caddy.sh`.
- `caldav-bridge/Dockerfile`; a further container in `k8s/me.yaml`;
  `run-kubernetes.sh` builds and loads its image.
- A `caldav-bridge` resource in the Aspire app host, waiting for the three
  services; Caddy waits for it.
- README: the project table, configuration, the Caddy table, logging, and a
  section "Connecting Thunderbird" with the steps to add the account and what
  to check.

## Testing

Integration tests start calendar-service and tasks-service in-process on
temporary databases, a stub of the verify endpoint, and the bridge in front of
them, and send the requests a CalDAV client sends.

- Sign-in: no credentials, wrong ones, a rate-limited answer, another user's
  path.
- Discovery: `/.well-known/caldav` is Caddy's; from `/dav/` to the principal,
  the home and the list of collections with their kinds, names and colours.
- Listing a collection; multiget; query.
- Events: create, read, change and delete a single event, an all-day event and
  a series; a series with a cancelled occurrence; a series with a changed
  occurrence, then that occurrence changed again and removed; `If-Match` and
  `If-None-Match: *`; the etag changes when an override changes.
- To-dos: create, complete, reopen, delete; a subtask; a rule sent on create is
  stored and never sent back; a change keeps the rule and the parent.
- What is dropped stays dropped and what is refused is refused, each with one
  example.
- A change made through the service's own API shows as a new change marker and
  a new etag.
- The two lookups by uid, in their own services' tests; the verify endpoint
  without the per-username limit.

The check with Thunderbird is done by hand, following the README.

## Build order

1. auth-service: verify endpoint.
2. calendar-service and tasks-service: lookup by uid.
3. `caldav-bridge` skeleton: config, health, logging, sign-in.
4. Discovery and listing (`OPTIONS`, `PROPFIND`).
5. To-dos: iCalendar mapping, `GET`, `PUT`, `DELETE`, reports.
6. Events: single and all-day.
7. Events: series, cancelled and changed occurrences.
8. Scripts, Dockerfile, Kubernetes, Aspire, Caddyfile, README.
