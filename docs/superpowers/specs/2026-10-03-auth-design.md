# Me — Authentication: design

Date: 2026-10-03
Status: awaiting review

## Context

"Me" is a personal suite: a calendar service, a tasks service, a CalDAV bridge
and a Slint desktop client (which also reads Gmail/Hotmail/Yahoo mail). Each is
a separate executable. All of them need one notion of "who is this user", so
authentication is built first.

This spec covers two projects:

- `auth-service` — Rust. Owns users, credentials, sessions and tokens.
- `auth-web` — .NET 10 Blazor Server. The user interface for auth-service.

It also creates `common`, a small Rust library the later services use to verify
tokens.

It runs on a home network first and on a public host later.

## Goals

- Local accounts (username + password) and sign-in with Google, GitHub and
  Microsoft.
- Self sign-up from the web app.
- A seeded admin user `wiertmir` who must set a new password at first login.
- Standard tokens (OAuth 2 authorization code + PKCE, OpenID Connect) that the
  Slint client and the other services consume.
- App passwords for clients that can only send a username and password
  (CalDAV on phones).
- A web app that looks polished and professional.

## Out of scope for this version

- Two-factor codes and passkeys.
- Registering third-party OAuth clients through a UI. Our clients are listed in
  the config file.
- Automatic signing-key rotation.

## Layout

```
Me/
  Cargo.toml            Rust workspace: common, auth-service
  common/               JWT verification against the JWKS endpoint; error/JSON conventions
  auth-service/         Rust binary
  auth-web/             Blazor Server project (own .csproj, not in the Cargo workspace)
  docs/
```

## Deployment shape

Both processes sit behind one reverse proxy (Caddy) on one origin, which also
provides TLS:

| Path                                             | Goes to      |
|--------------------------------------------------|--------------|
| `/oauth/*`, `/.well-known/*`, `/social/*`        | auth-service |
| everything else                                  | auth-web     |

auth-service's `/api/*` is not exposed publicly. Only auth-web (and later the
CalDAV bridge) call it, over the internal network, with a shared service secret
from config.

For local development no proxy is needed: both run on localhost ports and the
issuer URL is set in config.

## auth-service

**Stack:** tokio, axum, SQLite (rusqlite, bundled), argon2, jsonwebtoken,
oauth2 + reqwest (social providers), lettre (SMTP), serde, tracing, utoipa
(OpenAPI).

### Data model

- `users`: id (UUID), username (unique), email (unique), email_verified,
  display_name, password_hash (nullable — social-only accounts), is_admin,
  must_change_password, disabled, created_at.
- `identities`: user_id, provider (`google` | `github` | `microsoft`),
  provider_subject, email. Unique on (provider, provider_subject).
- `sessions`: token hash, user_id, created_at, last_seen, expires_at,
  user agent, IP. A session is what the web app holds after sign-in.
- `auth_requests`: pending OAuth authorization requests ("challenges"):
  client_id, redirect_uri, scope, state, PKCE challenge, nonce, expires_at.
- `auth_codes`: one-time authorization codes bound to an auth request and user.
- `refresh_tokens`: token hash, family id, user_id, client_id, expires_at,
  used flag.
- `app_passwords`: id, user_id, label, hash, created_at, last_used.
- `email_tokens`: hash, user_id, purpose (`verify` | `reset`), expires_at.

Every secret token is stored hashed (SHA-256; they are high-entropy random
values). Passwords use Argon2id.

### Accounts

- **Seed:** on first start with an empty users table, create admin `wiertmir`
  (username configurable) with a random one-time password logged to the
  console and `must_change_password` set.
- **Forced change:** a session for a user with `must_change_password` is
  accepted only by the change-password endpoint. Everything else answers
  "password change required". Admin-created users and admin password resets
  set the same flag.
- **Sign-up:** username, email, password. Controlled by config
  `signup = "open" | "disabled"` (default open). When SMTP is configured the
  account must verify its email before it can sign in; without SMTP it is
  active immediately.
- **Password rules:** minimum 12 characters, no composition rules.
- **Reset:** emailed one-time link (1 hour). Without SMTP only an admin can
  reset. The response never reveals whether an email exists.
- **Rate limiting:** failed sign-ins are counted per username and per IP, with
  increasing delays. Counters are in memory (lost on restart — acceptable for
  one process).

### Social sign-in

Authorization code flow against each provider, run entirely by auth-service
(`/social/{provider}/start` and `/social/{provider}/callback`), with state and
PKCE. Provider client IDs and secrets come from config; a provider with no
config is not offered.

On callback:

1. Identity already linked → sign that user in.
2. Not linked, started from the account page of a signed-in user → link it.
3. Not linked, provider reports a verified email equal to an existing user's
   **verified** email → link and sign in.
4. Otherwise, if sign-up is open → create a new account (no password) from the
   provider's profile. If sign-up is disabled → refuse.

The callback ends by redirecting the browser to auth-web with a one-time
ticket, which auth-web exchanges for a session.

### OAuth 2 / OpenID Connect for our own apps

- `GET /oauth/authorize` — validates client, redirect URI, PKCE (S256
  required), stores an auth request, redirects the browser to
  `auth-web /signin?challenge=…`.
- auth-web, once the user is signed in, calls
  `POST /api/auth-requests/{challenge}/accept` with the session and receives
  the redirect URL carrying the authorization code.
- `POST /oauth/token` — `authorization_code` and `refresh_token` grants.
- `GET /oauth/userinfo`, `GET /.well-known/openid-configuration`,
  `GET /.well-known/jwks.json`.
