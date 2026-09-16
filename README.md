# Data Product Contract Conformance Guard — MuleSoft Omni/Flex Gateway Policy

An **inbound, body-inspecting** custom policy for the MuleSoft Omni/Flex Gateway
that checks each data-product response against its **CDGC-governed field contract**
in Informatica IDMC and acts **per drift type** — `off | log | inform | strip |
reject`. It protects an agent from **contract-breaking responses**: ungoverned
fields, missing required fields, type mismatches, and **sensitive-field leaks**.

Built with the PDK, Rust → `wasm32-wasip1`, split-model. Works on **MCP**
(`tools/call`), **A2A**, and **REST/HTTP** JSON responses.

This is the enforcing sibling of the **Contract Metadata Injection** policy: that
one *describes* the data product (stamps contract identity as headers); this one
*enforces* that the data product's responses actually conform to the governed
contract.

---

## The governed contract (Business Terms + field schema)

The contract is a JSON array carried on the CDGC asset, one entry per field:

```json
[{"name":"orderId","type":"string","required":true,"term":"Order Id"},
 {"name":"total","type":"number","required":true,"term":"Order Total"},
 {"name":"currency","type":"string","required":true,"term":"Currency Code"},
 {"name":"customerEmail","type":"string","required":false,"sensitive":true,"term":"Customer Email"}]
```

- Each field references a **governed Business Term** (`term`) — the terms exist as
  first-class CDGC glossary assets (the governed vocabulary, with `FormatType` and
  `isCDE`); this block is their per-field projection onto the DataSet.
- Stored on the asset's governed **description** behind the `contractMarker`
  (`contract-fields=`). *(CDGC does not allow linking terms to a DataSet, nor
  enumerating scanned columns via API, and custom attributes need pre-definition —
  the description block is the reliable, API-readable carrier. A defined custom
  attribute is the productionization.)*

The policy fetches it via the CDGC `Login → JWT → data360` chain (`format:service`
egress + injected `HttpClient`, exactly like the metadata policy) and **caches**
it (lazy refresh, single-flight, `distributed` for cross-replica).

---

## How it decides — per drift type

For each record at `recordsPath` in the response payload:

| Drift | Meaning | Default action |
|---|---|---|
| **unexpected** | response field not in the contract (e.g. `internalMargin`) | `strip` |
| **missingRequired** | a `required` contract field absent | `reject` |
| **typeMismatch** | field's JSON type ≠ contract `type` (e.g. `total:"NINETY"`) | `inform` |
| **sensitive** | a `sensitive` contract field present (e.g. `customerEmail`) | `strip` |

Actions (`off | log | inform | strip | reject`), applied with precedence
**reject > strip > inform > log**:
- **reject** — replace the whole result with a JSON-RPC contract-violation error (`-32052`); contract-breaking data never reaches the agent.
- **strip** — remove the offending fields from every record.
- **inform** — annotate the outcome (no removal).
- **log** — emit a structured drift record to the gateway logs.
- **off** — ignore that drift type.

The outcome rides in a **`_contract` annotation in the response payload** (the
guard rewrites the body, so a transport header derived from body content isn't
possible on the split response flow). It **self-describes and enforces from a
single CDGC fetch** — the same asset-detail call yields the identity (name,
externalId) and the field contract:

```json
"_contract": {
  "status": "repaired|drift|ok",
  "name": "Sales Orders", "externalId": "DS-14", "assetId": "...",
  "drift": "!customerEmail,+internalMargin", "source": "cdgc"
}
```

> **Trusted data foundation, one policy.** This folds the *identity* half of the
> Contract Metadata Injection story into the guard: because the guard already
> buffers the body and fetches the governed asset, it emits the contract identity
> **and** enforces the field contract in one robust pass — no second policy, no
> second CDGC call. (Composing a separate header-stamping metadata policy on the
> same instance is unreliable: its response-leg fetch races the streamed
> response-head commit. See `demo/combined/README.md`.)

Drift markers: `+` unexpected · `-` missing-required · `~` type-mismatch · `!` sensitive.

---

## Live demo (verified against a real IDMC tenant)

A Sales Orders upstream that has **drifted** from its governed contract:

```
── variant=leak  (ungoverned internalMargin + sensitive customerEmail; all required present) ──
  DIRECT mock : {orders:[{orderId,total,currency,internalMargin,customerEmail}, …]}
  GATEWAY     : {orders:[{orderId,total,currency}, …],
                 _contract:{status:"repaired", drift:"!customerEmail,+internalMargin"}}   ← leak stripped

── variant=broken (missing required currency; total is a string) ──
  GATEWAY     : JSON-RPC error -32052 "response violated the governed contract … (~total,-currency)"  ← rejected
```

