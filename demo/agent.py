#!/usr/bin/env python3
"""
Agent simulation for the Data Product Contract Conformance Guard demo.

The Sales Orders upstream has DRIFTED from its CDGC-governed contract. An agent
calls get_orders twice:
  * variant=leak   — records carry an ungoverned `internalMargin` and a sensitive
                     `customerEmail` (all required fields present).
  * variant=broken — a record missing required `currency`, with `total` a string.

Set CMP_GW_URL to the direct A2D mock to see the raw (drifted) data, or to the
governed gateway endpoint to see the guard strip / reject per the contract.

Usage:
    CMP_GW_URL="https://<host>/conformance-demo/mcp" python3 agent.py
"""
import json, os, ssl, sys, urllib.request

GW = (sys.argv[1] if len(sys.argv) > 1 else os.environ.get("CMP_GW_URL", "")).strip()
if not GW:
    sys.exit("Set CMP_GW_URL (governed endpoint or direct mock). See demo/env.local.sh.example")
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE


def call(variant):
    body = {"jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": {"name": "get_orders", "arguments": {"variant": variant}}}
    req = urllib.request.Request(GW, data=json.dumps(body).encode(), method="POST", headers={
        "Content-Type": "application/json", "Accept": "application/json, text/event-stream",
        "Accept-Encoding": "identity", "mcp-session-id": "conformance-demo"})
    try:
        raw = urllib.request.urlopen(req, timeout=25, context=_CTX).read().decode()
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
    for line in raw.splitlines():
        if line.startswith("data:"):
            raw = line[len("data:"):].strip(); break
    try:
        rpc = json.loads(raw)
    except Exception:
        return {"raw": raw}
    if "error" in rpc:
        return {"REJECTED": rpc["error"]}
    res = rpc.get("result", {})
    try:
        return json.loads(res["content"][0]["text"])
    except Exception:
        return res.get("structuredContent", res)


def main():
    print(f"🛡️  contract-conformance agent  →  {GW}\n")
    print("── variant=leak (ungoverned internalMargin + sensitive customerEmail; all required present) ──")
    print(json.dumps(call("leak"), indent=2))
    print("\n── variant=broken (missing required currency; total is a string) ──")
    print(json.dumps(call("broken"), indent=2))
    print("\nAgainst the DIRECT mock you'll see the raw drifted rows. Through the GATEWAY:")
    print("  leak   → internalMargin + customerEmail stripped, _contract.status=repaired")
    print("  broken → whole result rejected (JSON-RPC contract-violation error)")


if __name__ == "__main__":
    main()
