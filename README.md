# Me

A personal suite of self-hosted services with a desktop client.

| Project            | What it is                                              | Tech                  | Status   |
|--------------------|---------------------------------------------------------|-----------------------|----------|
| `auth-service`     | Users, sign-in, OAuth 2 / OpenID Connect tokens          | Rust                  | working  |
| `auth-web`         | Web app for sign-in, sign-up and account management      | .NET Blazor Server    | working  |
| `calendar-service` | Calendar REST API                                        | Rust                  | planned  |
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
- `common/` — Rust code shared with later services (API errors, token verification).
- `auth-web.Tests/` — component tests for `auth-web`.
- `scripts/` — end-to-end checks.
- `Caddyfile` — reverse-proxy configuration that puts both processes on one origin.

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
```

- `auth-web` (open this one): <http://localhost:5080>
- `auth-service`: <http://localhost:8081> (API documentation at <http://localhost:8081/api/docs>)

On its first start with an empty data directory the service creates the admin user `wiertmir` and
writes a warning line to its console that contains `one_time_password=…`. Sign in with that
username and password; the first sign-in forces you to choose a new password. The line is printed
only once. If you lose it, stop the service, delete `./data` and start again.

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
| `service_secret` | Secret that `auth-web` sends on every API call; at least 16 characters               |
| `signup`         | `"open"` (anyone can create an account) or `"disabled"` (only an admin creates users) |
| `seed_username`  | Name of the admin created on the first start                                         |
| `[log]`          | `format` (`"pretty"` or `"json"`) and `level`                                        |
| `[[clients]]`    | OAuth clients: `id`, `name`, `redirect_uris`                                         |
| `[smtp]`         | Outgoing mail, see below                                                             |
| `[providers.*]`  | Social sign-in, see below                                                            |

Any value can be overridden by an environment variable named `ME_AUTH__<KEY>`, with `__` for a
nested table: `ME_AUTH__SERVICE_SECRET`, `ME_AUTH__DATA_DIR`, `ME_AUTH__LOG__FORMAT=json`.
Overrides are read as text, so numbers (such as `smtp.port`) have to be set in the file.

**Mail.** Without an `[smtp]` section no mail is sent: new accounts need no email verification and
only an admin can reset a password. With it, sign-up sends a verification link and "Forgot password"
sends a reset link.

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
characters, marks its cookies `Secure` and redirects HTTP to HTTPS, so it needs to sit behind a
TLS-terminating proxy (below).

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
when the provider confirms that the address is verified. Microsoft gives no such confirmation, so a
Microsoft account is never linked by email: sign in with your password and link it on the Security
page.

## Deployment behind Caddy

The [`Caddyfile`](Caddyfile) serves both processes on one origin and provides TLS:

| Path                                      | Goes to                          |
|-------------------------------------------|----------------------------------|
| `/social/complete`                        | auth-web (`127.0.0.1:5080`)      |
| `/oauth/*`, `/.well-known/*`, `/social/*` | auth-service (`127.0.0.1:8081`)  |
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
headers, which it trusts only from a proxy on the same machine (loopback). Keep both processes
listening on `127.0.0.1` so that nothing reaches them except through the proxy.

## API documentation

The service describes its API at `/api/docs` (a browsable page) and `/api/openapi.json`, on its
internal address: <http://127.0.0.1:8081/api/docs>. The page loads its viewer script from a public
CDN (`cdn.jsdelivr.net`), so it needs internet access in the browser. Neither endpoint asks for the
service secret; do not expose `/api/*` publicly (the Caddyfile does not).

## Logging

Both processes log to the console.

- `auth-service`: `[log] format = "pretty"` (default) or `"json"`, and `level`; or
  `ME_AUTH__LOG__FORMAT=json`.
- `auth-web`: set the environment variable `LOG_FORMAT=json` for JSON lines; anything else gives
  the coloured console format.

Every request gets an `X-Request-Id`. `auth-web` creates it (or keeps one sent by the proxy), writes
it on each of its log lines and sends it with its API calls; the service logs the same id. One id
therefore finds a user action in both logs.

Secrets are never logged, with one exception: the one-time password of the seeded admin.

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

- A password reset (by email link or by an admin) signs the user out everywhere: it revokes all
  sessions, refresh tokens and app passwords.
- Linked sign-in methods on an account with a verified email survive a password reset. After
  recovering an account, review them on the Security page and unlink any you do not recognise.
- Access tokens are valid for 15 minutes and are not checked against the database, so an app can
  keep using one for up to 15 minutes after a password reset or after the account is disabled.
- A temporary password (seeded admin, admin-created user, admin reset) must be changed at the next
  sign-in; until then the user reaches only the change-password page.
