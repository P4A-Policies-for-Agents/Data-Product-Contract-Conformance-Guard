// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Data Product Contract Conformance Guard — inbound Omni/Flex Gateway policy.
//!
//! Checks each data-product response against its CDGC-governed field contract and
//! acts per drift type — off / log / inform / strip / reject — for:
//!   * unexpected fields (not in the contract),
//!   * missing required fields,
//!   * type mismatches,
//!   * sensitive-field leaks (a contract field flagged sensitive appearing).
//!
//! The contract is a JSON array of {name,type,required,sensitive,term} carried on
//! the CDGC asset (its governed description block; the `term` links each field to
//! a governed Business Term). It is fetched via the CDGC Login→JWT→data360 chain
//! and cached (lazy refresh, single-flight). Governed by the same `format:service`
//! egress + `HttpClient` pattern as the metadata-injection policy.
//!
//! This guard **inspects and rewrites the response body** (strip/reject), so its
//! outcome rides in a `_contract` annotation in the payload. It handles JSON and
//! single-message SSE `tools/call` results; whole-stream rewrites are out of scope.
//! Fail-open on its own outage (no contract → pass through).

mod cdgc;
mod conformance;
mod generated;

use std::rc::Rc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use pdk::data_storage::{DataStorage, DataStorageBuilder, DataStorageError, StoreMode};
use pdk::hl::timer::Clock;
use pdk::hl::*;
use pdk::logger;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::cdgc::{nonce_from_time, CachedContract, RefreshLock};
use crate::conformance::{analyze_record, apply_strip, decide, Action, Actions, ContractField};
use crate::generated::config::Config;

const CONTRACT_CACHE_NAMESPACE: &str = "ccg-contract";
const REFRESH_LOCK_NAMESPACE: &str = "ccg-refresh-lock";
const CONTRACT_CACHE_KEY_PREFIX: &str = "ccg-contract-";
const REFRESH_LOCK_KEY_PREFIX: &str = "ccg-lock-";
const REFRESH_LOCK_TTL_SECONDS: i64 = 30;
const REFRESH_LOCK_TTL_MS: u32 = (REFRESH_LOCK_TTL_SECONDS as u32) * 1000;
const CONTRACT_STORE_MIN_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const CAS_MAX_RETRIES: u32 = 3;
const DEFAULT_TIMEOUT_MS: i64 = 5_000;
// Six chained CDGC calls (login, jwt, file, columns, term-links, terms).
const CDGC_REFRESH_BUDGET_MS: i64 = 15_000;
const DEFAULT_REFRESH_INTERVAL_SECONDS: i64 = 86_400;
const SEARCH_PATH: &str = "/ccgf-searchv2/api/v1/search";
const CT_FLATFIELD: &str = "com.infa.odin.models.file.flat.FlatField";
const REL_TECH_GLOSSARY: &str = "com.infa.ccgf.models.governance.IClassTechnicalGlossaryBase";
const ATTR_ISCDE: &str = "com.infa.ccgf.models.governance.isCDE";
// The structured IDMC "Security Level" classification on a Business Term
// (Public | Internal | Confidential | Restricted) — the primary sensitivity signal.
const ATTR_SECURITY_CLASS: &str = "com.infa.ccgf.models.governance.securityClassification";
const DEFAULT_SENSITIVE_LEVELS: &str = "confidential,restricted";
const DEFAULT_SENSITIVE_MARKER: &str = "confidential";
/// JSON-RPC error code for a contract-conformance reject (server-defined range).
const RPC_CONTRACT_VIOLATION: i64 = -32052;

#[derive(Deserialize)]
struct CdgcLoginResponse {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "orgId")]
    org_id: String,
}
#[derive(Deserialize)]
struct CdgcJwtResponse {
    jwt_token: String,
}

#[derive(Clone)]
struct Ctx {
    asset_id: String,
    rpc_id: Value,
}

fn is_content_method(method: &str) -> bool {
    matches!(
        method,
        "tools/call" | "resources/read" | "prompts/get"
            | "message/send" | "message/stream" | "SendMessage" | "SendStreamingMessage"
    )
}

