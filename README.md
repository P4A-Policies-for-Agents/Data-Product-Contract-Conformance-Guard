# Data Product Contract Conformance Guard — MuleSoft Omni/Flex Gateway Policy

An **inbound, body-inspecting** custom policy for the MuleSoft Omni/Flex Gateway
that **derives a data product's field contract live from Informatica CDGC** and
checks each response against it, acting **per drift type** — `off | log | inform
| strip | reject`. It protects an agent from **contract-breaking responses**:
ungoverned fields, missing required fields, type mismatches, and **sensitive-field
leaks**.

You configure it with **just one id** — a **CDGC asset id** for the scanned
schema (a flat file, table, etc.). Everything else (the field set, datatypes,
required flags, sensitivity, business-term vocabulary) is **derived from CDGC at
runtime** and cached. No per-field configuration.

Built with the PDK, Rust → `wasm32-wasip1`, split-model. Works on **MCP**
(`tools/call`), **A2A**, and **REST/HTTP** JSON responses.

---

## How the contract is derived (catalog-driven)

On a cache miss the policy authenticates to IDMC (**Login → JWT**) and then, via
the CDGC search API **`POST cdgc-api…/ccgf-searchv2/api/v1/search`** (Elasticsearch
DSL, `X-INFA-SEARCH-LANGUAGE: elasticsearch`):

1. **Resolve the schema asset** (`core.identity = schemaId`) → its `core.location`, name, external id.
2. **Enumerate its columns** — `FlatField` assets whose `core.location::path_hierarchy.parent` is the file location → field **names + datatypes** (`core.dataType`).
3. **Enumerate column → Business Term links** — `elementType=RELATIONSHIP`, `type=IClassTechnicalGlossaryBase`, `core.sourceIdentity ∈ columns`.
4. **Resolve the linked terms** → `core.name`, `core.description` (vocabulary), `isCDE` (**required**).
5. **Build the contract**, one field per column: `name`, `type`, `required` (term `isCDE`), `sensitive` (term description contains `sensitiveMarker`, e.g. *"Confidential…"*), and the governing term.

The result is cached in PDK DataStorage (lazy refresh, single-flight, `distributed`
for cross-replica). Credentials are `security:sensitive`; CDGC response bodies are
never logged.

This is exactly the CDGC governance graph: **catalog source → scanned table →
columns → Business Terms** — no contract duplicated into config.

---

## How it decides — per drift type

For each record at `recordsPath`:

| Drift | Meaning | Default action |
|---|---|---|
| **unexpected** | response field not among the governed columns (e.g. `internal_margin`) | `strip` |
| **missingRequired** | a `required` (isCDE) governed field absent (e.g. `sku`) | `reject` |
| **typeMismatch** | field JSON type ≠ governed `core.dataType` | `inform` |
| **sensitive** | a governed field whose term is marked confidential appears (e.g. `unit_cost`) | `strip` |

Precedence **reject > strip > inform > log**. The outcome rides in a `_contract`
annotation in the payload (the guard rewrites the body, self-describing + enforcing
from one CDGC fetch):

```json
"_contract": { "status":"repaired|drift|ok", "name":"dim_product.csv",
  "assetId":"…", "externalId":"…", "drift":"!unit_cost,+internal_margin", "source":"cdgc" }
```
Markers: `+` unexpected · `-` missing-required · `~` type-mismatch · `!` sensitive.
**reject** replaces the result with a JSON-RPC `-32052` contract-violation error.

Two behaviors worth calling out:
- **A `reject` short-circuits everything** — no body is returned, so strips that
  would otherwise apply (e.g. a sensitive field) never happen; the whole response fails closed.
- **A field that is both required and sensitive is stripped when present** — the
  missing-required rule only fires on *absence*, so on the outbound edge the leak
  guard wins and a required-but-confidential value is removed before it reaches the agent.

See [`demo/WALKTHROUGH.md`](demo/WALKTHROUGH.md) for a field-by-field trace of both
demo variants (raw upstream → contract → guarded output).

---

## Live demo (verified against the real governed `dim_product.csv`)

```
── get_products(variant=leak) ──
  governed asset : dim_product.csv  (assetId cb2345f7-…)
  outcome        : status=repaired  drift=!unit_cost,+internal_margin
  data           : [{brand, category, department, is_sellable, launch_date,
                     lifecycle_state, list_price, product_name, sku, subcategory}]   ← unit_cost + internal_margin stripped

── get_products(variant=broken) ──
  REJECTED       : -32052  (!unit_cost,-sku)      ← sku (required) missing
```

