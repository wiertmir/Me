# Me — Authentication: design

Date: 2026-10-03
Status: implemented

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
| `/social/complete`                               | auth-web (the page that finishes a social sign-in; matched first) |
| `/oauth/*`, `/.well-known/*`, `/social/*`        | auth-service |
| `/api/*`                                         | nothing (404) |
| everything else                                  | auth-web     |

auth-service's `/api/*` is not exposed publicly. Only auth-web (and later the
CalDAV bridge) call it, over the internal network, with a shared service secret
from config.

For local development no proxy is needed: both run on localhost ports and the
issuer URL is set in config.

## auth-service

**Stack:** tokio, axum, SQLite (rusqlite, bundled), argon2, jsonwebtoken,
reqwest (social providers; the authorization code flow is written directly
against it, the `oauth2` crate is not used), lettre (SMTP), serde, tracing,
utoipa (OpenAPI).

### Data model

- `users`: id (UUID), username (unique), email (unique), email_verified,
  display_name, password_hash (nullable — social-only accounts), is_admin,
  must_change_password, disabled, created_at.
- `identities`: user_id, provider (`google` | `github` | `microsoft`),
  provider_subject, email, created_at. Unique on (provider, provider_subject)
  and on (user_id, provider).
- `sessions`: id (UUID, what the session list and "revoke" refer to; never the
  token), token hash, user_id, created_at, last_seen, expires_at, user agent,
  IP. A session is what the web app holds after sign-in.
- `auth_requests`: pending OAuth authorization requests ("challenges"):
  client_id, redirect_uri, scope, state, PKCE challenge, nonce, expires_at.
- `auth_codes`: one-time authorization codes bound to an auth request and user.
- `refresh_tokens`: token hash, family id, user_id, client_id, scope,
  expires_at, used flag.
- `app_passwords`: id, user_id, label, hash, created_at, last_used.
- `email_tokens`: hash, user_id, purpose (`verify` | `reset`), expires_at.
- `social_states`: state hash, provider, PKCE verifier, optional auth-request
  challenge, optional link_user_id (set when the flow was started to link a
  provider to a signed-in user), expires_at (10 minutes). One per started
  provider flow; consumed by the callback.
- `social_tickets`: ticket hash, user_id, optional challenge, expires_at
  (60 seconds). What the callback hands to auth-web to exchange for a session.
- `social_link_intents`: token hash, user_id, provider, expires_at
  (10 minutes). Lets the signed-in user's browser start a link flow at
  auth-service, which cannot see the auth-web cookie.
- `social_link_tickets`: ticket hash, user_id, provider, subject, email,
  expires_at (60 seconds). The result of a link flow, waiting for the user to
  confirm it in their session.

Every secret token is stored hashed (SHA-256; they are high-entropy random
values). Passwords use Argon2id.

### Accounts

- **Seed:** on first start with an empty users table, create admin `wiertmir`
  (username configurable) with a random one-time password logged to the
  console and `must_change_password` set.
- **Forced change:** a session for a user with `must_change_password` is
  accepted only by the change-password endpoint, `GET /api/me` and sign-out.
  Everything else answers "password change required". Admin-created users and admin password resets
  set the same flag.