fn now_secs(clock: &Clock) -> i64 {
    clock.now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
fn elapsed_ms(start: SystemTime, now: SystemTime) -> i64 {
    now.duration_since(start).map(|d| d.as_millis() as i64).unwrap_or(0)
}
fn next_call_timeout(per_call_ms: i64, elapsed: i64) -> Option<Duration> {
    let remaining = CDGC_REFRESH_BUDGET_MS - elapsed;
    if remaining <= 0 {
        return None;
    }
    Some(Duration::from_millis(per_call_ms.min(remaining).max(1) as u64))
}
fn contract_store_ttl_ms(config: &Config) -> u32 {
    let refresh = config.refresh_interval_seconds.unwrap_or(DEFAULT_REFRESH_INTERVAL_SECONDS).max(0) as u64;
    refresh.saturating_mul(2).saturating_mul(1000).max(CONTRACT_STORE_MIN_TTL_MS).min(u32::MAX as u64) as u32
}
fn actions_of(config: &Config) -> Actions {
    Actions {
        unexpected: Action::parse(config.on_unexpected_field.as_deref().unwrap_or("strip")),
        missing_required: Action::parse(config.on_missing_required.as_deref().unwrap_or("reject")),
        type_mismatch: Action::parse(config.on_type_mismatch.as_deref().unwrap_or("inform")),
        sensitive: Action::parse(config.on_sensitive_field.as_deref().unwrap_or("strip")),
    }
}

/// Authenticate to IDMC (Login → JWT). Returns (jwt, orgId).
async fn cdgc_auth(client: &HttpClient, config: &Config, clock: &Clock, start: SystemTime) -> Result<(String, String)> {
    let per_call = config.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let login_body = serde_json::to_vec(&json!({
        "username": config.cdgc_org_username, "password": config.cdgc_org_password,
    }))?;
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now())).ok_or_else(|| anyhow!("budget before Login"))?;
    let login_resp = client.request(&config.cdgc_login_url).path("/identity-service/api/v1/Login")
        .headers(vec![("Content-Type", "application/json")]).body(&login_body).timeout(t).post().await
        .map_err(|e| anyhow!("CDGC login failed: {e}"))?;
    if login_resp.status_code() >= 300 {
        return Err(anyhow!("CDGC login status {}", login_resp.status_code()));
    }
    let login: CdgcLoginResponse = serde_json::from_slice(login_resp.body()).map_err(|e| anyhow!("parse login: {e}"))?;
    let nonce = nonce_from_time(clock.now());
    let cookie = format!("USER_SESSION={}", login.session_id);
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now())).ok_or_else(|| anyhow!("budget before JWT"))?;
    let jwt_resp = client.request(&config.cdgc_login_url)
        .path(&format!("/identity-service/api/v1/jwt/Token?client_id=idmc_api&nonce={nonce}"))
        .headers(vec![("cookie", cookie.as_str()), ("IDS-SESSION-ID", login.session_id.as_str())])
        .timeout(t).get().await.map_err(|e| anyhow!("CDGC JWT failed: {e}"))?;
    if jwt_resp.status_code() >= 300 {
        return Err(anyhow!("CDGC JWT status {}", jwt_resp.status_code()));
    }
    let jwt: CdgcJwtResponse = serde_json::from_slice(jwt_resp.body()).map_err(|e| anyhow!("parse jwt: {e}"))?;
    Ok((jwt.jwt_token, login.org_id))
}

