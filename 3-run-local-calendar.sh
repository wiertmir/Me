#!/bin/sh
# Starts calendar-service with the home-network config (.env + calendar-service/config.local.toml).
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
# Also log to ./logs, a file per day, unless .env names another directory.
export ME_CALENDAR__LOG__DIR="${ME_CALENDAR__LOG__DIR:-$PWD/logs}"
exec cargo run --release -p calendar-service -- calendar-service/config.local.toml
