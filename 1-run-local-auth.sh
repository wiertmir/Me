#!/bin/sh
# Starts auth-service with the home-network config (.env + auth-service/config.local.toml).
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
# Also log to ./logs, a file per day, unless .env names another directory.
export ME_AUTH__LOG__DIR="${ME_AUTH__LOG__DIR:-$PWD/logs}"
exec cargo run --release -p auth-service -- auth-service/config.local.toml