/// One ccgf-searchv2 Elasticsearch query. Returns the `hits.hits[]` array's `sourceAsMap`s.
async fn cdgc_search(
    client: &HttpClient, config: &Config, clock: &Clock, start: SystemTime,
    jwt: &str, org: &str, body: &Value,
) -> Result<Vec<Value>> {
    let per_call = config.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let authz = format!("Bearer {jwt}");
    let payload = serde_json::to_vec(body)?;
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now())).ok_or_else(|| anyhow!("budget before search"))?;
    let resp = client.request(&config.cdgc_search_url).path(SEARCH_PATH)
        .headers(vec![
            ("Authorization", authz.as_str()),
            ("X-INFA-ORG-ID", org),
            ("X-INFA-SEARCH-LANGUAGE", "elasticsearch"),
            ("Content-Type", "application/json"),
        ])
        .body(&payload).timeout(t).post().await
        .map_err(|e| anyhow!("CDGC search failed: {e}"))?;
    if resp.status_code() >= 300 {
        return Err(anyhow!("CDGC search status {}", resp.status_code()));
    }
    let v: Value = serde_json::from_slice(resp.body()).map_err(|e| anyhow!("parse search: {e}"))?;
    Ok(v.get("hits").and_then(|h| h.get("hits")).and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|h| h.get("sourceAsMap").cloned()).collect())
        .unwrap_or_default())
}

fn s(map: &Value, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Catalog-driven contract: Login → JWT, then via ccgf-searchv2 resolve the schema
/// asset, enumerate its columns, their linked Business Terms, and build the contract
/// (name + datatype + required[isCDE] + sensitive[term Security Level, desc-marker fallback] + term).
async fn fetch_contract(
    client: &HttpClient,
    config: &Config,
    clock: &Clock,
    schema_id: &str,
) -> Result<(Vec<ContractField>, Option<String>, Option<String>)> {
    let start = clock.now();
    let (jwt, org) = cdgc_auth(client, config, clock, start).await?;
    let sens_marker = config.sensitive_marker.as_deref().unwrap_or(DEFAULT_SENSITIVE_MARKER).to_lowercase();
    let sens_levels: Vec<String> = config.sensitive_levels.as_deref().unwrap_or(DEFAULT_SENSITIVE_LEVELS)
        .split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();

    // 1. Resolve the schema asset → location + identity.
    let files = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
        "from":0,"size":1,"query":{"bool":{"must":[
            {"terms":{"elementType":["OBJECT"]}},
            {"terms":{"core.identity":[schema_id]}}]}}
    })).await?;
    let file = files.into_iter().next().ok_or_else(|| anyhow!("schema asset '{schema_id}' not found"))?;
    let location = s(&file, "core.location").ok_or_else(|| anyhow!("schema asset has no core.location"))?;
    let file_name = s(&file, "core.name");
    let external_id = s(&file, "core.externalId");

    // 2. Enumerate columns (children of the schema location).
    let cols = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
        "from":0,"size":1000,"query":{"bool":{
            "must":[{"terms":{"core.classType":[CT_FLATFIELD]}}],
            "filter":[{"terms":{"core.location::path_hierarchy.parent":[location]}}]}}
    })).await?;
    if cols.is_empty() {
        return Err(anyhow!("no columns found for schema asset '{schema_id}'"));
    }
    let col_ids: Vec<String> = cols.iter().filter_map(|c| s(c, "core.identity")).collect();

    // 3. Column → Business Term links.
    let rels = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
        "from":0,"size":5000,"query":{"bool":{"must":[
            {"terms":{"elementType":["RELATIONSHIP"]}},
            {"terms":{"type":[REL_TECH_GLOSSARY]}},
            {"terms":{"core.sourceIdentity":col_ids}}]}}
    })).await?;
    let mut col_to_term: Map<String, Value> = Map::new();
    let mut term_ids: Vec<String> = Vec::new();
    for r in &rels {
        if let (Some(src), Some(tgt)) = (s(r, "core.sourceIdentity"), s(r, "core.targetIdentity")) {
            col_to_term.entry(src).or_insert(Value::String(tgt.clone()));
            if !term_ids.contains(&tgt) {
                term_ids.push(tgt);
            }
        }
    }

    // 4. Resolve the linked terms → name / Security Level / description (fallback) / isCDE.
    let mut terms: Map<String, Value> = Map::new(); // termId → {name, desc, isCDE}
    if !term_ids.is_empty() {
        let tdocs = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
            "from":0,"size":5000,"query":{"bool":{"must":[
                {"terms":{"elementType":["OBJECT"]}},
                {"terms":{"core.identity":term_ids}}]}}
        })).await?;
        for t in &tdocs {
            if let Some(id) = s(t, "core.identity") {
                terms.insert(id, json!({
                    "name": s(t, "core.name"),
                    "desc": s(t, "core.description").unwrap_or_default(),
                    "level": s(t, ATTR_SECURITY_CLASS).unwrap_or_default(),
                    "isCDE": t.get(ATTR_ISCDE).and_then(Value::as_bool).unwrap_or(false),
                }));
            }
        }
    }

    // 5. Build the contract, one field per column.
    let mut fields = Vec::new();
    for c in &cols {
        let (Some(id), Some(name)) = (s(c, "core.identity"), s(c, "core.name")) else { continue };
        let ftype = s(c, "core.dataType").or_else(|| s(c, "core.inferredDataType"));
        let mut required = false;
        let mut sensitive = false;
        let mut term_name: Option<String> = None;
        if let Some(Value::String(tid)) = col_to_term.get(&id) {
            if let Some(term) = terms.get(tid) {
                term_name = term.get("name").and_then(Value::as_str).map(str::to_string);
                required = term.get("isCDE").and_then(Value::as_bool).unwrap_or(false);
                // Primary: the term's structured Security Level classification.
                // Fallback (only when no level is set): the description substring marker.
                let level = term.get("level").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
                sensitive = if level.is_empty() {
                    let desc = term.get("desc").and_then(Value::as_str).unwrap_or("");
                    desc.to_lowercase().contains(&sens_marker)
                } else {
                    sens_levels.contains(&level)
                };
            }
        }
        fields.push(ContractField { name, ftype, required, sensitive, term: term_name });
    }
    Ok((fields, file_name, external_id))
}