- `POST /oauth/revoke` — revoke a refresh token.

Tokens:

- Access token: JWT, ES256, 15 minutes. Claims `iss`, `sub` (user id), `aud`,
  `exp`, `iat`, `scope`, `preferred_username`.
- ID token when the `openid` scope is requested.
- Refresh token: opaque, 30 days, rotated on every use. Reuse of an old one
  revokes the whole family.
- Signing key: generated on first start, stored in a key file next to the
  database. Rotation is manual (replace the file) in this version.

Clients are public (no secret) and listed in config with their allowed
redirect URIs. The Slint client uses a loopback redirect.

### Internal API (`/api/*`, service secret required)

Used by auth-web; user-scoped calls also carry the user's session token.

- Sign-in with password; sign-out; exchange social ticket for a session.
- Sign-up; verify email; request reset; complete reset; change password.
- Current user: profile, linked identities (list/unlink), sessions
  (list/revoke), app passwords (create — shown once / list / delete).
- Admin: list users, create user, disable/enable, reset password, set admin.
- Auth requests: fetch details, accept.
- `POST /api/app-passwords/verify` — for the CalDAV bridge: username + app
  password → user id.

Guard rails: an account cannot remove its last sign-in method; the last admin
cannot be demoted or disabled.

### Errors

One JSON error shape (`code`, `message`) with proper HTTP status, defined in
`common`. OAuth endpoints use the error format their RFCs require. Sign-in
failures give one generic message regardless of cause.

### Logging

Structured logging with `tracing`. Events carry fields (`user_id`,
`client_id`, `provider`, `request_id`, `ip`, outcome), not interpolated
strings. Output goes to the console; config selects human-readable (default,
coloured) or JSON lines, and the level.

- Every request gets a span with method, path, status, duration and a request
  id. An incoming `X-Request-Id` (sent by auth-web) is reused, so one user
  action can be followed across both processes.
- Security events are logged at info or warn: sign-in success and failure,
  sign-up, password change and reset, identity link/unlink, token issue,
  refresh-token reuse, app-password use, every admin action.
- Passwords, tokens, codes and secrets are never logged. The only exception is
  the seeded user's one-time password at first start.

### OpenAPI

The service's HTTP API is described by an OpenAPI 3.1 document generated from
the code (`utoipa` annotations on handlers and types), so it cannot drift from
the implementation.

- Served at `/api/openapi.json`, with a browsable documentation page at
  `/api/docs`. Both sit on the internal API surface.
- Covers the internal `/api/*` endpoints and the public OAuth and social
  endpoints, with request and response schemas, the error shape, the security
  schemes (service secret, session token, bearer access token) and an example
  per endpoint.
- A test builds the document and fails if any route is missing from it.

The calendar and tasks services will follow the same convention.

### Configuration

One TOML file plus environment overrides for secrets: issuer URL, listen
address, database path, service secret, sign-up mode, seed username, SMTP
settings, per-provider client id/secret, OAuth clients.

## common

- Fetch and cache the JWKS, verify an access token, return the user id and
  scopes. An axum extractor wraps it.
- The shared JSON error type.

Nothing else until a second service needs it.

## auth-web (Blazor Server, .NET 10)

Holds no data and makes no security decisions. Every action is a call to
auth-service's internal API.

- **Session:** after sign-in, the session token from auth-service is kept in an
  HttpOnly, Secure, SameSite=Lax cookie. Each API call forwards it.
- **Rendering:** pages that set or clear the cookie (sign in, sign up, sign
  out, social completion, forced password change) are statically rendered
  forms, because an interactive Blazor circuit cannot write cookies. Account
  and admin pages are interactive.
- **Pages:** sign in (password + provider buttons), sign up, verify email,
  forgot / reset password, forced password change, account (profile, linked
  identities, sessions, app passwords), admin users, and an error page.
- **Challenge handling:** `/signin?challenge=…` signs the user in if needed,
  then accepts the auth request and redirects to the client. No consent
  screen, since all clients are our own.
- **Logging:** NLog (`NLog.Web.AspNetCore`) as the logging provider, with
  structured message templates (`"Sign-in failed for {Username}"`) so
  properties stay queryable. Console target, coloured and human-readable by
  default, JSON layout selectable in `nlog.config`. Each request generates an
  `X-Request-Id`, includes it in every log event and forwards it to
  auth-service. Form values for passwords and tokens are never logged.
- **Design:** one visual system built for this app — light and dark themes,
  responsive from phone to desktop, keyboard navigation, visible focus, and
  labelled form controls. The visual direction is set during implementation
  and reviewed in the browser.

## Testing

- **auth-service:** integration tests that start the app in-process against a
  temporary SQLite file and exercise the real HTTP API: seeded user and forced
  change, sign-up and verification, sign-in rate limiting, reset, the full
  authorize → code → token → refresh → reuse-detection flow, app-password
  verification, and admin guard rails. Social providers are tested against a
  stub provider server.
- **common:** token verification accepts a valid token and rejects expired,
  wrong-audience and wrong-key tokens.
- **auth-web:** it has little logic of its own, so it is verified by running
  each flow in a real browser against a running auth-service.

## Build order

1. Workspace, config, database, seeded user, password sign-in, sessions,
   forced password change.
2. auth-web shell with sign-in, forced change and account basics.
3. Sign-up, email verification, password reset, admin users.
4. OAuth 2 / OpenID Connect endpoints and `common`.
5. Social sign-in.
6. App passwords.
7. Visual polish pass across all pages.
