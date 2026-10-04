# Me

A personal suite of self-hosted services with a desktop client.

| Project            | What it is                                              | Tech                  | Status   |
|--------------------|---------------------------------------------------------|-----------------------|----------|
| `auth-service`     | Users, sign-in, OAuth 2 / OpenID Connect tokens          | Rust                  | working  |
| `auth-web`         | Web app for sign-in, sign-up and account management      | .NET Blazor Server    | working  |
| `calendar-service` | Calendar REST API                                        | Rust                  | working  |
| `tasks-service`    | Tasks REST API                                           | Rust                  | planned  |
| `caldav-bridge`    | CalDAV front-end over the calendar and tasks services    | Rust                  | planned  |
| `client`           | Desktop app: calendar, tasks, Gmail/Hotmail/Yahoo mail   | Rust + Slint          | planned  |

Design documents live in [`docs/superpowers/specs`](docs/superpowers/specs).

## What is here

- `auth-service/` — the identity service. It owns users, passwords, sessions, app passwords, social
  sign-in and the OAuth 2 / OpenID Connect endpoints, and stores everything in SQLite under its data
  directory.
- `auth-web/` — the web interface: sign-in, sign-up, email verification, password reset, the account
  pages (profile, security, app passwords) and the admin page for users. It holds no data of its own
  and calls `auth-service` for everything.
- `calendar-service/` — the calendar API: calendars and events (with recurrence) per user, a range
  query that expands recurring events, and a changes feed. It stores everything in SQLite under its
  data directory and accepts the access tokens of `auth-service`.
- `common/` — Rust code shared with later services (API errors, token verification).
- `auth-web.Tests/` — component tests for `auth-web`.
- `scripts/` — end-to-end checks.
- `Caddyfile` — reverse-proxy configuration that puts the processes on one origin.

## Prerequisites

- Rust (stable, with `cargo`)
- .NET 10 SDK
- Only for `scripts/browser-check.sh`: Node.js and Google Chrome
- Only for `scripts/smoke-auth-web.sh`: `curl`, `openssl`, `python3`

## Quick start (development)

Run each command from the repository root, in its own terminal:

```sh
cargo run -p auth-service -- auth-service/config.example.toml
dotnet run --project auth-web
cargo run -p calendar-service -- calendar-service/config.example.toml   # optional
```

- `auth-web` (open this one): <http://localhost:5080>
- `auth-service`: <http://localhost:8081> (API documentation at <http://localhost:8081/api/docs>)
- `calendar-service`: <http://localhost:8083> (API documentation at <http://localhost:8083/api/docs>)

