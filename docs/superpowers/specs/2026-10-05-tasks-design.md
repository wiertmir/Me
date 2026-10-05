# Me — Tasks service: design

Date: 2026-10-05
Status: implemented

## Context

`auth-service`, `auth-web` and `calendar-service` exist. `tasks-service` is the
next executable of the suite: a REST API that stores each user's task lists and
tasks.

It has the same two consumers as calendar-service, neither of which exists yet:

- the Slint desktop client, the first and main consumer. One changes feed lets
  it keep a local copy up to date.
- the CalDAV bridge, which translates iCalendar `VTODO` from phones to this
  API.

The service is a sibling of calendar-service and follows it wherever this
document does not say otherwise.

## Goals

- Several task lists per user.
- Tasks with an optional due date or due time, a priority, reminders, and a
  completed state.
- Subtasks, one level deep.
- Recurring tasks: every occurrence is an ordinary task, created by the server
  on schedule.
- The same change tracking as calendar-service: an `etag` per task, a sync
  token per list, tombstones for deleted tasks.

## Out of scope for this version

- Sharing a list with another user.
- A start date, status values other than open and completed, percent complete,
  categories, attachments and other `VTODO` properties not listed under "Data
  model". The bridge drops them.
- Search, filtering and sorting on the server. The client holds a local copy.
- Sending reminder notifications. The service stores reminders; clients notify.
- Moving a task to another list (delete and create instead).
- Subtasks of subtasks.
- Editing all tasks of a recurring chain at once. Each task is independent once
  created.
- A background job. Scheduled tasks are created when a request arrives (see
  "Recurrence").
- A new OAuth scope, and removing a user's data when the account is removed —
  as in calendar-service.

## Layout

```
Me/
  Cargo.toml            workspace members gain tasks-service
  common/               gains what calendar-service and tasks-service share
  tasks-service/        Rust binary + library, own config and SQLite file
```

## Deployment shape

`tasks-service` listens on `127.0.0.1:8084`. The Caddyfile gains one rule:

| Path       | Goes to       |
|------------|---------------|
| `/tasks/*` | tasks-service |

Inside that rule Caddy removes the `X-Service-Secret` and `X-User-Id` request
headers, as it does for `/calendar/*`.

`/api/docs` and `/api/openapi.json` are served on the internal address. While
the project is in development the Caddyfile also publishes them, without
sign-in, as `/tasks/docs` and `/tasks/openapi.json`, as it does for
calendar-service.

## common

These move from `calendar-service` to `common` with unchanged behaviour, and
calendar-service uses them from there:

- `Caller`: the extractor that identifies the user from a bearer token or from
  `X-Service-Secret` + `X-User-Id`. It becomes generic over what it needs from
  the state (the verifier and the service secret).
- The date and time types and parsers of `time.rs`: `When`, `parse_when`,
  `parse_tz`, `to_utc`, and the year window.

The recurrence code stays in calendar-service. tasks-service needs one
function of its own (the next occurrence after a given time) and calls the
`rrule` crate directly; the rule check (parses, frequency daily to yearly) is
shared only if it turns out to be the same code.

## tasks-service

**Stack:** as calendar-service — tokio, axum, SQLite (rusqlite, bundled),
serde, tracing, utoipa, `rrule`, `chrono-tz`.

### Who is calling

As calendar-service: a bearer access token, or the service secret with
`X-User-Id`; a request with an `X-Service-Secret` header is judged by that
header alone. Every query is filtered by the caller's user id. A list or task
that belongs to someone else answers 404 `not_found`. The service keeps no user
table.

### Data model

`lists`

| Column       | Meaning |
|--------------|---------|
| `id`         | UUID |
| `user_id`    | Owner (auth-service user id) |
| `name`       | 1 to 100 characters |
| `color`      | `#rrggbb` |
| `sync_token` | Integer, starts at 0, increased by one on every task write in the list |
| `created_at`, `updated_at` | |

`tasks`

| Column          | Meaning |
|-----------------|---------|
| `id`            | UUID |
| `list_id`       | Deleting a list deletes its tasks |
| `uid`           | iCalendar UID. Given by the caller or, by default, the task id. Unique per list |
| `summary`       | Up to 500 characters; may be empty |
| `description`   | Up to 10,000 characters |
| `due`           | Null, a date, or a wall-clock time. See "Time" |
| `tz`            | IANA zone name when `due` is a wall-clock time; null otherwise |
| `priority`      | 0 to 9 as in iCalendar: 0 is none, 1 is highest |
| `completed_at`  | Instant at which the task was completed; null while it is open |
| `reminders`     | JSON list of minutes before `due`, each 0 to 40,320, at most 5. Must be empty when `due` is null |
| `parent_id`     | Set on a subtask: the task it belongs to |
| `rrule`         | One RFC 5545 rule without the `RRULE:` prefix, or null |
| `recurrence_id` | Set on every task of a recurring chain: the id of the chain's first task |
| `revision`      | The list's `sync_token` value at this task's last write |
| `deleted`       | Tombstone flag |
| `due_utc`       | Derived: the instant at which `due` has passed. Null when `due` is null |
| `created_at`, `updated_at` | |

