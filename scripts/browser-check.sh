#!/usr/bin/env bash
# Real-browser check: starts auth-service (temp data dir) and auth-web, then drives Google Chrome through
# scripts/browser-check/check.mjs. Needs Node and Chrome; run `npm install` in scripts/browser-check once.
# Screenshots go to $SCREENS (default: .superpowers/sdd/2026-10-03-auth/screens, git-ignored).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
SVC=http://127.0.0.1:8081
WEB=http://localhost:5080
PROXY_PORT=8082
T="$(mktemp -d)"
PIDS=()

cleanup() {
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill -- "-$p" 2>/dev/null; done
  sleep 1
  rm -rf "$T"
}
trap cleanup EXIT

[ -d scripts/browser-check/node_modules/playwright-core ] || { echo "run: (cd scripts/browser-check && npm install)"; exit 2; }
for port in 8081 "$PROXY_PORT" 5080; do
  if curl -s -o /dev/null "http://127.0.0.1:$port/"; then echo "port $port already in use"; exit 2; fi
done

echo "== building"
cargo build -q -p auth-service || { echo "cargo build failed"; exit 2; }
dotnet build auth-web -v q --nologo 2>&1 | tail -3
[ "${PIPESTATUS[0]}" -eq 0 ] || { echo "dotnet build failed"; exit 2; }

echo "== starting auth-service and auth-web"
# All three providers are configured (with dummy credentials) so the sign-in page shows their buttons.
cp auth-service/config.example.toml "$T/config.toml"
for p in google github microsoft; do printf '\n[providers.%s]\nclient_id = "dummy"\nclient_secret = "dummy"\n' "$p" >>"$T/config.toml"; done
ME_AUTH__DATA_DIR="$T/data" ME_AUTH__LOG__FORMAT=json setsid ./target/debug/auth-service "$T/config.toml" >"$T/svc.log" 2>&1 &
PIDS+=("$!")
# auth-web reaches the service through a recording proxy that check.mjs runs, so the check can see the
# headers auth-web sends on calls made from a circuit.
ASPNETCORE_ENVIRONMENT=Development ASPNETCORE_URLS=$WEB LOG_FORMAT=json DataProtection__Path="$T/dp" \
  AuthService__BaseUrl="http://127.0.0.1:$PROXY_PORT" \
  setsid dotnet run --no-build --no-launch-profile --project auth-web >"$T/web.log" 2>&1 &
PIDS+=("$!")

for i in $(seq 60); do curl -sf "$SVC/health" >/dev/null && break; sleep 1; done
for i in $(seq 60); do curl -sf -o /dev/null "$WEB/app.css" && break; sleep 1; done
curl -sf "$SVC/health" >/dev/null || { echo "auth-service did not start"; cat "$T/svc.log"; exit 2; }
curl -sf -o /dev/null "$WEB/app.css" || { echo "auth-web did not start"; cat "$T/web.log"; exit 2; }
SEED=$(grep -o '"one_time_password":"[^"]*"' "$T/svc.log" | head -1 | cut -d'"' -f4)
[ -n "$SEED" ] || { echo "no seed password in service log"; exit 2; }

echo "== checks"
WEB="$WEB" SVC="$SVC" PROXY_PORT="$PROXY_PORT" SEED="$SEED" WEB_LOG="$T/web.log" \
  SCREENS="${SCREENS:-$ROOT/.superpowers/sdd/2026-10-03-auth/screens}" \
  node scripts/browser-check/check.mjs
RC=$?
if [ "$RC" -ne 0 ]; then echo "--- web log tail"; tail -15 "$T/web.log"; fi
exit "$RC"
