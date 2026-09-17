#!/usr/bin/env python3
"""
Agent simulation for the catalog-driven Contract Conformance Guard demo.

The guard is configured with only a CDGC schema-asset id. At
runtime it derives the field contract live from Informatica CDGC (the scanned
dim_product.csv columns + their linked Business Terms) and checks each response:

  * variant=leak   — a record with all governed columns PLUS an ungoverned
                     `internal_margin`, and `unit_cost` (whose Business Term is
                     marked Confidential) → both stripped, status=repaired.
  * variant=broken — a record missing the required `sku` column → rejected.

Usage:
    CMP_GW_URL="https://<host>/catalog-conformance-demo/mcp" python3 agent.py
"""
import json, os, ssl, sys, urllib.request

GW = (sys.argv[1] if len(sys.argv) > 1 else os.environ.get("CMP_GW_URL", "")).strip()
if not GW:
    sys.exit("Set CMP_GW_URL (governed endpoint or direct mock). See demo/env.local.sh.example")
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE


def call(variant):
    body = {"jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": "get_products", "arguments": {"variant": variant}}}
    req = urllib.request.Request(GW, data=json.dumps(body).encode(), method="POST", headers={
        "Content-Type": "application/json", "Accept": "application/json, text/event-stream",
        "Accept-Encoding": "identity", "mcp-session-id": "catalog-demo"})
    try:
        raw = urllib.request.urlopen(req, timeout=25, context=_CTX).read().decode()
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
    for line in raw.splitlines():
        if line.startswith("data:"):
            raw = line[len("data:"):].strip(); break
    rpc = json.loads(raw)
    if "error" in rpc:
        return {"REJECTED": rpc["error"]}
    try:
        return json.loads(rpc["result"]["content"][0]["text"])
    except Exception:
        return rpc.get("result", {})


def show(variant):
    p = call(variant)
    print(f"── get_products(variant={variant}) ──")
    if "_contract" in p:
        c = p["_contract"]
        print(f"  governed asset : {c.get('name')}  (assetId {c.get('assetId')})")
        print(f"  outcome        : status={c.get('status')}  drift={c.get('drift') or '(none)'}")
        print(f"  data           : {p.get('products')}")
    elif "REJECTED" in p:
        print(f"  REJECTED       : {p['REJECTED']}")
    else:
        print(f"  {p}")
    print()


def main():
    print(f"🛡️  catalog-driven conformance guard  →  {GW}\n")
    print("Contract derived live from CDGC (dim_product.csv columns + linked Business Terms):\n")
    show("leak")
    show("broken")
    print("The guard was given only a schema-asset id; the field set,")
    print("required flags (isCDE) and sensitivity (term marked Confidential) all came")
    print("from Informatica CDGC. internal_margin (ungoverned) + unit_cost (sensitive)")
    print("stripped; a record missing required sku is rejected.")


if __name__ == "__main__":
    main()
