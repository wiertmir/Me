#!/bin/sh
# Starts Caddy on https://$ME_HOST (from .env) in front of both services.
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
exec caddy run