A list holds at most 10,000 tasks that are not deleted. A user has at most 100
lists.

A new user has no rows. The first `GET /lists` for a user who has no list
creates one named "Tasks".

### Time

- **Due time:** a wall-clock time without offset (`2026-10-05T09:00:00`), read
  in the task's `tz`. It has passed at that instant. Skipped and repeated
  wall-clock times are resolved as in calendar-service.
- **Due date:** a date (`2026-10-05`); `tz` is null. It has passed when the
  date is before today's date in UTC, that is at 00:00 UTC of the following
  day.
- **Year window:** `due` is in the years 1900 to 2200; anything else is 422.

### Completion

The client sends `completed` (boolean). The server sets `completed_at` to now
when a task goes from open to completed, clears it when the task is reopened,
and leaves it unchanged otherwise. Completing a task has no other effect: it
does not touch subtasks and does not create anything.

### Subtasks

- A subtask has a `parent_id` naming a task of the same list that is not itself
  a subtask and is not deleted. Anything else is 422.
- A subtask cannot have an `rrule`.
- A task that has subtasks cannot be given a `parent_id`.
- `parent_id` is set at creation and cannot change.
- Deleting a parent deletes its subtasks (each leaves a tombstone).
- Completing or reopening a parent leaves its subtasks as they are.

### Recurrence

Every occurrence of a recurring task is an ordinary task row. There are no
series, cancelled occurrences or overrides.

- **Rule.** `rrule` must parse, and its frequency must be daily, weekly,
  monthly or yearly; anything else is 422. A task with a rule must have a
  `due`, and cannot be a subtask. The rule is read with the task's `due` as its
  start, in the task's `tz`.
- **Chain.** Giving a task a rule starts a chain: its `recurrence_id` becomes
  its own id. Only the newest task of a chain carries the rule; it is the
  chain's *head*.
- **Creating the next task.** When the head's `due` has passed, the server
  creates the next task and makes it the head:
  - the new task copies `summary`, `description`, `tz`, `priority`,
    `reminders`, `recurrence_id` and the rule; it is open, has a new `id` and
    `uid`, and its `due` is the rule's next occurrence after the old head's
    `due`;
  - the old head's `rrule` becomes null. It keeps its `recurrence_id` and is
    from then on a plain task;
  - the old head's subtasks that are not deleted are copied to the new task,
    open, with new ids and uids. A copied subtask keeps its own `due`, `tz`,
    `priority` and `reminders` unchanged.

  This repeats until the head's `due` has not passed, so each chain has exactly
  one task due in the future, and missed ones stay behind as overdue tasks.
  Whether the old head is completed makes no difference.
- **End of a chain.** A rule with `UNTIL` ends when it has no next occurrence;
  the last task keeps the rule, and nothing more is created. A rule with
  `COUNT=n` is copied as `COUNT=n-1`; a head with `COUNT=1` creates nothing.
- **Changing a chain.** The head is edited like any task. Its texts, `due` and
  rule shape every later task. Setting its `rrule` to null, or deleting it,
  stops the chain. Giving a rule to a task that has a `recurrence_id` but is
  not the head is 409 `conflict` while the chain has a head; when it has none
  any more, it starts a new chain from that task. This stops a client that
  writes back an old copy of a head, after the server has moved the rule on,
  from running the chain twice.
- **Long absence.** When more than 30 occurrences have been missed in one
  chain, only the 30 most recent are created, and the head after them.
- **Full list.** When the list holds 10,000 tasks, nothing is created; the
  chain continues at the next request that finds room.
- **When it runs.** There is no timer. Every `/tasks/v1/*` request first
  creates whatever is due in all lists of the caller, in one transaction, and
  then does its own work. Each created or changed task is a normal write: it
  raises the list's `sync_token` and gets that value as its `revision`. The
  service reads the time from a clock in its state, so tests can set it.

### Endpoints

All under `/tasks/v1`. Bodies are JSON.

| Route | What it does |
|---|---|
| `GET /lists` | The caller's lists, each with its `sync_token` |
| `POST /lists` | Create (`name`, `color`). At most 100 per user |
| `GET /lists/{id}` | Read |
| `PATCH /lists/{id}` | Change `name` and/or `color` |
| `DELETE /lists/{id}` | Delete the list and its tasks, for good |
| `GET /lists/{id}/changes?since=` | Tasks changed since a sync token |
| `POST /lists/{id}/tasks` | Create a task or a subtask |
| `GET /tasks/{id}` | Read one task |
| `PUT /tasks/{id}` | Replace it |
| `DELETE /tasks/{id}` | Delete it and its subtasks (leaves tombstones) |

Outside that prefix: `GET /health`, `GET /api/openapi.json`, `GET /api/docs`.

