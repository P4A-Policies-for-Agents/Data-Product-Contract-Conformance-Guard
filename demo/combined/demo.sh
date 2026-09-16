#!/usr/bin/env bash
set -uo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
[ -f "$DIR/env.local.sh" ] && . "$DIR/env.local.sh"
: "${TDF_GW_URL:?Set TDF_GW_URL (see env.local.sh.example)}"
python3 "$DIR/agent.py"
