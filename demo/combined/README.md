# Combined demo — trusted data foundation (one policy)

This demonstrates a data-product response that is **both self-describing and
contract-enforced**, sourced live from Informatica CDGC — delivered by the
**Contract Conformance Guard alone** (one CDGC fetch does both):

- **self-describe** — the response carries `_contract.name` / `.externalId` /
  `.assetId` (the governed identity), and
- **enforce** — drift is stripped or rejected (`_contract.status` = repaired /
  the call is rejected).

## Why one policy, not two

We first tried composing two policies (Contract Metadata Injection for identity
headers + Conformance Guard for enforcement) on one instance. That failed: the
metadata policy stamps response **headers** after a CDGC fetch on the response
leg, which races the streamed response-head commit and 500s on a cold instance
(and when composed). The Conformance Guard **buffers the response body** while it
fetches CDGC, so its fetch is robust — and since it already reads the asset's
`summary`, it can emit the identity too, from the **same** fetch. Hence the
identity is folded into the guard: one policy, one fetch, both behaviors, robust
on a fresh instance. (The standalone Metadata Injection policy remains useful for
the header-only, enrichment-only case on warm/non-streaming paths.)

## Run

```bash
# The Conformance Guard (>=1.0.2) is applied to the /conformance-demo/ instance
# (see ../PROVISION.md). Then:
cp env.local.sh.example env.local.sh   # set TDF_GW_URL to the governed endpoint
./demo.sh
```

Expected: `variant=leak` → identity stamped + `internalMargin`/`customerEmail`
stripped (`status: repaired`); `variant=broken` → rejected (JSON-RPC -32052).