On its first start with an empty data directory the service creates the admin user `wiertmir` and
writes a warning line to its console that contains `one_time_password=…`. Sign in with that
username and password; the first sign-in forces you to choose a new password. The line is printed
only once. If you lose it, see [Locked out](#locked-out).

The service keeps its database and signing key in `./data` (relative to where you start it);
`auth-web` keeps its cookie-encryption keys in `auth-web/data/dp-keys`.

## Configuration

### auth-service

The service reads the TOML file named as its first argument (`config.toml` in the current directory
when none is given). [`auth-service/config.example.toml`](auth-service/config.example.toml) is a
working development configuration; copy it to `config.local.toml` (git-ignored) for real use.

| Key              | Meaning                                                                              |
|------------------|--------------------------------------------------------------------------------------|
| `issuer`         | Public URL of the service, as browsers and OAuth clients reach it                    |
| `web_url`        | Public URL of `auth-web`; the service redirects browsers there                       |
| `listen`         | Address and port to listen on                                                        |
| `data_dir`       | Directory for the database and the signing key                                       |
| `service_secret` | Secret that `auth-web` sends on every API call; at least 16 characters. The value in the example file is refused unless `listen` is a loopback address |
| `signup`         | `"open"` (anyone can create an account) or `"disabled"` (only an admin creates users) |
| `seed_username`  | Name of the admin created on the first start                                         |
| `seed_email`     | Email address of that admin; default `<seed_username>@localhost`. Read only when the admin is created; it cannot be changed later |
| `audience`       | The `aud` claim of access tokens; default `me-api`. Services that accept the tokens must expect the same value |
| `[log]`          | `format` (`"pretty"` or `"json"`) and `level`                                        |
| `[[clients]]`    | OAuth clients: `id`, `name`, `redirect_uris`                                         |
| `[smtp]`         | Outgoing mail, see below                                                             |
| `[providers.*]`  | Social sign-in, see below                                                            |

Any value can be overridden by an environment variable named `ME_AUTH__<KEY>`, with `__` for a
nested table: `ME_AUTH__SERVICE_SECRET`, `ME_AUTH__DATA_DIR`, `ME_AUTH__LOG__FORMAT=json`.
Overrides are read as text, so numbers (such as `smtp.port`) have to be set in the file.

**Mail.** Without an `[smtp]` section no mail is sent: a new account can sign in at once, its email
address stays "not verified" (nothing can prove it), and only an admin can reset a password. With
it, sign-up sends a verification link that must be followed before the first sign-in, and "Forgot
password" sends a reset link. Accounts created before mail was switched on are asked to verify
their address at their next sign-in.

Set `seed_email` to a real address before the first start if you want "Forgot password" to work
for the admin. With the default (`<seed_username>@localhost`) no mail can reach that account.

```toml
[smtp]
host = "smtp.example.com"
port = 587
from = "Me <no-reply@example.com>"
tls = "starttls"      # or "implicit" (usually port 465)
username = "…"
password = "…"
```

`tls = "none"` sends mail unencrypted. It exists for a local mail catcher during testing only.

### calendar-service

The service reads the TOML file named as its first argument, like `auth-service`.
[`calendar-service/config.example.toml`](calendar-service/config.example.toml) is a working
development configuration; copy it to `config.local.toml` (git-ignored) for real use.

| Key              | Meaning                                                                              |
|------------------|--------------------------------------------------------------------------------------|
| `listen`         | Address and port to listen on; `127.0.0.1:8083`                                      |
| `data_dir`       | Directory for the database (`calendar.db`)                                           |
| `issuer`         | The `iss` claim of the access tokens it accepts: the public URL of `auth-service`    |
| `jwks_url`       | Where to fetch the signing keys; default `{issuer}/.well-known/jwks.json`            |
| `audience`       | The `aud` claim that access tokens must carry; default `me-api`. Must equal `audience` of `auth-service` |
| `service_secret` | Secret of internal callers; at least 16 characters. The value in the example file is refused unless `listen` is a loopback address |
| `[log]`          | `format` (`"pretty"` or `"json"`) and `level`                                        |

Any value can be overridden by an environment variable named `ME_CALENDAR__<KEY>`, with `__` for a
nested table: `ME_CALENDAR__SERVICE_SECRET`, `ME_CALENDAR__DATA_DIR`,
`ME_CALENDAR__LOG__FORMAT=json`.

The API is under `/calendar/v1`. A caller sends either a bearer access token from `auth-service`, or
(internal callers only, such as the planned CalDAV bridge) the headers `X-Service-Secret` and
`X-User-Id`. The service secret may be the same value as `auth-service`'s or a different one; only
internal callers use it. Caddy removes those two headers from requests on the public route.

Limits: dates and times must be in the years 1900 to 2200, and one calendar holds at most 10,000
events, of which at most 1,000 are repeating ones.

For the home-network setup (the scripts, the app host and Kubernetes) do this once before the first
start of `calendar-service`. The scripts `1-run-local-auth.sh`, `2-run-local-auth-web.sh`,
`3-run-caddy.sh` and `4-run-local-calendar.sh` start the four processes of that setup, each in its
own terminal (`4-run-local-calendar.sh` runs `calendar-service`, like `1-run-local-auth.sh` runs
`auth-service`):

- add `ME_CALENDAR__SERVICE_SECRET=<a secret>` to `.env`;
- copy `calendar-service/config.example.toml` to `calendar-service/config.local.toml`, set `issuer`
  to the public origin (as for `auth-service`) and add
  `jwks_url = "http://127.0.0.1:8081/.well-known/jwks.json"`. The service then fetches the keys from
  `auth-service` directly, not through the proxy and its certificate. Keep `data_dir = "./data"`
  (the example's value), beside `auth.db`.

### auth-web

Settings are in [`auth-web/appsettings.json`](auth-web/appsettings.json); each can be set by an
environment variable instead (`:` becomes `__`).

| Key                     | Environment variable      | Meaning                                                          |
|-------------------------|---------------------------|------------------------------------------------------------------|
| `AuthService:BaseUrl`   | `AuthService__BaseUrl`    | Internal address of the service, for API calls                   |
| `AuthService:PublicUrl` | `AuthService__PublicUrl`  | Public URL of the service, for the "Continue with …" links       |
| `AuthService:Secret`    | `AuthService__Secret`     | The same value as the service's `service_secret`                 |
| `DataProtection:Path`   | `DataProtection__Path`    | Directory for the cookie-encryption keys; keep it across restarts |

`ASPNETCORE_URLS` sets the listen address and `ASPNETCORE_ENVIRONMENT` the environment.
`dotnet run --project auth-web` uses the development profile (`http://localhost:5080`,
`Development`), in which the secret comes from `appsettings.Development.json` and matches the
example service configuration.

Outside `Development`, `auth-web` refuses to start unless `AuthService:Secret` has at least 16
characters, marks its cookies `Secure` and expects every request to have arrived over HTTPS, so it
needs to sit behind a TLS-terminating proxy (below).

The service secret is the only thing that protects the service's `/api/*`. Use a long random value,
the same in both processes, and never the development one outside your own machine.

## Social sign-in

A provider is offered once it has a block in the service configuration:

```toml
[providers.google]      # or providers.github, providers.microsoft
client_id = "…"
client_secret = "…"
```

Register an OAuth application with the provider and give it the callback URL
`{issuer}/social/{provider}/callback`, for example `https://me.example.com/social/google/callback`.

| Provider  | Where to register                                                                  | Callback field                 |
|-----------|------------------------------------------------------------------------------------|--------------------------------|
| Google    | Google Cloud Console → APIs & Services → Credentials → OAuth client ID (Web application) | Authorised redirect URIs  |
| GitHub    | GitHub → Settings → Developer settings → OAuth Apps → New OAuth App                 | Authorization callback URL     |
| Microsoft | Microsoft Entra admin center → App registrations → New registration (accounts in any directory and personal accounts) | Redirect URI (Web) |

A first sign-in with a provider is attached to an existing account with the same email address only
when the provider confirms that the address is verified and the account's own address is verified
too (by the mailed link; an address typed at sign-up is never taken on trust, so without `[smtp]`
nothing is linked by email). Microsoft gives no such confirmation, so a Microsoft account is never
linked by email. In every other case, sign in with your password and link the provider on the
Security page.

An account created through a provider has no password. It can set one on the Security page
("Set a password"), after which it can also unlink the provider.

## Deployment behind Caddy

The [`Caddyfile`](Caddyfile) serves the processes on one origin and provides TLS:

| Path                                      | Goes to                          |
|-------------------------------------------|----------------------------------|
| `/social/complete`                        | auth-web (`127.0.0.1:5080`)      |
| `/oauth/*`, `/.well-known/*`, `/social/*` | auth-service (`127.0.0.1:8081`)  |
| `/calendar/*`                             | calendar-service (`127.0.0.1:8083`) |
| `/api/*`                                  | nothing: answered with 404       |
| everything else                           | auth-web (`127.0.0.1:5080`)      |

As written it uses `localhost`, which works for a local try-out with Caddy's own certificate;
replace it with your hostname for a real deployment. Start it with `caddy run` in the repository
root.

These settings must all be that single public origin (for example `https://me.example.com`):

- `issuer` and `web_url` in the service configuration
- `AuthService:PublicUrl` in `auth-web`

`AuthService:BaseUrl` stays the internal address (`http://127.0.0.1:8081`). `auth-web` learns the
client address and the HTTPS scheme from the proxy's `X-Forwarded-For` and `X-Forwarded-Proto`
headers, which it trusts only from a proxy on the same machine (loopback). Keep all the processes
listening on `127.0.0.1` so that nothing reaches them except through the proxy.

**Access logs.** The Caddyfile does not switch on Caddy's access log. If you enable one (in Caddy
or in any other proxy in front), it records full URLs, and some of them carry one-time tokens in
the query string: the email verification and password reset links (`/verify`, `/reset`), the
social sign-in and link tickets (`/social/complete`, `/account/security`, `/social/…/start`,
`/social/…/callback`) and the sign-in challenge (`/signin`). They are short-lived and single-use,
but treat such a log as sensitive, or leave the query string out of it.

## Running in production

Build once:

```sh
cargo build --release -p auth-service              # → target/release/auth-service
cargo build --release -p calendar-service          # → target/release/calendar-service
dotnet publish auth-web -c Release -o /opt/me/auth-web
openssl rand -base64 32                            # a service secret
```

Start `auth-service` with your own configuration file (a copy of the example with `issuer` and
`web_url` set to the public origin, `listen = "127.0.0.1:8081"` and an absolute `data_dir`):

```sh
ME_AUTH__SERVICE_SECRET='<the secret>' target/release/auth-service /etc/me/auth.toml
```

Start `auth-web` from its published directory:

```sh
cd /opt/me/auth-web
ASPNETCORE_ENVIRONMENT=Production \
ASPNETCORE_URLS=http://127.0.0.1:5080 \
AuthService__Secret='<the secret>' \
AuthService__PublicUrl=https://me.example.com \
DataProtection__Path=/var/lib/me/dp-keys \
dotnet AuthWeb.dll
```

Start `calendar-service` with its own configuration file (a copy of its example with
`listen = "127.0.0.1:8083"`, an absolute `data_dir`, `issuer` set to the public origin and
`jwks_url = "http://127.0.0.1:8081/.well-known/jwks.json"`, so that it fetches the signing keys from
`auth-service` directly and not through the proxy):

```sh
ME_CALENDAR__SERVICE_SECRET='<a secret>' target/release/calendar-service /etc/me/calendar.toml
```

- `AuthService__Secret` is required (at least 16 characters) and must equal `service_secret` of
  `auth-service`. Neither service accepts the example secret on a non-loopback address.
- `ME_CALENDAR__SERVICE_SECRET` is the secret of `calendar-service`'s internal callers (at least 16
  characters); it may be the same value or a different one.
