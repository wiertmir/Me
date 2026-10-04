#!/bin/sh
# Publishes auth-web and starts it in Production mode behind Caddy (settings from .env).
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
dotnet publish auth-web -c Release -o auth-web/bin/publish
cd auth-web/bin/publish
exec dotnet AuthWeb.dll