Same upstream, same request: **a silent data leak / broken payload without the
gateway, and a repaired-or-rejected, contract-conformant response through it.**

```bash
cp demo/config.json.example demo/config.json   # fill IDMC creds/url/assetId
# provision per demo/PROVISION.md, then:
cp demo/env.local.sh.example demo/env.local.sh # set CMP_GW_URL
./demo/demo.sh
```

---

## Configuration reference

| Property | Type | Default | Description |
|---|---|---|---|
| `cdgcLoginUrl` / `cdgcBaseApiUrl` | string (service) | required | IDMC login + CDGC API hosts (`format:service` egress). |
| `cdgcOrgUsername` / `cdgcOrgPassword` | string (sensitive) | required | IDMC read-only service account. |
| `cdgcAssetId` | string | required | CDGC asset whose contract is enforced. |
| `assetIdHeader` | string | `x-dp-contract-id` | Per-request asset-id override. |
| `contractMarker` | string | `contract-fields=` | Marker preceding the contract JSON in the asset description. |
| `recordsPath` | string | `""` | `/`-path to the record(s) whose fields are checked (`orders`); empty = payload root; array = each element. |
| `onUnexpectedField` | enum | `strip` | Action for ungoverned fields. |
| `onMissingRequired` | enum | `reject` | Action for missing required fields. |
| `onTypeMismatch` | enum | `inform` | Action for type mismatches. |
| `onSensitiveField` | enum | `strip` | Action for sensitive-field leaks. |
| `refreshIntervalSeconds` | integer | `86400` | Contract cache TTL. |
| `failOpenOnCdgcError` | boolean | `true` | Serve last-known-good contract on transient CDGC error; with no contract at all, pass through (never block on its own outage). |
| `distributed` | boolean | `false` | Share cache + refresh lock across replicas. |
| `timeout` | integer (ms) | `5000` | Per-CDGC-call timeout (≤ ~10s chain budget). |

---

## Repository layout

```
contract-conformance-guard-definition/   # gcl.yaml, exchange.json, Makefile
contract-conformance-guard-flex/          # Rust implementation
  src/lib.rs          # CDGC fetch + cache-aside + response body inspect/strip/reject
  src/conformance.rs  # PURE: contract parse, drift analysis, per-type decision, strip — 10 unit tests
  src/cdgc.rs         # PURE: request-target encoding, nonce, cached types
  src/generated/      # config.rs (Service types + service_create)
demo/  # drifted-upstream mock, config, agent (before/after), PROVISION
```

---

## Build, test & release

```bash
cd contract-conformance-guard-definition && make release
cd ../contract-conformance-guard-flex
make build-asset-files && cargo build --target wasm32-wasip1 --release
cargo test --lib            # 10 pure unit tests
make release
```

Published at **1.0.1**. Requires **PDK 1.10** (`HttpClient`, `format:service`,
`Clock`, DataStorage CAS).

---

## Caveats & scope

- **Body-inspecting** → heavier than a header policy, and applies to **JSON /
  single-message SSE `tools/call` results**. Whole-stream (token-by-token) SSE
  rewrites are out of scope (event-local rewrite is future work).
- **Reject is fail-closed** — a deliberate config choice per drift type. The guard
  itself is fail-open on its **own** outage (no contract resolvable → pass through).
- Contract fidelity depends on the governed field list you publish. Governed
  scanned columns (types + classification from MCC) are a richer future source;
  today the description-block contract (referencing governed terms) is the
  API-reliable carrier.
- Composes **after** an auth policy and (ideally) alongside **Contract Metadata
  Injection** (which shares the CDGC client).

---

## Skills used

- **PDK** (`omni-gateway-pdk-skills`): `pdk-create-policy`, `pdk-mcp`,
  `pdk-request-headers-bodies`, `pdk-data-storage`, `pdk-distributed-cache-gossip`,
  `pdk-sse-parsing`, `pdk-schema-definition`, `pdk-unit-tests`.
- **P4A** (`p4a-skills`): `p4a-build-policy`, `p4a-verify-requirements`,
  `p4a-mcp-usage`, `p4a-test-mcp-policies-with-a2d`.
- **IDMC** (`governed-data-product-skills`, `IDMC - Data Governance Skills`):
  `cdgc-publishing` / `infa-cdgc-publish` (content API, term + asset model).
