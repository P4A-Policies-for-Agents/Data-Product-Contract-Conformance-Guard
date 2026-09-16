# Roadmap

## 1. Governed field source: from description block → richer CDGC sources

Today the contract is a JSON block on the asset's governed description (the only
API-reliable carrier: CDGC rejects term→DataSet links, won't enumerate scanned
columns via API, and custom attributes need pre-definition). Upgrades:
- **Defined custom attribute** (`contract.fields`) — define the attribute once in
  CDGC settings, then read it via `segments=customAttributes` (cleaner than the
  description block).
- **MCC-scanned columns** — a Secure Agent scan gives real columns with datatypes
  + classification/PII; resolve per-column assets for authoritative types and
  sensitivity (needs the scan infra; see `infa-secure-agent-eks`).

## 2. Streaming (whole-stream SSE) conformance

Current body inspection covers JSON and single-message SSE `tools/call` results.
Token-by-token LLM streams need **event-local** rewrite (the `pdk-sse-parsing`
buffer-drain pattern) — check/strip per SSE event without buffering the whole
stream. Reject on a stream is inherently partial (bytes already sent).

## 3. Per-caller sensitivity (entitlement-aware strip)

Today a `sensitive` field is stripped for everyone. Combine with caller claims so
an entitled consumer keeps it and others don't — overlaps the Field-Level
Entitlement Filter idea; share the classification source.

## 4. Consumer-driven contracts (#9)

Let the caller declare expected fields (a header); verify provider output against
the *consumer's* contract in addition to the governed one — runtime
consumer-contract testing.

## 5. Outcome signalling

Emit the drift outcome as a structured audit/decision record (for the
observability suite) and, where the runtime allows, a response header in addition
to the in-body `_contract` annotation.

## 6. Enum / range / format conformance

Extend beyond type to declared enumerations, numeric ranges, and string formats
(the term/schema can carry them), catching out-of-domain values (grounding
conformance, #30).
