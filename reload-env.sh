#!/bin/bash
# Recreates the bot container from the already-built image, picking up the current .env file -
# no Rust rebuild needed. Use this after changing ONLY environment variables (e.g. TAX_*,
# PVP_*, GROWTH_*); if you changed any .rs/.yml source file, you need a full rebuild instead.
set -euo pipefail
cd "$(dirname "$0")"
docker compose up -d --no-deps DickGrowerBot
echo "Done - the container restarted with the current .env values (no rebuild)."
