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

echo
if [ "$FAILS" -eq 0 ]; then echo "ALL PASSED"; else echo "$FAILS FAILED"; echo "--- web log tail"; tail -15 "$T/web.log"; fi
exit "$FAILS"