- `DataProtection__Path` must be a directory that survives restarts and that only this process's
  user can read. The default (`./data/dp-keys`) is relative to the directory you start it in.
- All three processes listen on loopback (`127.0.0.1`) and speak plain HTTP; the proxy in front is
  the only thing that should be reachable from outside, and it provides TLS. In `Production`,
  `auth-web` serves only requests that the proxy reports as HTTPS (`X-Forwarded-Proto: https`);
  a plain-HTTP request sent straight to port 5080 fails.
- Set `LOG_FORMAT=json`, `ME_AUTH__LOG__FORMAT=json` and `ME_CALENDAR__LOG__FORMAT=json` if a log
  collector reads the output.

Run all three under a process supervisor of your choice so that they restart; none daemonises.

## Running under .NET Aspire

```sh
dotnet run --project apphost
```

runs the home-network setup, the same four processes as the `N-run-*.sh` scripts (`auth-service`,
`calendar-service`, `auth-web` in `Production`, Caddy), with the same `.env`,
`auth-service/config.local.toml`, `calendar-service/config.local.toml`, `Caddyfile` and data
directories. Stop the scripts or the Kubernetes cluster first; they use the same ports. The console
prints the login link of the Aspire dashboard (`http://localhost:15080/login?t=…`), which shows:

