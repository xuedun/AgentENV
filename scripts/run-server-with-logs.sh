#!/usr/bin/env bash
set -euo pipefail
exec scripts/run-with-capabilities.sh ./target/release/server 2>/var/lib/aenv/logs/server.log