- **Sign-up:** username, email, password. Controlled by config
  `signup = "open" | "disabled"` (default open). When SMTP is configured the
  account must verify its email before it can sign in; without SMTP it is
  active immediately, with its email address left unverified (see "Security
  rules" below).
- **Password rules:** minimum 12 characters, no composition rules.
- **Reset:** emailed one-time link (1 hour). Without SMTP only an admin can
  reset. The response never reveals whether an email exists.
- **Setting a first password:** an account without a password (created by
  social sign-in) sets one through the change-password endpoint without a
  current password. An account that has one must always give it.
- **Recovery command:** `auth-service <config> reset-password <username>` gives
  the user a one-time password through the admin-reset code path, prints it
  and exits without starting the server. It is the way back in for a sole
  admin who cannot use "Forgot password" (the seeded admin's address defaults
  to `<seed_username>@localhost`; config `seed_email` sets a real one).
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
2. Not linked, started from the account page of a signed-in user → issue a
   one-time link ticket; the identity is attached only when that user confirms
   it in their own session.
3. Not linked, provider reports a verified email equal to an existing user's
   **verified** email → link and sign in.
4. The email belongs to an existing user but rule 3 does not hold → refuse
   (`account_exists`): the user signs in with the password and links the
   provider from the account page.
5. Otherwise, if sign-up is open → create a new account (no password) from the
   provider's profile. If sign-up is disabled → refuse.

The callback ends by redirecting the browser to auth-web with a one-time
ticket, which auth-web exchanges for a session.

### Security rules

Decided during implementation; each closes a way to take over an account.

- **Self sign-up is never auto-verified.** An address typed at sign-up is
  verified only by the mailed link (or by completing a mailed reset). Without
  SMTP it stays unverified, so rule 3 never attaches a provider identity to an
  account someone registered with another person's address. Admin-created
  users and the seeded admin are verified: the admin vouches for them.
- **Microsoft emails are never treated as verified.** Microsoft gives no
  reliable verification signal, so a Microsoft identity is never linked by
  email and an account created from one has an unverified address.
- **No verification mail for passwordless accounts.** An account created
  through a provider may carry an address its creator does not own; mailing a
  verification link would let the real owner bless it by accident. Such an
  account's address becomes verified only through a password reset.
- **A password reset revokes all sign-in state** — sessions, refresh tokens,
  authorization codes, pending social tickets, link intents and link tickets,
  other reset links, app passwords — in the transaction that stores the new
  password. When the account's email was unverified it also removes the linked
  identities, so whoever proves ownership of the address gets the account
  alone. This holds for the emailed reset, the admin reset and the recovery
  command.
- **A voluntary password change and an unlink revoke everything but the
  current session.** Change-password also deletes app passwords; unlink keeps
  them. Disabling a user revokes everything. One function
  (`users::revoke_sign_in_state`) holds the list of credential tables for all
  of these, and a test fails when the schema gains a table it does not cover.
  Recovery order for a user: unlink what you do not recognise, then change the
  password.
- **Identity links are confirmed in-session.** A link flow ends with a one-time
  link ticket bound to the user who started it; nothing is attached until that
  user's session posts the ticket (`POST /api/me/identities/confirm`) after an
  explicit confirmation on the Security page. A link flow completed in another
  browser therefore attaches nothing.
- **The example service secret is refused off loopback.** The service does not
  start with the secret published in `config.example.toml` unless it listens
  on a loopback address.

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
- Sign-up; verify email; resend verification; request reset; complete reset;
  change or set password.
- Social: list configured providers; create a link intent.
- Current user: profile, linked identities (list/unlink/confirm link), sessions
  (list/revoke), app passwords (create — shown once / list / delete).
- Admin: list users, create user, disable/enable, reset password, set admin.
- Auth requests: fetch details, accept.
- `POST /api/app-passwords/verify` — for the CalDAV bridge: username (or
  email) + app password → user id and username. Guarded by the service secret
  like the rest of `/api/*`, rate limited, and it fails for disabled users and
  users who must change their password.

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
address, data directory, service secret, sign-up mode, seed username and
email, token audience, log format and level, SMTP settings, per-provider
client id/secret, OAuth clients.

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
  default; the `LOG_FORMAT=json` environment variable selects the JSON layout
  (both targets are defined in `nlog.config`). Each request generates an
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
- **auth-web:** three layers.
  - bUnit component tests (`auth-web.Tests`) for the interactive pages, against
    a stubbed API: link confirmation, unlink, app passwords, admin actions,
    the profile form, session expiry.
  - `scripts/smoke-auth-web.sh`: starts both processes on a temporary data
    directory and drives the server-rendered flows with curl — sign-in, forced
    change, sign-up with and without mail (against a local SMTP sink),
    verification, reset, the OAuth challenge, security headers, outage
    handling, setting a first password.
  - `scripts/browser-check.sh`: the same two processes driven through headless
    Google Chrome (Playwright) for what needs a real browser — the Blazor
    circuit, dialogs, clipboard, responsive layout, console and CSP errors.

## Build order

1. Workspace, config, database, seeded user, password sign-in, sessions,
   forced password change.
2. auth-web shell with sign-in, forced change and account basics.
3. Sign-up, email verification, password reset, admin users.
4. OAuth 2 / OpenID Connect endpoints and `common`.
5. Social sign-in.
6. App passwords.
7. Visual polish pass across all pages.