- Resources: the four processes with their state, start/stop/restart and console logs, and under
  Graph how they depend on each other (Caddy on all three, `auth-web` and `calendar-service` on
  `auth-service`);
- Traces: every request as one trace across `auth-web` and `auth-service`, and the requests to
  `calendar-service`.

`auth-service` is started with `cargo run --release`; the others wait until its `/health` answers.

Run `dotnet dev-certs https --trust` once on a new machine; without it the dashboard warns that no
trusted development certificate was found. On Linux the app host's launch profile points
`SSL_CERT_DIR` at the directory that command fills.

## Tracing

The services export OpenTelemetry traces over OTLP (gRPC) when the standard variable
`OTEL_EXPORTER_OTLP_ENDPOINT` is set, as the Aspire app host does; without it nothing is collected.
Any OTLP collector works: set the variable (and `OTEL_EXPORTER_OTLP_HEADERS` if it needs a key) for
every process.

- `auth-web` records a span per request and per call to `auth-service`, and sends the trace context
  along, so the service's span joins the same trace.
- `auth-service` and `calendar-service` record a span per request, named by method and route
  template, with the request's log events attached.
- The rules for logs hold for traces: no query strings, and for calls to `auth-service` only the
  origin, never the path, since both can carry one-time tokens.

## Running on local Kubernetes

