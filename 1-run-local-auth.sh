#!/bin/sh
# Starts auth-service with the home-network config (.env + auth-service/config.local.toml).
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
exec cargo run --release -p auth-service -- auth-service/config.local.toml
