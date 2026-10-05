#!/bin/sh
# Starts tasks-service with the home-network config (.env + tasks-service/config.local.toml).
set -e
cd "$(dirname "$0")"
set -a; . ./.env; set +a
# Also log to ./logs, a file per day, unless .env names another directory.
export ME_TASKS__LOG__DIR="${ME_TASKS__LOG__DIR:-$PWD/logs}"
exec cargo run --release -p tasks-service -- tasks-service/config.local.toml