async fn read_cached<S: DataStorage>(store: &S, key: &str) -> Option<CachedContract> {
    match store.get::<CachedContract>(key).await {
        Ok(Some((c, _))) => Some(c),
        Ok(None) => None,
        Err(e) => {
            logger::warn!("ccg: cache read failed: {e}");
            None
        }
    }
}
async fn write_cached<S: DataStorage>(store: &S, key: &str, entry: &CachedContract) {
    for _ in 0..CAS_MAX_RETRIES {
        match store.get::<CachedContract>(key).await {
            Ok(Some((_, v))) => match store.store(key, &StoreMode::Cas(v), entry).await {
                Ok(()) => return,
                Err(DataStorageError::CasMismatch) => continue,
                Err(e) => { logger::warn!("ccg: persist failed: {e}"); return; }
            },
            Ok(None) => match store.store(key, &StoreMode::Absent, entry).await {
                Ok(()) => return,
                Err(DataStorageError::CasMismatch) => continue,
                Err(e) => { logger::warn!("ccg: persist failed: {e}"); return; }
            },
            Err(e) => { logger::warn!("ccg: read-before-persist failed: {e}"); return; }
        }
    }
}
async fn try_acquire_refresh_lock<S: DataStorage>(store: &S, key: &str, now: i64) -> Result<bool, DataStorageError> {
    let entry = RefreshLock { acquired_at: now };
    match store.store(key, &StoreMode::Absent, &entry).await {
        Ok(()) => Ok(true),
        Err(DataStorageError::CasMismatch) => match store.get::<RefreshLock>(key).await? {
            Some((existing, v)) => {
                if now - existing.acquired_at < REFRESH_LOCK_TTL_SECONDS { Ok(false) }
                else {
                    match store.store(key, &StoreMode::Cas(v), &entry).await {
                        Ok(()) => Ok(true),
                        Err(DataStorageError::CasMismatch) => Ok(false),
                        Err(e) => Err(e),
                    }
                }
            }
            None => match store.store(key, &StoreMode::Absent, &entry).await {
                Ok(()) => Ok(true),
                Err(DataStorageError::CasMismatch) => Ok(false),
                Err(e) => Err(e),
            },
        },
        Err(e) => Err(e),
    }
}

