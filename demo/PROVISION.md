# Demo provisioning runbook (catalog-driven)

Stands up the live catalog-driven Contract Conformance Guard demo with
`anypoint-cli-v4` + the A2D MCP tools. The contract is derived from CDGC, so you
need a real IDMC tenant with a **scanned schema asset** (a flat file, table, etc.
from an MCC scan) whose columns are (ideally) linked to Business Terms.

| Placeholder | What it is |
|---|---|
| `<orgId>` | Business-group / org id |
| `<mockServerId>` | A2D product-mock MCP server id |
| `<gatewayId>` | Managed Flex Gateway with a public ingress |
| `<gatewayPublicHost>` | Gateway public ingress URL |
| `<apiInstanceId>` | API Manager instance id |
| `<schemaId>` | Scanned schema asset (flat file, table, etc.) `core.identity` |

Governed endpoint: `https://<gatewayPublicHost>/catalog-conformance-demo/mcp`

## 0. Find the schema-asset id (CDGC search API)

```bash
# list scanned flat files (name + id + location)
curl -s -X POST "https://cdgc-api.<pod>.informaticacloud.com/ccgf-searchv2/api/v1/search" \
  -H "Authorization: Bearer <jwt>" -H "X-INFA-ORG-ID: <orgId>" \
  -H "X-INFA-SEARCH-LANGUAGE: elasticsearch" -H "Content-Type: application/json" \
  -d '{"from":0,"size":25,"query":{"bool":{"must":[{"terms":{"core.classType":["com.infa.odin.models.file.flat.FlatFile"]}}]}}}'
```
`core.identity` → `schemaId`. (Get `<jwt>` via `/identity-service/api/v1/Login`
then `/jwt/Token` — the same chain the policy uses.)

## 1. A2D mock (records mirroring the governed columns)

`design_mcp_server` (mock) + `add_mcp_tool get_products` with two scenarios keyed
on `variant`: `leak` (all governed columns + an ungoverned field + a sensitive
column present) and `broken` (missing a required column).

## 2. Publish + deploy the MCP Flex instance

```bash
anypoint-cli-v4 exchange:asset:upload --name "Product Catalog Data Product" \
  --type mcp --status published --properties='{"platform":"a2d"}' \
  --files='{"mcp-metadata.json":"./mcp-metadata.json"}' product-catalog-data-product/1.0.0
anypoint-cli-v4 api-mgr:api:manage product-catalog-data-product 1.0.0 <orgId> \
  --environment Sandbox --isFlex --type mcp \
  --uri "https://www.a2d-ai.com/api/platform/<mockServerId>/" --apiInstanceLabel catalog-conformance-demo
anypoint-cli-v4 api-mgr:api:edit <apiInstanceId> --environment Sandbox --isFlex --type mcp \
  --withProxy --scheme http --port 8081 --path "/catalog-conformance-demo/" \
  --uri "https://www.a2d-ai.com/api/platform/<mockServerId>/"
anypoint-cli-v4 api-mgr:api:deploy <apiInstanceId> --environment Sandbox \
  --target <gatewayId> --gatewayVersion 1.0.0 --overwrite
```

## 3. Apply the guard (config = one id + creds)

```bash
cp config.json.example config.json   # fill cdgc creds/urls + schemaId
anypoint-cli-v4 api-mgr:policy:apply <apiInstanceId> contract-conformance-guard \
  --environment Sandbox --groupId <orgId> --policyVersion 1.0.5 --configFile ./config.json
anypoint-cli-v4 api-mgr:api:redeploy <apiInstanceId> --environment Sandbox
```

## 4. Run

```bash
cp env.local.sh.example env.local.sh   # set CMP_GW_URL
./demo.sh
```

Expected: `variant=leak` → ungoverned + sensitive fields stripped, `_contract.status=repaired`;
`variant=broken` → JSON-RPC `-32052` (missing required column).

## Notes
- Egress: the guard calls `cdgcLoginUrl` + `cdgcSearchUrl` (both `format:service`);
  confirm the gateway can reach `*.informaticacloud.com`.
- `recordsPath=products` because records live under `products` in the payload.
- Sensitivity comes from a field's Business Term description containing the
  `sensitiveMarker` (default "confidential"); required comes from the term's `isCDE`.
