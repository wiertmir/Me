#!/usr/bin/env bash
# End-to-end smoke test: starts auth-service (temp data dir) and auth-web, drives the sign-in flows with curl.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
SVC=http://127.0.0.1:8081
PUBLIC=http://localhost:8081
WEB=http://localhost:5080
T="$(mktemp -d)"
PIDS=()
FAILS=0

cleanup() {
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -- "-$p" 2>/dev/null; done
  sleep 1
  rm -rf "$T"
}
trap cleanup EXIT

pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; FAILS=$((FAILS + 1)); }
check() { # check "name" <condition exit code>
  if [ "$2" -eq 0 ]; then pass "$1"; else fail "$1 (${3:-})"; fi
}

# req JAR URL [curl args...] -> CODE, LOC, $T/body, $T/hdr (never follows redirects)
req() {
  local jar="$1" url="$2"; shift 2
  CODE=$(curl -s -b "$jar" -c "$jar" -D "$T/hdr" -o "$T/body" -w '%{http_code}' "$@" "$url")
  LOC=$(grep -i '^location:' "$T/hdr" | head -1 | cut -d' ' -f2- | tr -d '\r\n')
  LOC=${LOC#"$WEB"} # Blazor redirects are absolute; compare paths
}

# form_post JAR URL handler [-d field=value ...]: GET the form for its antiforgery token, then POST it
form_post() {
  local jar="$1" url="$2" handler="$3"; shift 3
  req "$jar" "$url"
  local token
  token=$(grep -o 'name="__RequestVerificationToken"[^>]*value="[^"]*"' "$T/body" | head -1 | sed 's/.*value="//;s/"$//')
  req "$jar" "$url" -X POST --data-urlencode "__RequestVerificationToken=$token" --data-urlencode "_handler=$handler" "$@"
}

has_cookie() { grep -q $'\tme_auth\t' "$1" 2>/dev/null; }

for port in 8081 5080; do
  if curl -s -o /dev/null "http://127.0.0.1:$port/" ; then echo "port $port already in use"; exit 2; fi
done

echo "== building"
cargo build -q -p auth-service || { echo "cargo build failed"; exit 2; }
dotnet build auth-web -v q --nologo 2>&1 | tail -3
[ "${PIPESTATUS[0]}" -eq 0 ] || { echo "dotnet build failed"; exit 2; }

echo "== starting auth-service and auth-web"
ME_AUTH__DATA_DIR="$T/data" ME_AUTH__LOG__FORMAT=json setsid ./target/debug/auth-service auth-service/config.example.toml >"$T/svc.log" 2>&1 &
SVC_PID=$!; PIDS+=("$SVC_PID")
ASPNETCORE_ENVIRONMENT=Development ASPNETCORE_URLS=$WEB LOG_FORMAT=json DataProtection__Path="$T/dp" \
  setsid dotnet run --no-build --no-launch-profile --project auth-web >"$T/web.log" 2>&1 &
PIDS+=("$!")

for i in $(seq 60); do curl -sf "$SVC/health" >/dev/null && break; sleep 1; done
for i in $(seq 60); do curl -s -o /dev/null "$WEB/signin" && break; sleep 1; done
curl -sf "$SVC/health" >/dev/null || { echo "auth-service did not start"; cat "$T/svc.log"; exit 2; }
curl -s -o /dev/null "$WEB/signin" || { echo "auth-web did not start"; cat "$T/web.log"; exit 2; }
SEED=$(grep -o '"one_time_password":"[^"]*"' "$T/svc.log" | head -1 | cut -d'"' -f4)
[ -n "$SEED" ] || { echo "no seed password in service log"; exit 2; }
NEWPW="smoke-test-password-1"

echo "== checks"
A="$T/a.jar"; B="$T/b.jar"; C="$T/c.jar"; D="$T/d.jar"; E="$T/e.jar"

req "$A" "$WEB/account"
[ "$CODE" = 302 ] && [[ "$LOC" == /signin* ]]; check "1 unauthenticated /account redirects to /signin" $? "$CODE $LOC"

form_post "$A" "$WEB/signin" signin --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=$SEED"
[ "$CODE" = 302 ] && [[ "$LOC" == /change-password* ]]; check "2a seed sign-in redirects to /change-password" $? "$CODE $LOC"
req "$A" "$WEB/account"
[ "$CODE" = 302 ] && [[ "$LOC" == /change-password* ]]; check "2b /account redirects to /change-password while change is pending" $? "$CODE $LOC"

form_post "$B" "$WEB/signin" signin --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=wrong-password-xx"
[ "$CODE" = 200 ] && grep -q "Incorrect username, email or password" "$T/body" && ! has_cookie "$B"
check "3 wrong password: 200, generic error, no cookie" $? "$CODE"

form_post "$A" "$WEB/change-password" change-password \
  --data-urlencode "Input.Current=$SEED" --data-urlencode "Input.New=$NEWPW" --data-urlencode "Input.Confirm=$NEWPW"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "4a change-password redirects to /account" $? "$CODE $LOC"
req "$A" "$WEB/account"
[ "$CODE" = 200 ] && grep -q wiertmir "$T/body"; check "4b /account shows the profile" $? "$CODE"

req "$C" "$WEB/signin?returnUrl=//evil.example"
form_post "$C" "$WEB/signin?returnUrl=//evil.example" signin --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=$NEWPW"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "5 returnUrl=//evil.example is ignored" $? "$CODE $LOC"

VERIFIER=$(head -c 48 /dev/urandom | base64 | tr -d '=+/\n' | head -c 64)
CHAL=$(printf %s "$VERIFIER" | openssl dgst -sha256 -binary | basenc --base64url | tr -d '=\n')
req "$C" "$PUBLIC/oauth/authorize?response_type=code&client_id=me-desktop&redirect_uri=http%3A%2F%2F127.0.0.1%2Fcallback&scope=openid&state=xyz&code_challenge=$CHAL&code_challenge_method=S256"
[ "$CODE" = 302 ] && [[ "$LOC" == "/signin?challenge="* ]]; check "6a /oauth/authorize redirects to auth-web with a challenge" $? "$CODE $LOC"
req "$C" "$WEB$LOC"
[ "$CODE" = 302 ] && [[ "$LOC" == http://127.0.0.1*/callback\?code=*\&state=xyz ]]; check "6b signed-in challenge goes to the client callback" $? "$CODE" # redirect target not printed: it holds the code

req "$C" "$WEB/signout"
[ "$CODE" = 200 ]; check "7a GET /signout only shows the confirmation" $? "$CODE"
req "$C" "$WEB/account"
[ "$CODE" = 200 ]; check "7b still signed in after GET /signout" $? "$CODE"
form_post "$C" "$WEB/signout" signout
[ "$CODE" = 302 ] && [ "$LOC" = "/signin" ]; check "7c POST /signout redirects to /signin" $? "$CODE $LOC"
req "$C" "$WEB/account"
[ "$CODE" = 302 ] && [[ "$LOC" == /signin* ]]; check "7d /account redirects to /signin after sign-out" $? "$CODE $LOC"

RID="smoke-$$-$RANDOM"
req "$D" "$WEB/signin"
TOKEN=$(grep -o 'name="__RequestVerificationToken"[^>]*value="[^"]*"' "$T/body" | head -1 | sed 's/.*value="//;s/"$//')
req "$D" "$WEB/signin" -X POST -H "X-Request-Id: $RID" -H "X-Forwarded-For: 203.0.113.9" \
  --data-urlencode "__RequestVerificationToken=$TOKEN" --data-urlencode "_handler=signin" \
  --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=$NEWPW"
sleep 1
grep -q "$RID" "$T/web.log" && grep -q "$RID" "$T/svc.log"; check "10 request id $RID is in both logs" $?
grep '"signin"' "$T/svc.log" | grep -q '203.0.113.9'; check "10b service saw the forwarded client IP" $?

# non-ASCII User-Agent must not break sign-in
UA_JAR="$T/ua.jar"
form_post "$UA_JAR" "$WEB/signin" signin -A 'Tést/1.0 ✓' --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=$NEWPW"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "11 sign-in works with a non-ASCII User-Agent" $? "$CODE $LOC"

# headers on a 404
req "$E" "$WEB/definitely-not-a-page"
H=$(tr -d '\r' < "$T/hdr" | tr 'A-Z' 'a-z')
echo "$H" | grep -q '^x-content-type-options: nosniff' && echo "$H" | grep -q '^content-security-policy: ' && echo "$H" | grep -q '^x-request-id: '
check "12 404 carries security headers and X-Request-Id" $? "$CODE"

# /session-expired: valid session is kept, vanished session is cleared (service restarted on an empty data dir)
req "$UA_JAR" "$WEB/session-expired"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ] && ! grep -qi '^set-cookie: me_auth' "$T/hdr"
check "13a /session-expired with a valid session goes to /account and keeps the cookie" $? "$CODE $LOC"
kill -- "-$SVC_PID" 2>/dev/null; sleep 1; rm -rf "$T/data"
ME_AUTH__DATA_DIR="$T/data" ME_AUTH__LOG__FORMAT=json setsid ./target/debug/auth-service auth-service/config.example.toml >>"$T/svc.log" 2>&1 &
SVC_PID=$!; PIDS+=("$SVC_PID")
for i in $(seq 30); do curl -sf "$SVC/health" >/dev/null && break; sleep 1; done
req "$UA_JAR" "$WEB/session-expired"
[ "$CODE" = 302 ] && [ "$LOC" = "/signin" ] && grep -i '^set-cookie: me_auth=;' "$T/hdr" | grep -qi 'expires=Thu, 01 Jan 1970'
check "13b /session-expired with a vanished session clears the cookie and goes to /signin" $? "$CODE $LOC"

req "$E" "$WEB/signin"
H=$(tr -d '\r' < "$T/hdr" | tr 'A-Z' 'a-z')
echo "$H" | grep -q '^x-content-type-options: nosniff' && echo "$H" | grep -q '^referrer-policy: no-referrer' \
  && echo "$H" | grep -q '^x-frame-options: deny' && echo "$H" | grep -q '^content-security-policy: ' \
  && echo "$H" | grep -Eq '^cache-control: .*no-store'
check "9 security headers on /signin" $?

TOKEN=$(curl -s -b "$E" -c "$E" "$WEB/signin" | grep -o 'name="__RequestVerificationToken"[^>]*value="[^"]*"' | head -1 | sed 's/.*value="//;s/"$//')
kill -- "-$SVC_PID" 2>/dev/null; sleep 1
req "$E" "$WEB/signin" -X POST --data-urlencode "__RequestVerificationToken=$TOKEN" --data-urlencode "_handler=signin" \
  --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=$NEWPW"
FIRST_CODE=$CODE; FIRST_LOC=$LOC
if [ "$CODE" = 302 ]; then req "$E" "$WEB$LOC"; fi
[ "$FIRST_CODE" != 500 ] && [ "$CODE" != 500 ] && [[ "$FIRST_LOC" == /unavailable* || "$CODE" = 503 || "$CODE" = 200 ]] \
  && grep -q "Service unavailable" "$T/body" && ! grep -Eqi 'exception| at [A-Za-z]+\.|stack' "$T/body"
check "8 service down: unavailable page, no 500, no stack trace" $? "$FIRST_CODE $FIRST_LOC -> $CODE"
sleep 1
N=$(grep -c '"level":"error"' "$T/web.log")
[ "$N" = 1 ]; check "8b exactly one error line in the auth-web log" $? "$N lines"
grep '"level":"error"' "$T/web.log" | grep -qi 'StackTrace\| at System\.'; [ $? -ne 0 ]; check "8c error line has no stack trace" $?
# non-JSON 502 from the service (proxy error page) must show the unavailable page, not a 500
setsid python3 - <<'PY' &
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def _go(self):
        self.send_response(502); self.send_header("Content-Type", "text/html"); self.end_headers()
        self.wfile.write(b"<html><body>Bad Gateway</body></html>")
    do_GET = do_POST = _go
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", 8099), H).serve_forever()
PY
STUB_PID=$!; PIDS+=("$STUB_PID")
ASPNETCORE_ENVIRONMENT=Development ASPNETCORE_URLS=http://localhost:5081 LOG_FORMAT=json DataProtection__Path="$T/dp2" \
  AuthService__BaseUrl=http://127.0.0.1:8099 setsid dotnet run --no-build --no-launch-profile --project auth-web >"$T/web2.log" 2>&1 &
PIDS+=("$!")
for i in $(seq 60); do curl -s -o /dev/null "http://localhost:5081/signin" && break; sleep 1; done
F="$T/f.jar"; W2=http://localhost:5081
req "$F" "$W2/signin"
TOKEN=$(grep -o 'name="__RequestVerificationToken"[^>]*value="[^"]*"' "$T/body" | head -1 | sed 's/.*value="//;s/"$//')
req "$F" "$W2/signin" -X POST --data-urlencode "__RequestVerificationToken=$TOKEN" --data-urlencode "_handler=signin" \
  --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=x"
C1=$CODE; L1=$LOC
LOC=${LOC#"$W2"}
[ "$CODE" = 302 ] && req "$F" "$W2$LOC"
[ "$C1" != 500 ] && [ "$CODE" != 500 ] && grep -q "Service unavailable" "$T/body"
check "14 HTML 502 from the service shows the unavailable page" $? "$C1 $L1 -> $CODE"


# ---- sign-up, verification, reset, providers, social (each section restarts auth-service on a fresh data dir)
start_svc() { # start_svc NAME "extra TOML appended to the example config"
  [ -n "${SVC_PID:-}" ] && kill -- "-$SVC_PID" 2>/dev/null; sleep 1
  cp auth-service/config.example.toml "$T/$1.toml"; printf '%s\n' "$2" >>"$T/$1.toml"
  ME_AUTH__DATA_DIR="$T/data-$1" ME_AUTH__LOG__FORMAT=json setsid ./target/debug/auth-service "$T/$1.toml" >"$T/svc-$1.log" 2>&1 &
  SVC_PID=$!; PIDS+=("$SVC_PID")
  for i in $(seq 30); do curl -sf "$SVC/health" >/dev/null && return; sleep 1; done
  echo "service $1 did not start"; cat "$T/svc-$1.log"; exit 2
}
mail_count() { ls "$T/mail" 2>/dev/null | grep -c '\.eml$'; }
wait_mail() { for i in $(seq 20); do [ "$(mail_count)" -ge "$1" ] && return 0; sleep 0.5; done; return 1; }
mail_token() { # mail_token verify|reset -> the token in the newest mail's link
  python3 - "$T/mail" "$1" <<'PY'
import email, glob, re, sys
msg = email.message_from_bytes(open(sorted(glob.glob(sys.argv[1] + "/*.eml"))[-1], "rb").read())
m = re.search(r"/%s\?token=([A-Za-z0-9_-]+)" % sys.argv[2], msg.get_payload(decode=True).decode())
print(m.group(1) if m else "")
PY
}
PW1="first-smoke-password-1"; PW2="second-smoke-password-2"; PW3="third-smoke-password-3"
SIGNUP_FIELDS() { echo --data-urlencode "Input.Username=$1" --data-urlencode "Input.Email=$2" --data-urlencode "Input.Password=$3" --data-urlencode "Input.Confirm=${4:-$3}"; }

echo "== sign-up with mail disabled"
start_svc nomail ""
G="$T/g.jar"
form_post "$G" "$WEB/signup" signup $(SIGNUP_FIELDS alice alice@example.test "$PW1")
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "S1a sign-up with mail disabled signs in and goes to /account" $? "$CODE $LOC"
req "$G" "$WEB/account"
[ "$CODE" = 200 ] && grep -q alice "$T/body"; check "S1b /account shows the new username" $? "$CODE"
H="$T/h.jar"
form_post "$H" "$WEB/signup" signup $(SIGNUP_FIELDS alice other@example.test "$PW1")
[ "$CODE" = 200 ] && grep -q "That username or email is already in use" "$T/body"; check "S2a duplicate username: 200 with the conflict message" $? "$CODE"
form_post "$H" "$WEB/signup" signup $(SIGNUP_FIELDS carol carol@example.test "$PW1" "$PW2")
[ "$CODE" = 200 ] && grep -q "passwords do not match" "$T/body"; check "S2b mismatched confirm: message" $? "$CODE"
form_post "$H" "$WEB/signup" signup $(SIGNUP_FIELDS carol carol@example.test "elevenchars")
[ "$CODE" = 200 ] && grep -q 'id="password-error"[^>]*>[^<]*at least 12' "$T/body"; check "S2c 11-character password: message under the password field" $? "$CODE"
req "$G" "$WEB/signup"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "S2d signed-in user visiting /signup is sent to /account" $? "$CODE $LOC"

echo "== mail: verification and reset"
setsid python3 scripts/smtp-sink.py "$T/mail" 2525 >"$T/sink.log" 2>&1 &
PIDS+=("$!")
for i in $(seq 20); do (echo > /dev/tcp/127.0.0.1/2525) 2>/dev/null && break; sleep 0.25; done
start_svc mail '[smtp]
host = "127.0.0.1"
port = 2525
from = "Me <no-reply@example.test>"
tls = "none"
username = ""
password = ""'
J="$T/j.jar"
form_post "$J" "$WEB/signup" signup $(SIGNUP_FIELDS bob bob@example.test "$PW1")
[ "$CODE" = 200 ] && grep -q "Check your email to finish creating your account" "$T/body" && grep -q "Resend email" "$T/body"
check "S3a sign-up with mail: 'check your email' page" $? "$CODE"
wait_mail 1; check "S3b verification mail arrived in the sink" $?
form_post "$J" "$WEB/signup" resend --data-urlencode "Input.Email=bob@example.test"
[ "$CODE" = 200 ] && grep -q "needs verifying" "$T/body" && wait_mail 2; check "S3c resend form: confirmation and a second mail" $? "$CODE"
VTOK=$(mail_token verify); [ -n "$VTOK" ]; check "S3d verify link found in the mail" $?
req "$J" "$WEB/verify?token=$VTOK"
[ "$CODE" = 200 ] && grep -q "Confirm your email" "$T/body" && grep -q 'name="Input.Token"' "$T/body"; check "S3e GET /verify only shows the confirm button" $? "$CODE"
form_post "$J" "$WEB/signin" signin --data-urlencode "Input.Login=bob" --data-urlencode "Input.Password=$PW1"
[ "$CODE" = 200 ] && grep -q "Verify your email before signing in" "$T/body" && ! has_cookie "$J"; check "S3f still unverified after the GET" $? "$CODE"
form_post "$J" "$WEB/verify?token=$VTOK" verify --data-urlencode "Input.Token=$VTOK"
[ "$CODE" = 200 ] && grep -q "Email verified" "$T/body"; check "S3g POST /verify verifies" $? "$CODE"
form_post "$J" "$WEB/signin" signin --data-urlencode "Input.Login=bob" --data-urlencode "Input.Password=$PW1"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "S3h sign-in works after verification" $? "$CODE $LOC"
form_post "$E" "$WEB/verify?token=$VTOK" verify --data-urlencode "Input.Token=$VTOK"
[ "$CODE" = 200 ] && grep -q "expired or was already used" "$T/body" && grep -q "Resend email" "$T/body"; check "S3i reused verify link: expired message with resend form" $? "$CODE"

N0=$(mail_count)
form_post "$E" "$WEB/forgot" forgot --data-urlencode "Input.Email=bob@example.test"
[ "$CODE" = 200 ] && grep -q "sent a reset link" "$T/body"; check "S4a forgot: confirmation text" $? "$CODE"
sed 's/value="[^"]*"//g;s/<!--Blazor[^>]*-->//' "$T/body" >"$T/forgot-known"
wait_mail $((N0 + 1)); check "S4b forgot: reset mail arrived" $?
form_post "$E" "$WEB/forgot" forgot --data-urlencode "Input.Email=nobody@example.test"
sed 's/value="[^"]*"//g;s/<!--Blazor[^>]*-->//' "$T/body" >"$T/forgot-unknown"
sleep 1
cmp -s "$T/forgot-known" "$T/forgot-unknown" && [ "$(mail_count)" = $((N0 + 1)) ]; check "S4c unknown address: identical page, no new mail" $?
RTOK=$(mail_token reset); [ -n "$RTOK" ]; check "S4d reset link found in the mail" $?
req "$E" "$WEB/reset?token=$RTOK"
[ "$CODE" = 200 ] && grep -q 'name="Input.New"' "$T/body"; check "S4e reset link shows the form" $? "$CODE"
form_post "$E" "$WEB/reset?token=$RTOK" reset --data-urlencode "Input.Token=$RTOK" --data-urlencode "Input.New=short" --data-urlencode "Input.Confirm=short"
[ "$CODE" = 200 ] && grep -q 'id="new-error"[^>]*>[^<]*at least 12' "$T/body"; check "S4f weak password: message under the field" $? "$CODE"
form_post "$E" "$WEB/reset?token=$RTOK" reset --data-urlencode "Input.Token=$RTOK" --data-urlencode "Input.New=$PW2" --data-urlencode "Input.Confirm=$PW2"
[ "$CODE" = 302 ] && [ "$LOC" = "/signin?notice=reset" ]; check "S4g token survived the weak attempt; good password redirects to /signin?notice=reset" $? "$CODE $LOC"
req "$E" "$WEB/signin?notice=reset"
grep -q "Password changed. Sign in with your new password" "$T/body"; check "S4h /signin shows the reset notice" $?
form_post "$E" "$WEB/signin" signin --data-urlencode "Input.Login=bob" --data-urlencode "Input.Password=$PW1"
[ "$CODE" = 200 ] && grep -q "Incorrect username" "$T/body"; check "S4i old password fails" $? "$CODE"
form_post "$E" "$WEB/signin" signin --data-urlencode "Input.Login=bob" --data-urlencode "Input.Password=$PW2"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "S4j new password works" $? "$CODE $LOC"
form_post "$D" "$WEB/reset?token=$RTOK" reset --data-urlencode "Input.Token=$RTOK" --data-urlencode "Input.New=$PW3" --data-urlencode "Input.Confirm=$PW3"
[ "$CODE" = 200 ] && grep -q "expired or was already used" "$T/body"; check "S4k reused reset link: expired message" $? "$CODE"

echo "== providers, error codes, social ticket"
K="$T/k.jar"
start_svc github '[providers.github]
client_id = "dummy-id"
client_secret = "dummy-secret"
auth_url = "http://127.0.0.1:9/authorize"'
req "$K" "$WEB/signin"
grep -q "Continue with GitHub" "$T/body" && grep -q 'href="http://localhost:8081/social/github/start"' "$T/body"
check "S5a provider configured: GitHub link to {PublicUrl}/social/github/start" $?
req "$K" "$WEB/signin?challenge=abc123"
grep -q 'href="http://localhost:8081/social/github/start?challenge=abc123"' "$T/body" && grep -q 'href="/signup?challenge=abc123"' "$T/body" \
  && grep -q 'href="/forgot?challenge=abc123"' "$T/body"; check "S5b with a challenge the links carry it" $?
start_svc noprov ""
req "$K" "$WEB/signin"
! grep -q "Continue with" "$T/body" && grep -q 'name="Input.Password"' "$T/body"; check "S5c no providers: no buttons, password form still there" $?
req "$K" "$WEB/signin?error=account_exists"
grep -q "An account with that email already exists" "$T/body"; check "S6a ?error=account_exists shows the mapped text" $?
req "$K" "$WEB/signin?error=%3Cscript%3Ealert(1)%3C/script%3E"
grep -q "Please try again" "$T/body" && ! grep -q "alert(1)" "$T/body"; check "S6b unknown error code: generic message, raw value not echoed" $?
TICKET="bogus-ticket-$RANDOM$RANDOM"
req "$K" "$WEB/social/complete?ticket=$TICKET"
[ "$CODE" = 302 ] && [ "$LOC" = "/signin?error=social_failed" ]; check "S7 bogus ticket redirects to /signin?error=social_failed" $? "$CODE $LOC"
ok=0
for u in verify reset signup forgot social/complete session-expired; do
  req "$K" "$WEB/$u"; grep -Eqi '^cache-control: .*no-store' "$T/hdr" || { ok=1; echo "   missing on /$u"; }
done
check "S8 /verify /reset /signup /forgot /social/complete /session-expired carry Cache-Control: no-store" $ok

sleep 1
leak=0
for secret in "$VTOK" "$RTOK" "$TICKET" "$PW1" "$PW2" "$PW3" "$NEWPW"; do
  if grep -qF -- "$secret" "$T"/svc-*.log "$T/svc.log" "$T/web.log"; then leak=1; echo "   secret of ${#secret} chars found in a log"; fi
done
check "S9 no token, ticket or password in either process's log" $leak

echo "== account and admin pages"
start_svc acct ""
SEED2=$(grep -o '"one_time_password":"[^"]*"' "$T/svc-acct.log" | head -1 | cut -d'"' -f4)
M="$T/m.jar"; N="$T/n.jar"; U="$T/u.jar"
form_post "$M" "$WEB/signup" signup $(SIGNUP_FIELDS dave dave@example.test "$PW1")
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "A0a non-admin user dave signed up and signed in" $? "$CODE $LOC"
form_post "$N" "$WEB/signin" signin --data-urlencode "Input.Login=wiertmir" --data-urlencode "Input.Password=$SEED2"
form_post "$N" "$WEB/change-password?returnUrl=/account/security%3Fnotice%3Dpassword" change-password \
  --data-urlencode "Input.Current=$SEED2" --data-urlencode "Input.New=$PW1" --data-urlencode "Input.Confirm=$PW1"
[ "$CODE" = 302 ] && [ "$LOC" = "/account/security?notice=password" ]; check "A0b change-password with a local returnUrl returns there" $? "$CODE $LOC"
form_post "$N" "$WEB/change-password?returnUrl=//evil.example" change-password \
  --data-urlencode "Input.Current=$PW1" --data-urlencode "Input.New=$PW2" --data-urlencode "Input.Confirm=$PW2"
[ "$CODE" = 302 ] && [ "$LOC" = "/account" ]; check "A0c change-password with returnUrl=//evil.example goes to /account instead" $? "$CODE $LOC"

# a standalone 43-character base64url run (Blazor's own markers are longer standard-base64 strings with + / =)
B64RUN='(^|[^A-Za-z0-9_+/=\\-])[A-Za-z0-9_-]{43}([^A-Za-z0-9_+/=\\-]|$)'
token_like() { grep -Eo -- ".{0,30}$B64RUN.{0,10}" "$T/body" | head -2 | tr '\n' ' '; }
for pair in "/account:Profile" "/account/security:Security" "/account/app-passwords:App passwords"; do
  path=${pair%%:*}; head=${pair#*:}
  req "$M" "$WEB$path"
  [ "$CODE" = 200 ] && grep -q "<h1>$head</h1>" "$T/body"; check "A1 $path: 200 with the '$head' heading" $? "$CODE"
  grep -Eqi '^cache-control: .*no-store' "$T/hdr"; check "A2 $path carries Cache-Control: no-store" $?
  # the session token is a 43-character base64url string: none may appear in the HTML (antiforgery fields are not on these pages)
  grep -Eq "$B64RUN" "$T/body"; [ $? -ne 0 ]; check "A3 $path HTML holds no 43-character token-like string" $? "$(token_like)"
  req "$U" "$WEB$path"
  [ "$CODE" = 302 ] && [[ "$LOC" == "/signin?returnUrl=%2F"* ]]; check "A4 unauthenticated $path redirects to /signin with a local returnUrl" $? "$CODE $LOC"
done
req "$M" "$WEB/admin/users"
[ "$CODE" = 403 ] && grep -q "Not authorised" "$T/body" && ! grep -q "<h1>Users</h1>" "$T/body"; check "A5 non-admin /admin/users: 403 'Not authorised'" $? "$CODE"
req "$N" "$WEB/admin/users"
[ "$CODE" = 200 ] && grep -q "<h1>Users</h1>" "$T/body"; check "A6 admin /admin/users: 200" $? "$CODE"
grep -Eqi '^cache-control: .*no-store' "$T/hdr"; check "A7 /admin/users carries Cache-Control: no-store" $?
grep -Eq "$B64RUN" "$T/body"; [ $? -ne 0 ]; check "A8 /admin/users HTML holds no 43-character token-like string" $? "$(token_like)"
req "$U" "$WEB/admin/users"
[ "$CODE" = 302 ] && [[ "$LOC" == "/signin?returnUrl="* ]]; check "A9 unauthenticated /admin/users redirects to /signin" $? "$CODE $LOC"
req "$M" "$WEB/account"
grep -q 'href="/admin/users"' "$T/body"; [ $? -ne 0 ]; check "A10 non-admin navigation has no Users link" $?
req "$N" "$WEB/account"
grep -q 'href="/admin/users"' "$T/body"; check "A11 admin navigation has the Users link" $?

echo
if [ "$FAILS" -eq 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; echo "--- web log tail"; tail -15 "$T/web.log"; fi
exit "$FAILS"