async fn get_contract<S: DataStorage>(
    client: &HttpClient, config: &Config, clock: &Clock, contract_store: &S, lock_store: &S, asset_id: &str,
) -> Option<CachedContract> {
    let key = format!("{CONTRACT_CACHE_KEY_PREFIX}{asset_id}");
    let ttl = config.refresh_interval_seconds.unwrap_or(DEFAULT_REFRESH_INTERVAL_SECONDS).max(0);
    let now = now_secs(clock);
    let cached = read_cached(contract_store, &key).await;
    if let Some(c) = &cached {
        if now - c.timestamp < ttl {
            return cached;
        }
    }
    let lock_key = format!("{REFRESH_LOCK_KEY_PREFIX}{asset_id}");
    if !try_acquire_refresh_lock(lock_store, &lock_key, now).await.unwrap_or(true) {
        return cached;
    }
    match fetch_contract(client, config, clock, asset_id).await {
        Ok((fields, name, external_id)) => {
            let entry = CachedContract { fields, name, external_id, timestamp: now };
            write_cached(contract_store, &key, &entry).await;
            Some(entry)
        }
        Err(e) => {
            logger::warn!("ccg: contract refresh failed for '{asset_id}': {e}");
            if config.fail_open_on_cdgc_error.unwrap_or(true) { cached } else { None }
        }
    }
}

// ---- response body helpers ----

fn parse_rpc(text: &str, is_sse: bool) -> Option<Value> {
    if is_sse {
        for line in text.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(rest.trim()) {
                    return Some(v);
                }
            }
        }
        None
    } else {
        serde_json::from_str(text).ok()
    }
}
fn frame(rpc: &Value, is_sse: bool) -> String {
    let j = rpc.to_string();
    if is_sse { format!("event: message\ndata: {j}\n\n") } else { j }
}
fn nav<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}
fn nav_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(m) => m.get_mut(seg)?,
            Value::Array(a) => a.get_mut(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}
/// Records (as owned object snapshots) at `path` in the payload.
fn records_snapshot(payload: &Value, path: &str) -> Vec<Map<String, Value>> {
    match nav(payload, path) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_object().cloned()).collect(),
        Some(Value::Object(m)) => vec![m.clone()],
        _ => Vec::new(),
    }
}

async fn request_filter(request_state: RequestState, config: Rc<Config>) -> Flow<Option<Ctx>> {
    let hs = request_state.into_headers_state().await;
    let header_name = config.schema_id_header.as_deref().unwrap_or("x-dp-schema-id");
    let asset_id = hs.handler().header(header_name).filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| config.schema_id.clone());
    let ct = hs.handler().header("content-type").unwrap_or_default();
    if ct.starts_with("application/json") && hs.method().as_str() == "POST" {
        let bs = hs.into_body_state().await;
        if let Ok(v) = serde_json::from_slice::<Value>(&bs.handler().body()) {
            match v.get("method").and_then(Value::as_str) {
                Some(m) if is_content_method(m) => {
                    let rpc_id = v.get("id").cloned().unwrap_or(Value::Null);
                    return Flow::Continue(Some(Ctx { asset_id, rpc_id }));
                }
                Some(_) => return Flow::Continue(None), // non-content JSON-RPC → skip
                None => {}
            }
        }
    }
    // REST / non-JSON-RPC → still guard, with a null rpc id.
    Flow::Continue(Some(Ctx { asset_id, rpc_id: Value::Null }))
}

#[allow(clippy::too_many_arguments)]
/// Where the business payload lives in the response, so we can write it back.
/// MCP/A2A wrap it in a JSON-RPC envelope; a REST API returns it directly.
enum Place {
    Structured,     // result.structuredContent (MCP)
    Content(usize), // result.content[i].text as embedded JSON (MCP)
    Rest,           // the response body *is* the payload (REST/HTTP API)
}

