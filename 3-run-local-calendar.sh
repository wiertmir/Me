#!/bin/sh
# Starts calendar-service with the home-network config (.env + calendar-service/config.local.toml).
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
exec cargo run --release -p calendar-service -- calendar-service/config.local.toml
