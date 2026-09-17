# Demo walkthrough — how the guard decides, field by field

This traces the two demo requests (`leak` and `broken`) end to end: the raw
upstream response, the contract the guard derived from CDGC, and exactly why the
guard repairs one and rejects the other. Nothing here is configured per field —
the only policy input is the one `schemaId`.

## The contract (derived live from CDGC — not configured)

On the first call the guard resolves `schemaId = cb2345f7-d54f-4d8b-ba76-9b78b14576c4`
in CDGC, enumerates the scanned asset's columns, follows each column → Business
Term link, and builds this contract for `dim_product.csv` (then caches it, TTL
`refreshIntervalSeconds = 86400`):

- **11 governed columns**: `brand, category, department, is_sellable, launch_date,
  lifecycle_state, list_price, product_name, sku, subcategory, unit_cost`
- **required** (the term's `isCDE = true`): `list_price, sku, unit_cost`
- **sensitive** (the term's description contains `sensitiveMarker` = `confidential`): `unit_cost`

`recordsPath = products` tells the guard to check **each element of the `products`
array** in the tool result.

## Drift types → your configured actions

For every record the guard computes drifts (`analyze_record`) then maps each to
the action you set (`decide`). This demo's config:

| Drift | Marker | What it means | This demo's action |
|---|---|---|---|
| Unexpected | `+` | a response field not among the 11 governed columns | `onUnexpectedField = strip` |
| MissingRequired | `-` | a `required` (isCDE) column absent from the record | `onMissingRequired = reject` |
| TypeMismatch | `~` | a field's JSON type ≠ the governed `core.dataType` | `onTypeMismatch = inform` |
| Sensitive | `!` | a governed field marked confidential is present (leak) | `onSensitiveField = strip` |

**Precedence: `reject > strip > inform > log`.** Any single `reject` drift blocks
the whole response — it short-circuits the body rewrite, so nothing is stripped
or returned; the call fails closed with a JSON-RPC `-32052` error instead.

---

## `leak` → repaired

**Raw upstream** (`products[0]`, 12 fields — the 11 governed ones **plus**
`internal_margin`, and it includes `unit_cost`):

```json
{ "products": [ {
  "department":"Home","category":"Kitchen","is_sellable":"true",
  "launch_date":"2025-01-10","product_name":"Blender X","brand":"Acme",
  "lifecycle_state":"active","subcategory":"Small Appliances",
  "list_price":"99.99","unit_cost":"42.50","sku":"SKU-1001",
  "internal_margin":"0.57" } ], "count": 1 }
```

**Guard analysis**
- `internal_margin` → not a governed column → **Unexpected** `+internal_margin` → **strip**.
- `unit_cost` → present **and** sensitive → **Sensitive** `!unit_cost` → **strip**.
- All required columns (`list_price, sku, unit_cost`) are **present**, so no
  missing-required drift fires. No type mismatches.

No `reject` drift → the guard **repairs**: it removes the two marked fields and
rewrites the body, stamping a `_contract` annotation.

**What the client receives** (10 fields; `unit_cost` + `internal_margin` gone):

```json
{ "_contract": { "assetId":"cb2345f7-…","name":"dim_product.csv",
    "externalId":"b423cc70-…://FileServer/data/csv/dim_product.csv~…FlatFile",
    "source":"cdgc","drift":"!unit_cost,+internal_margin","status":"repaired" },
  "count": 1,
  "products": [ { "brand":"Acme","category":"Kitchen","department":"Home",
    "is_sellable":"true","launch_date":"2025-01-10","lifecycle_state":"active",
    "list_price":"99.99","product_name":"Blender X","sku":"SKU-1001",
    "subcategory":"Small Appliances" } ] }
```

> **Subtlety worth noticing:** `unit_cost` is *both* required and sensitive.
> Because it was **present**, the guard treats it as a leak and strips it — the
> "required" rule only fires when a field is **absent**. On the outbound edge the
> leak guard wins; the guard will happily remove a required-but-confidential value
> before it reaches the agent.

---

## `broken` → rejected

**Raw upstream** (`products[0]`, only 4 fields — `sku` is missing):

```json
{ "products": [ {
  "department":"Home","category":"Kitchen",
  "list_price":"99.99","unit_cost":"42.50" } ], "count": 1 }
```

**Guard analysis**
- `sku` → a **required** column that is **absent** → **MissingRequired** `-sku` → **reject**.
- `unit_cost` → present + sensitive → **Sensitive** `!unit_cost` → would strip.
- The other missing columns (`product_name`, `brand`, …) are governed but **not
  required**, so their absence is fine.

By precedence, the `reject` from `-sku` wins and short-circuits everything — the
`unit_cost` strip is never applied because no body is returned at all.

**What the client receives** (JSON-RPC error, HTTP-level failure to the agent):

```
-32052  response violated the governed contract for asset cb2345f7-… (!unit_cost,-sku)
```

The error message still lists every drift the guard saw (`!unit_cost,-sku`), but
the outcome is rejection because `-sku` maps to `reject`.

---

## The `_contract` annotation (repaired responses)

On any non-rejected response the guard rewrites the tool-result body and adds a
`_contract` object so the decision is auditable in-band:

| Field | Meaning |
|---|---|
| `assetId` | the `schemaId` the contract was derived from |
| `name` | CDGC `core.name` of the asset (`dim_product.csv`) |
| `externalId` | the asset's CDGC external id |
| `source` | `cdgc` — provenance of the contract |
| `drift` | marker string of every drift detected (`+ - ~ !` prefixes) |
| `status` | `repaired` (fields stripped) / `drift` (detected, informed only) / `ok` (clean) |

## Run it yourself

```bash
cp env.local.sh.example env.local.sh   # set CMP_GW_URL to the governed endpoint
./demo.sh                              # or: CMP_GW_URL=… python3 agent.py
```

Provisioning (mock + gateway instance + policy) is in [`PROVISION.md`](./PROVISION.md).