async fn response_filter<S: DataStorage>(
    response_state: ResponseState,
    request_data: RequestData<Option<Ctx>>,
    config: Rc<Config>,
    client: Rc<HttpClient>,
    clock: Rc<Clock>,
    contract_store: Rc<S>,
    lock_store: Rc<S>,
) {
    let ctx = match request_data {
        RequestData::Continue(Some(c)) => c,
        _ => return,
    };

    let cc = match get_contract(&client, &config, &clock, &*contract_store, &*lock_store, &ctx.asset_id).await {
        Some(c) if !c.fields.is_empty() => c,
        _ => return, // no governed contract → pass through (fail-open)
    };
    let contract = cc.fields.clone();
    let actions = actions_of(&config);
    let records_path = config.records_path.as_deref().unwrap_or("");

    let hs = response_state.into_headers_state().await;
    let ct = hs.handler().header("content-type").unwrap_or_default();
    let is_sse = ct.contains("event-stream");
    if !is_sse && !ct.contains("json") {
        return;
    }
    hs.handler().remove_header("content-length");
    hs.handler().remove_header("content-encoding");
    let bs = hs.into_body_state().await;
    let text = match String::from_utf8(bs.handler().body()) {
        Ok(t) => t,
        Err(_) => return,
    };

    let mut rpc = match parse_rpc(&text, is_sse) {
        Some(v) => v,
        None => return,
    };
    // MCP/A2A wrap the payload in a JSON-RPC envelope; a REST API returns the JSON
    // payload directly. Detect which, extract the payload, and remember where to
    // write it back.
    let is_rpc = is_sse
        || rpc.get("jsonrpc").is_some()
        || (rpc.get("result").is_some() && rpc.get("id").is_some());
    let (mut payload, place) = if is_rpc {
        // Only govern successful tool results.
        let result = match rpc.get("result") {
            Some(r) => r,
            None => return,
        };
        // structuredContent, else the first JSON content[].text.
        if let Some(sc) = result.get("structuredContent") {
            (sc.clone(), Place::Structured)
        } else if let Some(arr) = result.get("content").and_then(Value::as_array) {
            let mut found = None;
            for (i, item) in arr.iter().enumerate() {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        if let Ok(v) = serde_json::from_str::<Value>(t) {
                            found = Some((v, i));
                            break;
                        }
                    }
                }
            }
            match found {
                Some((v, i)) => (v, Place::Content(i)),
                None => return,
            }
        } else {
            return;
        }
    } else {
        // REST / HTTP API: the whole response body is the payload.
        (rpc.clone(), Place::Rest)
    };

    // Analyze all records against the contract.
    let records = records_snapshot(&payload, records_path);
    if records.is_empty() {
        return;
    }
    let mut all_drifts = Vec::new();
    for r in &records {
        all_drifts.extend(analyze_record(r, &contract));
    }
    let dec = decide(&all_drifts, &actions);

    for entry in &dec.logged {
        logger::info!("ccg-drift asset={} {}", ctx.asset_id, entry);
    }

    // Reject → withhold the offending data and report the violation. MCP/A2A carry
    // the error in a JSON-RPC error envelope; a REST API gets a JSON error body.
    // (HTTP status can't be changed once the body is buffered, so REST rejects stay
    // at the upstream status with an error payload — same constraint as MCP's 200.)
    if dec.reject {
        logger::warn!("ccg: REJECT asset={} drift={}", ctx.asset_id, dec.summary);
        let message = format!("response violated the governed contract for asset {} ({})", ctx.asset_id, dec.summary);
        let err = if let Place::Rest = place {
            json!({ "error": { "code": RPC_CONTRACT_VIOLATION, "message": message },
                "_contract": { "status": "rejected", "assetId": ctx.asset_id,
                    "name": cc.name, "externalId": cc.external_id, "drift": dec.summary, "source": "cdgc" } })
        } else {
            json!({ "jsonrpc": "2.0", "id": ctx.rpc_id,
                "error": { "code": RPC_CONTRACT_VIOLATION, "message": message } })
        };
        if let Err(e) = bs.handler().set_body(frame(&err, is_sse).as_bytes()) {
            logger::warn!("ccg: set_body (reject) failed: {e:?}");
        }
        return;
    }

    // Strip marked fields from each record, then annotate the payload.
    if !dec.strip.is_empty() {
        if let Some(node) = nav_mut(&mut payload, records_path) {
            match node {
                Value::Array(a) => {
                    for e in a.iter_mut() {
                        if let Some(o) = e.as_object_mut() {
                            apply_strip(o, &dec.strip);
                        }
                    }
                }
                Value::Object(o) => {
                    apply_strip(o, &dec.strip);
                }
                _ => {}
            }
        }
    }
    let status = if !dec.strip.is_empty() { "repaired" } else if dec.inform { "drift" } else { "ok" };
    if let Value::Object(root) = &mut payload {
        // Self-describe (identity from the same CDGC fetch) AND report enforcement.
        root.insert("_contract".to_string(), json!({
            "status": status, "assetId": ctx.asset_id,
            "name": cc.name, "externalId": cc.external_id,
            "drift": dec.summary, "source": "cdgc",
        }));
    }

    // Write the payload back where we found it, then reframe.
    match place {
        Place::Structured => {
            if let Some(r) = rpc.get_mut("result") {
                if let Some(obj) = r.as_object_mut() {
                    obj.insert("structuredContent".to_string(), payload.clone());
                    // keep the text mirror consistent if present
                    if let Some(arr) = obj.get_mut("content").and_then(Value::as_array_mut) {
                        if let Some(first) = arr.iter_mut().find(|i| i.get("type").and_then(Value::as_str) == Some("text")) {
                            first["text"] = Value::String(payload.to_string());
                        }
                    }
                }
            }
        }
        Place::Content(i) => {
            if let Some(item) = rpc.pointer_mut(&format!("/result/content/{i}/text")) {
                *item = Value::String(payload.to_string());
            }
        }
        Place::Rest => {
            // REST: the response body is the payload itself (no envelope to reframe).
            rpc = payload.clone();
        }
    }

    if let Err(e) = bs.handler().set_body(frame(&rpc, is_sse).as_bytes()) {
        logger::warn!("ccg: set_body failed: {e:?}");
    }
}

