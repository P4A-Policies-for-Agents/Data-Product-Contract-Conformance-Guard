// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Pure contract-conformance core (no PDK imports) — fully unit-testable.
//!
//! A governed field contract (parsed from the CDGC asset) is compared against the
//! actual fields of a response record. Each divergence is a typed `Drift`, and a
//! per-type `Action` (off/log/inform/strip/reject) turns the drifts into a
//! `Decision` the policy applies to the response.

use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One governed field from the contract. `term` (the governing Business Term) is
/// carried for provenance but not used in the logic.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ContractField {
    pub name: String,
    #[serde(rename = "type", default)]
    pub ftype: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub term: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Off,
    Log,
    Inform,
    Strip,
    Reject,
}

impl Action {
    pub fn parse(s: &str) -> Action {
        match s {
            "log" => Action::Log,
            "inform" => Action::Inform,
            "strip" => Action::Strip,
            "reject" => Action::Reject,
            _ => Action::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftKind {
    Unexpected,
    MissingRequired,
    TypeMismatch,
    Sensitive,
}

impl DriftKind {
    /// Compact marker used in the drift summary header.
    pub fn marker(self) -> char {
        match self {
            DriftKind::Unexpected => '+',
            DriftKind::MissingRequired => '-',
            DriftKind::TypeMismatch => '~',
            DriftKind::Sensitive => '!',
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Drift {
    pub kind: DriftKind,
    pub field: String,
}

/// Per-drift-type actions.
#[derive(Debug, Clone, Copy)]
pub struct Actions {
    pub unexpected: Action,
    pub missing_required: Action,
    pub type_mismatch: Action,
    pub sensitive: Action,
}

impl Actions {
    pub fn for_kind(&self, k: DriftKind) -> Action {
        match k {
            DriftKind::Unexpected => self.unexpected,
            DriftKind::MissingRequired => self.missing_required,
            DriftKind::TypeMismatch => self.type_mismatch,
            DriftKind::Sensitive => self.sensitive,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Decision {
    pub reject: bool,
    pub strip: BTreeSet<String>,
    pub inform: bool,
    pub summary: String,
    pub logged: Vec<String>,
}

/// Parse the JSON field-contract array that follows `marker` in the asset
/// description. Tolerates prose before the marker and trailing text: it slices
/// from the first `[` after the marker to the last `]`. Missing/invalid → empty.
pub fn parse_contract(description: &str, marker: &str) -> Vec<ContractField> {
    let Some(pos) = description.find(marker) else { return Vec::new() };
    let tail = &description[pos + marker.len()..];
    let (Some(start), Some(end)) = (tail.find('['), tail.rfind(']')) else { return Vec::new() };
    if end < start {
        return Vec::new();
    }
    serde_json::from_str::<Vec<ContractField>>(&tail[start..=end]).unwrap_or_default()
}

fn type_matches(ftype: &str, v: &Value) -> bool {
    match ftype.to_lowercase().as_str() {
        "string" => v.is_string(),
        "number" | "float" | "double" | "decimal" => v.is_number(),
        "integer" | "int" => v.is_i64() || v.is_u64(),
        "boolean" | "bool" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        _ => true, // unknown declared type → don't flag
    }
}

/// Compute all drifts for one response record against the contract.
pub fn analyze_record(record: &Map<String, Value>, contract: &[ContractField]) -> Vec<Drift> {
    let names: HashSet<&str> = contract.iter().map(|c| c.name.as_str()).collect();
    let mut drifts = Vec::new();
    for c in contract {
        match record.get(&c.name) {
            None => {
                if c.required {
                    drifts.push(Drift { kind: DriftKind::MissingRequired, field: c.name.clone() });
                }
            }
            Some(v) => {
                if c.sensitive {
                    drifts.push(Drift { kind: DriftKind::Sensitive, field: c.name.clone() });
                }
                if let Some(t) = &c.ftype {
                    if !type_matches(t, v) {
                        drifts.push(Drift { kind: DriftKind::TypeMismatch, field: c.name.clone() });
                    }
                }
            }
        }
    }
    for k in record.keys() {
        if !names.contains(k.as_str()) {
            drifts.push(Drift { kind: DriftKind::Unexpected, field: k.clone() });
        }
    }
    drifts
}

/// Turn drifts into an action decision. Precedence: any `reject` blocks the whole
/// response; otherwise strip the marked fields; inform/log are additive.
pub fn decide(drifts: &[Drift], actions: &Actions) -> Decision {
    let mut dec = Decision::default();
    let mut tokens: Vec<String> = Vec::new();
    for d in drifts {
        let act = actions.for_kind(d.kind);
        if act == Action::Off {
            continue;
        }
        let token = format!("{}{}", d.kind.marker(), d.field);
        if !tokens.contains(&token) {
            tokens.push(token);
        }
        match act {
            Action::Reject => dec.reject = true,
            Action::Strip => {
                // A missing field can't be stripped; surface it as inform instead.
                if d.kind == DriftKind::MissingRequired {
                    dec.inform = true;
                } else {
                    dec.strip.insert(d.field.clone());
                }
            }
            Action::Inform => dec.inform = true,
            Action::Log => dec.logged.push(format!("{:?}:{}", d.kind, d.field)),
            Action::Off => {}
        }
    }
    dec.summary = tokens.join(",");
    dec
}

/// Remove the stripped fields from a record in place. Returns true if it changed.
pub fn apply_strip(record: &mut Map<String, Value>, strip: &BTreeSet<String>) -> bool {
    let mut changed = false;
    for f in strip {
        if record.remove(f).is_some() {
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contract() -> Vec<ContractField> {
        parse_contract(
            r#"prose... contract-fields=[
              {"name":"orderId","type":"string","required":true,"term":"Order Id"},
              {"name":"total","type":"number","required":true},
              {"name":"currency","type":"string","required":true},
              {"name":"customerEmail","type":"string","required":false,"sensitive":true}]"#,
            "contract-fields=",
        )
    }

    fn rec(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn actions() -> Actions {
        Actions {
            unexpected: Action::Strip,
            missing_required: Action::Reject,
            type_mismatch: Action::Inform,
            sensitive: Action::Strip,
        }
    }

    #[test]
    fn parses_contract() {
        let c = contract();
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].name, "orderId");
        assert_eq!(c[3].sensitive, true);
        assert_eq!(c[1].ftype.as_deref(), Some("number"));
    }

    #[test]
    fn bad_or_absent_contract_is_empty() {
        assert!(parse_contract("no marker here", "contract-fields=").is_empty());
        assert!(parse_contract("contract-fields= not json", "contract-fields=").is_empty());
    }

    #[test]
    fn clean_record_no_drift() {
        let d = analyze_record(&rec(json!({"orderId":"SO-1","total":10.0,"currency":"USD"})), &contract());
        assert!(d.is_empty());
    }

    #[test]
    fn detects_unexpected_missing_sensitive() {
        // + internalMargin (unexpected), - currency (missing required), ! customerEmail (sensitive present)
        let d = analyze_record(
            &rec(json!({"orderId":"SO-1","total":10.0,"internalMargin":0.6,"customerEmail":"a@b.com"})),
            &contract(),
        );
        assert!(d.contains(&Drift { kind: DriftKind::Unexpected, field: "internalMargin".into() }));
        assert!(d.contains(&Drift { kind: DriftKind::MissingRequired, field: "currency".into() }));
        assert!(d.contains(&Drift { kind: DriftKind::Sensitive, field: "customerEmail".into() }));
    }

    #[test]
    fn detects_type_mismatch() {
        let d = analyze_record(&rec(json!({"orderId":"SO-1","total":"10.00","currency":"USD"})), &contract());
        assert!(d.contains(&Drift { kind: DriftKind::TypeMismatch, field: "total".into() }));
    }

    #[test]
    fn decide_strips_and_rejects() {
        let d = analyze_record(
            &rec(json!({"orderId":"SO-1","total":10.0,"internalMargin":0.6,"customerEmail":"a@b.com"})),
            &contract(),
        );
        let dec = decide(&d, &actions());
        assert!(dec.reject); // currency missing → reject
        assert!(dec.strip.contains("internalMargin"));
        assert!(dec.strip.contains("customerEmail"));
        assert!(dec.summary.contains("+internalMargin"));
        assert!(dec.summary.contains("-currency"));
        assert!(dec.summary.contains("!customerEmail"));
    }

    #[test]
    fn decide_strip_only_when_no_reject() {
        // all required present; extra + sensitive → strip, no reject
        let d = analyze_record(
            &rec(json!({"orderId":"SO-1","total":10.0,"currency":"USD","internalMargin":0.6,"customerEmail":"a@b.com"})),
            &contract(),
        );
        let dec = decide(&d, &actions());
        assert!(!dec.reject);
        assert_eq!(dec.strip, BTreeSet::from(["internalMargin".to_string(), "customerEmail".to_string()]));
    }

    #[test]
    fn apply_strip_mutates() {
        let mut r = rec(json!({"orderId":"SO-1","internalMargin":0.6,"customerEmail":"a@b.com"}));
        let strip = BTreeSet::from(["internalMargin".to_string(), "customerEmail".to_string()]);
        assert!(apply_strip(&mut r, &strip));
        assert_eq!(r.keys().collect::<Vec<_>>(), vec!["orderId"]);
    }

    #[test]
    fn off_action_ignores_drift() {
        let a = Actions { unexpected: Action::Off, missing_required: Action::Off, type_mismatch: Action::Off, sensitive: Action::Off };
        let d = analyze_record(&rec(json!({"foo":"bar"})), &contract());
        let dec = decide(&d, &a);
        assert!(!dec.reject && dec.strip.is_empty() && !dec.inform);
    }
}
