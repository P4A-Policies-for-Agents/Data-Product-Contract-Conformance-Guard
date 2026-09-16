# Demo provisioning runbook

Stands up the live Contract Conformance Guard demo with `anypoint-cli-v4` + the
A2D MCP tools. Needs a real IDMC tenant (read-only service account) and a
CDGC asset carrying a `contract-fields=[…]` block in its governed description.

| Placeholder | What it is |
|---|---|
| `<orgId>` | Business-group / org id |
| `<mockServerId>` | A2D drifted-upstream mock MCP server id |
| `<gatewayId>` | Managed Flex Gateway with a public ingress |
| `<gatewayPublicHost>` | Gateway public ingress URL |
| `<apiInstanceId>` | API Manager instance id |

Governed endpoint: `https://<gatewayPublicHost>/conformance-demo/mcp`

## 0. Governed contract in CDGC (one-time)

Create the governed **Business Terms** (Order Id, Order Total, Currency Code,
Customer Email — each with `FormatType` + `isCDE`) and put the field-contract
block on the DataSet's description via the content API:

```
PATCH {cdgcBaseApiUrl}/data360/content/v1/assets/{assetId}?scheme=internal
[ { "operation":"replace","segment":"summary",
    "attributes": { "core.description": "…prose…\n\ncontract-fields=[{\"name\":\"orderId\",\"type\":\"string\",\"required\":true,\"term\":\"Order Id\"},{\"name\":\"total\",\"type\":\"number\",\"required\":true,\"term\":\"Order Total\"},{\"name\":\"currency\",\"type\":\"string\",\"required\":true,\"term\":\"Currency Code\"},{\"name\":\"customerEmail\",\"type\":\"string\",\"required\":false,\"sensitive\":true,\"term\":\"Customer Email\"}]" } } ]
```
(Headers: `Authorization: Bearer <jwt>`, `X-INFA-ORG-ID`, `X-INFA-PRODUCT-ID: CDGC`.)

## 1. A2D drifted-upstream mock

`design_mcp_server` (mock) + `add_mcp_tool get_orders` with two scenarios keyed on
`variant`: `leak` (records with ungoverned `internalMargin` + sensitive
`customerEmail`, all required present) and `broken` (missing `currency`, `total`
as a string).

## 2. Publish + deploy the MCP Flex instance

```bash
anypoint-cli-v4 exchange:asset:upload --name "Sales Orders Drifted Upstream" \
  --type mcp --status published --properties='{"platform":"a2d"}' \
  --files='{"mcp-metadata.json":"./mcp-metadata.json"}' sales-orders-drifted-upstream/1.0.0
anypoint-cli-v4 api-mgr:api:manage sales-orders-drifted-upstream 1.0.0 <orgId> \
  --environment Sandbox --isFlex --type mcp \
  --uri "https://www.a2d-ai.com/api/platform/<mockServerId>/" --apiInstanceLabel conformance-demo
anypoint-cli-v4 api-mgr:api:edit <apiInstanceId> --environment Sandbox --isFlex --type mcp \
  --withProxy --scheme http --port 8081 --path "/conformance-demo/" \
  --uri "https://www.a2d-ai.com/api/platform/<mockServerId>/"
anypoint-cli-v4 api-mgr:api:deploy <apiInstanceId> --environment Sandbox \
  --target <gatewayId> --gatewayVersion 1.0.0 --overwrite
```

## 3. Apply the guard

```bash
cp config.json.example config.json   # fill CDGC creds/url + cdgcAssetId
anypoint-cli-v4 api-mgr:policy:apply <apiInstanceId> contract-conformance-guard \
  --environment Sandbox --groupId <orgId> --policyVersion 1.0.1 --configFile ./config.json
anypoint-cli-v4 api-mgr:api:redeploy <apiInstanceId> --environment Sandbox
```

## 4. Run

```bash
cp env.local.sh.example env.local.sh   # set CMP_GW_URL
./demo.sh
```

Expected through the gateway: `variant=leak` → `internalMargin` + `customerEmail`
stripped, `_contract.status=repaired`; `variant=broken` → JSON-RPC `-32052`
contract-violation (missing `currency`, `total` type mismatch). Point `CMP_GW_URL`
at the direct mock surface to see the raw drifted rows for contrast.

## Notes

- The guard's **egress to CDGC** uses the `format:service` cluster from
  `cdgcBaseApiUrl`/`cdgcLoginUrl`; confirm the gateway can reach
  `*.informaticacloud.com`.
- `recordsPath=orders` because the records live under `orders` in the payload.
- Body-inspecting + JSON/single-message-SSE only; whole-stream SSE is out of scope.