fn launch_policy<S: DataStorage + 'static>(
    launcher: Launcher, config: Rc<Config>, client: Rc<HttpClient>, clock: Rc<Clock>,
    contract_store: Rc<S>, lock_store: Rc<S>,
) -> impl std::future::Future<Output = Result<()>> {
    let cfg_req = config.clone();
    let filter = on_request(move |rs| {
        let c = cfg_req.clone();
        async move { request_filter(rs, c).await }
    })
    .on_response(move |rs, rd| {
        let c = config.clone();
        let cl = client.clone();
        let ck = clock.clone();
        let cs = contract_store.clone();
        let ls = lock_store.clone();
        async move { response_filter(rs, rd, c, cl, ck, cs, ls).await }
    });
    async move { launcher.launch(filter).await.map_err(Into::into) }
}

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    client: HttpClient,
    storage_builder: DataStorageBuilder,
    clock: Clock,
) -> Result<()> {
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|err| anyhow!("Failed to parse configuration '{}'. Cause: {}", String::from_utf8_lossy(&bytes), err))?;
    let config = Rc::new(config);
    let client = Rc::new(client);
    let clock = Rc::new(clock);
    if config.distributed.unwrap_or(false) {
        let cs = Rc::new(storage_builder.remote(CONTRACT_CACHE_NAMESPACE, contract_store_ttl_ms(&config)));
        let ls = Rc::new(storage_builder.remote(REFRESH_LOCK_NAMESPACE, REFRESH_LOCK_TTL_MS));
        launch_policy(launcher, config, client, clock, cs, ls).await
    } else {
        let cs = Rc::new(storage_builder.local(CONTRACT_CACHE_NAMESPACE));
        let ls = Rc::new(storage_builder.local(REFRESH_LOCK_NAMESPACE));
        launch_policy(launcher, config, client, clock, cs, ls).await
    }
}
