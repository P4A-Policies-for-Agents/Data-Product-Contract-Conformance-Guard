#!/usr/bin/env bash
set -uo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
[ -f "$DIR/env.local.sh" ] && . "$DIR/env.local.sh"
: "${CMP_GW_URL:?Set CMP_GW_URL (see demo/env.local.sh.example)}"
python3 "$DIR/agent.py"