`./run-kubernetes.sh` runs the same setup on a local [kind](https://kind.sigs.k8s.io) cluster named
`me` (needs Docker, `kind` and `kubectl`). It builds the three images
([`auth-service/Dockerfile`](auth-service/Dockerfile),
[`calendar-service/Dockerfile`](calendar-service/Dockerfile),
[`auth-web/Dockerfile`](auth-web/Dockerfile)), loads them into the cluster and applies [`k8s/me.yaml`](k8s/me.yaml). Run it again after a code or
configuration change.

The three services and Caddy are four containers of one pod. They share its network, so they talk
over loopback as the plain processes do and only Caddy's ports 80 and 443 are published, on every
address of the machine (Docker publishes them itself, past a host firewall such as `ufw`).

A fifth container is the Aspire dashboard, to which the services send their traces:
<http://localhost:18888>. It has no sign-in, so its port is published on this machine's loopback
only, and it keeps traces in memory (they are gone when the pod restarts). It opens on the
Structured logs page, which stays empty; the data is on the Traces page, once requests have been
made. Sending telemetry to it needs a key that the script generates on every run.

The script takes its settings from the files the plain processes use: `.env` (as the secret
`me-env`), `auth-service/config.local.toml`, `calendar-service/config.local.toml` and the
`Caddyfile` (as the config map `me-config`). The data directories are the same ones too (`./data`,
`auth-web/data/dp-keys`, Caddy's `~/.local/share/caddy`), mounted into the cluster and written as
user id 1000. So accounts, sessions and the certificate carry over in both directions, and only one
of the two ways can run at a time.

```sh
kubectl --context kind-me -n me logs deploy/auth -c auth-service -f    # or -c calendar-service, -c auth-web, -c caddy
kubectl --context kind-me -n me exec deploy/auth -c auth-service -- \
  auth-service /etc/me/config.toml reset-password wiertmir             # see "Locked out"
docker stop me-control-plane          # stop (frees ports 80 and 443); `docker start` resumes
kind delete cluster --name me         # remove the cluster; the data stays on the host
```

## Backup

Everything worth keeping is in two directories. Stop the process before copying its directory (a
copy of a database that is being written to can be inconsistent).

| What                                      | Where                                   | If it is lost |
|-------------------------------------------|-----------------------------------------|---------------|
| Database                                  | `<data_dir>/auth.db` (with `auth.db-wal` and `auth.db-shm` when present) | Every account, linked sign-in method, session and app password is gone. On the next start the service creates an empty database and seeds a new admin |
| Calendars and events                      | `<data_dir>/calendar.db` (with `calendar.db-wal` and `calendar.db-shm` when present) | Every calendar and event is gone. On the next start the service creates an empty database |
| Token signing key                         | `<data_dir>/signing.key`                | The service creates a new key on the next start. Access and ID tokens issued before stop verifying; apps get new ones at their next refresh (refresh tokens live in the database), and services that verify tokens fetch the new public key by themselves |
| Cookie-encryption keys of `auth-web`      | the `DataProtection:Path` directory      | New keys are created. Every browser has to sign in again, and a form that was open at the time has to be reloaded. No account data is affected |

Both key files are secrets: whoever holds `signing.key` can issue tokens, and whoever holds the
cookie keys together with a cookie can read the session token in it. Protect the backup like the
original.

## Upgrading

There are no database migrations yet. Each service creates missing tables at start-up and never
alters an existing one, so a new version whose schema differs from the one that created your
database will not work with it. Until migrations exist, such a version needs a fresh database:
stop the service, move its database (`auth.db` or `calendar.db`) away and start again, which means
every account (or every calendar and event) has to be created again. Versions that do not change the
schema can be swapped in place. Keep a backup before any upgrade.

## Locked out

If the only admin forgets the password and "Forgot password" cannot help (no `[smtp]`, or the
admin's address is the default `…@localhost`), run this on the machine that holds the data
directory, with the same configuration file (and the same `ME_AUTH__…` variables) the service uses:

```sh
auth-service /etc/me/auth.toml reset-password wiertmir
```

It prints a one-time password for that username and exits; it does not start the server. Standard
output carries only the password (one line); the explanation and any log lines go to standard error. Sign in
with it and choose a new password. It works whether the service is running or stopped. Like an
admin's reset, it signs that user out everywhere and removes their app passwords (see the table
below). An unknown username gives an error and a non-zero exit status. Anyone who can run it can
read the database anyway, so it adds no new way in; it cannot be reached over HTTP.

If the one-time password of the very first start was lost before anything was set up, it is
simpler to stop the service, delete the data directory and start again.

## API documentation

Each service describes its API at `/api/docs` (a browsable page) and `/api/openapi.json`, on its
internal address: <http://127.0.0.1:8081/api/docs> for `auth-service`,
<http://127.0.0.1:8083/api/docs> for `calendar-service`. The page loads its viewer script from a
public CDN (`cdn.jsdelivr.net`), so it needs internet access in the browser. Neither endpoint asks
for a secret; do not expose `/api/*` publicly (the Caddyfile does not).

While the project is in development the Caddyfile publishes the two of `calendar-service` under
other paths, so that they can be read from other machines: `https://<host>/calendar/docs` (the
page) and `https://<host>/calendar/openapi.json`. Anyone who can reach the site can read them
without signing in; they describe the API and contain no data. Remove the two `handle` blocks
marked for this in the Caddyfile to keep them internal.

## Logging

All processes log to the console.

- `auth-service`: `[log] format = "pretty"` (default) or `"json"`, and `level` (default `debug`);
  or `ME_AUTH__LOG__FORMAT=json`.
- `calendar-service`: the same keys under `[log]`; `ME_CALENDAR__LOG__FORMAT=json`.
- `auth-web` (Serilog): set the environment variable `LOG_FORMAT=json` for JSON lines; anything
  else gives the coloured console format. The level is `debug`, except for the framework's own
  loggers (`Microsoft.AspNetCore`, `System.Net.Http`), which are held at warning because below
  that they print request URLs, and those carry one-time tokens. One line per request is logged
  with the path only, never the query string.

Timestamps in all processes are local time: `2026-10-04 09:28:15.303` on the console, and with
the UTC offset in JSON lines (`2026-10-04T09:28:15.303+09:00`).

Every request gets an `X-Request-Id`. `auth-web` creates it, or keeps the one that arrived with the
request when it is well-formed (1 to 128 printable ASCII characters, no spaces) — from the proxy or
from any client, so do not treat the id as proof of where a request came from. It writes the id on
each of its log lines and sends it with its API calls; the service logs the same id. One id
therefore finds a user action in both logs. `calendar-service` does the same for the requests it
receives: it keeps a well-formed id or creates one, and writes it on its log lines.

Secrets are never logged, with one exception: the one-time password of the seeded admin. (The
`reset-password` command prints its one-time password to standard output, not to the log.) A login
name typed into a failed sign-in is logged with control characters removed and cut to 64 characters.

## Running the checks

```sh
cargo test --workspace
dotnet test auth-web.Tests
scripts/smoke-auth-web.sh
(cd scripts/browser-check && npm install)   # once
scripts/browser-check.sh
```

The two scripts build and start both processes themselves on a temporary data directory and need
ports 8081 and 5080 free (`smoke-auth-web.sh` also 2525, 5081 and 8099; `browser-check.sh` also 8082).
`browser-check.sh` drives the installed Google Chrome; it downloads no browser.

## Security notes for the operator

- What each action revokes for the account it is applied to ("grants" are refresh tokens,
  authorization codes, pending social sign-in and link tickets, and unused password-reset links):

  | Action                          | Sessions            | Grants | App passwords | Linked sign-in methods                 |
  |---------------------------------|---------------------|--------|---------------|----------------------------------------|
  | Password reset (email, admin or `reset-password`) | all                 | all    | all           | removed if the email was never verified |
  | Email verified through the mailed link | all                 | all    | all           | all removed (the password is kept)     |
  | Change password                 | all but the current | all    | all           | kept                                   |
  | Unlink a sign-in method         | all but the current | all    | kept          | the unlinked one is removed            |
  | Disable the account             | all                 | all    | all           | kept, unusable while disabled          |
  | Sign out                        | the current         | none   | none          | kept                                   |

- If you think someone else got into your account: first open the Security page and unlink every
  sign-in method you do not recognise, then change your password. That order matters: a password
  change keeps linked sign-in methods, so one left in place would let the intruder straight back in.
  If you cannot sign in at all, use "Forgot password" (or ask an admin), then do the same two steps.
- An email address typed at sign-up is "not verified" until its owner follows the mailed link (or
  completes a password reset by mail). Without `[smtp]` it therefore stays unverified, and a
  provider sign-in with the same address is refused ("an account with that email already exists")
  rather than attached to the account. An admin's reset of an account whose address is unverified
  also removes its linked sign-in methods.
- Verifying an address through the mailed link removes every sign-in method linked before that
  moment and signs the account out everywhere; a password reset on an account with an unverified
  address does the same. Whatever was attached while the address was unproven may belong to someone
  who registered another person's address. If you receive a verification mail for an account you
  did not create and choose to confirm it, use "Forgot password" next: you do not know the password
  that was set, and the reset replaces it.
- Access tokens are valid for 15 minutes and are not checked against the database, so an app can
  keep using one for up to 15 minutes after any of the actions above.
- A temporary password (seeded admin, admin-created user, admin reset) must be changed at the next
  sign-in; until then the user reaches only the change-password page.