Config for that run was **only** `dim_product.csv`'s schema-asset id; the field
set, `sku`'s required flag, and `unit_cost`'s sensitivity all came from CDGC.
Run: `cp demo/config.json.example demo/config.json` (fill id/creds) →
provision per [`demo/PROVISION.md`](demo/PROVISION.md) → `./demo/demo.sh`.
[`demo/WALKTHROUGH.md`](demo/WALKTHROUGH.md) traces both variants field by field —
why `leak` is repaired and `broken` is rejected.

---

## Configuration reference

| Property | Type | Default | Description |
|---|---|---|---|
| `cdgcLoginUrl` | string (service) | required | IDMC login base URL. |
| `cdgcSearchUrl` | string (service) | required | CDGC search host (serves `ccgf-searchv2`), e.g. `https://cdgc-api.<pod>.informaticacloud.com`. |
| `cdgcOrgUsername` / `cdgcOrgPassword` | string (sensitive) | required | IDMC read-only service account. |
| `schemaId` | string | required | CDGC asset id of the scanned schema (flat file, table, etc.) whose columns define the contract. |
| `schemaIdHeader` | string | `x-dp-schema-id` | Per-request schema-asset id override. |
| `recordsPath` | string | `""` | `/`-path to the record(s) checked (`products`); array = each element. |
| `sensitiveMarker` | string | `confidential` | Case-insensitive substring in a field's term description that marks it sensitive. |
| `onUnexpectedField` / `onMissingRequired` / `onTypeMismatch` / `onSensitiveField` | enum | `strip`/`reject`/`inform`/`strip` | Per-drift-type action (`off\|log\|inform\|strip\|reject`). |
| `refreshIntervalSeconds` | integer | `86400` | Contract cache TTL. |
| `failOpenOnCdgcError` | boolean | `true` | Serve last-known-good contract on transient CDGC error; no contract → pass through. |
| `distributed` | boolean | `false` | Share cache + refresh lock across replicas. |
| `timeout` | integer (ms) | `5000` | Per-CDGC-call timeout (≤ ~15s chained budget across the 6 calls). |

---

## Repository layout

```
contract-conformance-guard-definition/   # gcl.yaml, exchange.json, Makefile
contract-conformance-guard-flex/          # Rust implementation
  src/lib.rs          # CDGC auth + ccgf-searchv2 contract derivation + cache-aside + body strip/reject
  src/conformance.rs  # PURE: drift analysis + per-type decision + strip — 10 unit tests
  src/cdgc.rs         # PURE: nonce + cached types
demo/  # dim_product-shaped mock, config (schemaId), agent (leak/broken), combined/, PROVISION, WALKTHROUGH
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
Published at **1.0.5** (1.0.0–1.0.2 used a description-block contract; 1.0.3+ is
the catalog-driven model; 1.0.5 drops `catalogId` and renames `flatFileId`→`schemaId`).
Requires **PDK 1.10**.

---

## Caveats & scope

- **Requires an MCC scan** so the schema asset has governed columns (and, for
  required/sensitive/vocabulary, columns linked to Business Terms). Unlinked
  columns still contribute their name + datatype to the contract.
- **Body-inspecting** → JSON / single-message-SSE `tools/call` results; whole-stream
  SSE rewrites are out of scope (event-local rewrite is future work).
- **Reject is fail-closed** (per drift type); the guard is fail-open on its own
  CDGC outage.
- Calls the `ccgf-searchv2` API on `cdgc-api` (a `format:service` egress) — the
  gateway must reach `*.informaticacloud.com`.

---

## Skills used

- **PDK** (`omni-gateway-pdk-skills`): `pdk-create-policy`, `pdk-mcp`,
  `pdk-request-headers-bodies`, `pdk-data-storage`, `pdk-distributed-cache-gossip`,
  `pdk-schema-definition`, `pdk-unit-tests`.
- **P4A** (`p4a-skills`): `p4a-build-policy`, `p4a-verify-requirements`,
  `p4a-mcp-usage`, `p4a-test-mcp-policies-with-a2d`.
- **IDMC** (`governed-data-product-skills`, `IDMC - Data Governance Skills`):
  CDGC content + `ccgf-searchv2` search graph, MCC-scanned columns, Business Terms.