**Task** (what create, read, replace and the changes feed carry): the fields of
the data model except `due_utc`, with `completed` (boolean) beside
`completed_at`, plus `etag`. `PUT` takes the whole task; `list_id`, `uid` and
`parent_id` cannot change after creation; `completed_at` and `recurrence_id`
are set by the server only.

**Changes feed.** As calendar-service. Without `since`: every task of the list
that is not deleted and the list's current `sync_token`; this is also how a
client lists tasks. With `since`: every task whose `revision` is greater,
including tombstones (`id`, `uid`, `deleted: true`), and the current token. A
`since` greater than the current token is 410 `sync_token_invalid`.

**Concurrency.** As calendar-service: an `etag` per task, also sent as the
`ETag` header; `PUT` and `DELETE` accept `If-Match`; a stale one is 412
`etag_mismatch`.

### Errors

The shared `{code, message}` shape from `common`.

| Status | Code | When |
|---|---|---|
| 401 | `unauthorized` | No valid token or service secret |
| 404 | `not_found` | Unknown id, or someone else's |
| 409 | `conflict` | `uid` already used in the list; list limit reached; the list already holds 10,000 tasks; a rule given to a task whose chain already continues in a later task |
| 410 | `sync_token_invalid` | `since` is ahead of the list |
| 412 | `etag_mismatch` | Stale `If-Match` |
| 422 | `validation` | Malformed body, bad zone, rule, colour, priority or parent, length caps, a year outside 1900 to 2200, a rule without `due` or on a subtask, reminders without `due` |
| 503 | `unavailable` | Signing keys cannot be fetched |

### Logging, tracing, OpenAPI

As calendar-service, through `common`. Writes are logged at info with
`user_id`, `list_id` and `task_id`; a task created by a rule is logged with the
id of the task it follows. Task texts (summary, description) are never logged.
A test fails if a route is missing from the OpenAPI document.

### Configuration

One TOML file plus `ME_TASKS__<KEY>` overrides, with the keys of
calendar-service:

| Key              | Meaning |
|------------------|---------|
| `listen`         | Address and port (`127.0.0.1:8084`) |
| `data_dir`       | Directory for `tasks.db` |
| `issuer`         | Expected `iss` of access tokens: auth-service's public URL |
| `jwks_url`       | Where to fetch the signing keys; default `{issuer}/.well-known/jwks.json` |
| `audience`       | Expected `aud`; default `me-api` |
| `service_secret` | Same rules as the other services |
| `[log]`          | `format`, `level`, `dir` |

### Database

SQLite in WAL mode, tables created at start-up, no migrations — the same
convention, and the same upgrade limitation, as the other services.

## Running it

- `4-run-local-tasks.sh` beside the existing scripts; Caddy's becomes
  `5-run-caddy.sh`, so the numbers stay the start order. With
  `tasks-service/config.example.toml`.
- `tasks-service/Dockerfile`; a further application container in
  `k8s/me.yaml`; `run-kubernetes.sh` builds and loads its image.
- A `tasks-service` resource in the Aspire app host; Caddy waits for it.
- README: the project table, configuration, the Caddy table, backup
  (`tasks.db`), logging, and the checks.

## Testing

Integration tests start the service in-process on a temporary database and use
the real HTTP API, with the token and keys stub of calendar-service's tests and
a clock the test sets.

- Lists: default list on first use, create, rename, delete, the limit.
- Both ways of authenticating; a wrong secret; no fallback from a bad secret to
  a token.
- Isolation: one user cannot read, change or list into another's list or task.
- Tasks: create, read, replace, delete; due as null, date and time; validation
  of zone, lengths, priority, reminders; completing stamps `completed_at`,
  reopening clears it, an unrelated edit keeps it.
- Subtasks: create; refused under a subtask, in another list, under a deleted
  task, with a rule; deleting a parent deletes them; completing a parent does
  not.
- Recurrence: nothing is created before the head's due has passed; one task
  after it has; several after a longer gap, each overdue but the last; the 30
  cap; a daily time rule across a daylight-saving change keeps its wall-clock
  time; a date rule; `UNTIL` and `COUNT` end the chain; removing the rule or
  deleting the head stops it; subtasks are copied open; a completed head still
  produces the next; created tasks appear in the changes feed; a full list
  creates nothing; refused frequencies; a rule without `due`; a stale write to
  an advanced head is refused.
- Changes feed: full listing, incremental listing, tombstones, a token from the
  future.
- `If-Match`: accepted, stale, absent.
- Every route is in the OpenAPI document.

calendar-service's existing tests keep passing after the move into `common`.

## Build order

1. Move `Caller` and the time types into `common`; calendar-service uses them
   from there.
2. `tasks-service` skeleton: config, database, health, logging, OpenAPI, both
   ways of authenticating, the clock.
3. Lists.
4. Tasks: create, read, replace, delete, completion, `etag`, `If-Match`.
5. Subtasks.
6. Changes feed.
7. Recurrence.
8. Scripts, Dockerfile, Kubernetes, Aspire, Caddyfile, README.
