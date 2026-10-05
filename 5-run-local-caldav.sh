#!/bin/sh
# Starts caldav-bridge with the home-network config (.env + caldav-bridge/config.local.toml).
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
# The bridge speaks to the three services with their service secrets, which .env already holds.
export ME_CALDAV__AUTH_SECRET="$ME_AUTH__SERVICE_SECRET"
export ME_CALDAV__CALENDAR_SECRET="$ME_CALENDAR__SERVICE_SECRET"
export ME_CALDAV__TASKS_SECRET="$ME_TASKS__SERVICE_SECRET"
# Also log to ./logs, a file per day, unless .env names another directory.
export ME_CALDAV__LOG__DIR="${ME_CALDAV__LOG__DIR:-$PWD/logs}"
exec cargo run --release -p caldav-bridge -- caldav-bridge/config.local.toml
