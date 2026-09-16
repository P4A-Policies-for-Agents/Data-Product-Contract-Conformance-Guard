#!/usr/bin/env python3
"""
Combined "trusted data foundation" demo — ONE robust policy that both
self-describes and enforces, from a single CDGC fetch.

The Contract Conformance Guard reads the governed asset from Informatica CDGC and,
on each response:
  * SELF-DESCRIBES  — stamps the contract identity (name, externalId, assetId) into
    a `_contract` annotation, sourced live from CDGC.
  * ENFORCES        — checks the record fields against the governed contract and
    strips / rejects drift (unexpected, missing-required, type, sensitive).

(Design note: this folds the metadata-injection identity into the guard because a
separate response-header enrichment policy that fetches CDGC is unreliable on a
cold/streaming response — the guard buffers the body, so its CDGC fetch is robust.
One policy, one fetch, both behaviors — see the repo README.)

Usage:
    TDF_GW_URL="https://<host>/conformance-demo/mcp" python3 agent.py
"""
import json, os, ssl, sys, urllib.request

GW = (sys.argv[1] if len(sys.argv) > 1 else os.environ.get("TDF_GW_URL", "")).strip()
if not GW:
    sys.exit("Set TDF_GW_URL (governed endpoint). See env.local.sh.example")
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE


def call(variant):
    body = {"jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": "get_products", "arguments": {"variant": variant}}}
    req = urllib.request.Request(GW, data=json.dumps(body).encode(), method="POST", headers={
        "Content-Type": "application/json", "Accept": "application/json, text/event-stream",
        "Accept-Encoding": "identity", "mcp-session-id": "tdf-demo"})
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
    payload = call(variant)
    print(f"── get_products(variant={variant}) ──")
    if "_contract" in payload:
        c = payload["_contract"]
        print(f"  self-describe : name={c.get('name')} externalId={c.get('externalId')} assetId={c.get('assetId')}")
        print(f"  enforce       : status={c.get('status')} drift={c.get('drift') or '(none)'}")
        print(f"  data          : {payload.get('products')}")
    elif "REJECTED" in payload:
        print(f"  REJECTED      : {payload['REJECTED']}")
    else:
        print(f"  {payload}")
    print()


def main():
    print(f"🧱 trusted data foundation — one policy: self-describe + enforce  →  {GW}\n")
    show("leak")    # identity stamped + internalMargin/customerEmail stripped → repaired
    show("broken")  # missing currency + wrong type → rejected
    print("One CDGC-governed asset, one fetch: the response carries its contract")
    print("identity AND is repaired/rejected to conform — sourced live from Informatica CDGC.")


if __name__ == "__main__":
    main()
