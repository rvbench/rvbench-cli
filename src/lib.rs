//! Contract validation, digests and the `rvbench` client library.
pub mod auth;
pub mod baseline;
pub mod cli;
pub mod control;
pub mod files;
pub mod hidden;
pub mod init;
pub mod results;
pub mod spec;
pub mod worker;
pub mod worker_cmd;

pub use baseline::validate_baseline_results;
pub use hidden::{hidden_readiness_blockers, resolve_hidden_case, validate_hidden_spec};
pub use results::validate_eval_results;
pub use spec::{readiness_blockers, validate_spec};

pub use rvbench_protocol::json;
pub use rvbench_protocol::json::{load_json, parse_json, spec_digest};

use serde_json::Value;
use std::collections::BTreeSet;

pub fn a(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
pub fn b(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}
pub fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or(-1)
}
pub fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}
pub fn names(items: &Value, key: &str) -> Vec<String> {
    a(items).iter().map(|v| s(&v[key]).to_owned()).collect()
}
pub fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}
pub fn set(values: &[String]) -> BTreeSet<String> {
    values.iter().cloned().collect()
}
pub fn require(errors: &mut Vec<String>, condition: bool, message: impl Into<String>) {
    if !condition {
        errors.push(message.into());
    }
}
pub fn unique(errors: &mut Vec<String>, values: Vec<String>, label: &str) {
    require(
        errors,
        set(&values).len() == values.len(),
        format!("duplicate {label}"),
    );
}
/// 编译期嵌入的合同 schema 原文；`init` 会把同一份副本写到生成环境里。
pub fn schema_text(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "spec" => include_str!("../contracts/schemas/spec.schema.json"),
        "hidden" => include_str!("../contracts/schemas/spec.hidden.schema.json"),
        "results" => include_str!("../contracts/schemas/eval_results.schema.json"),
        "baseline-results" => include_str!("../contracts/schemas/baseline_results.schema.json"),
        _ => return None,
    })
}
pub fn schema_errors(value: &Value, kind: &str) -> Vec<String> {
    use std::sync::OnceLock;
    static PUBLIC: OnceLock<jsonschema::Validator> = OnceLock::new();
    static HIDDEN: OnceLock<jsonschema::Validator> = OnceLock::new();
    static RESULTS: OnceLock<jsonschema::Validator> = OnceLock::new();
    static BASELINE_RESULTS: OnceLock<jsonschema::Validator> = OnceLock::new();
    let (cell, formats) = match kind {
        "spec" => (&PUBLIC, false),
        "hidden" => (&HIDDEN, false),
        "results" => (&RESULTS, true),
        "baseline-results" => (&BASELINE_RESULTS, true),
        _ => return vec!["unknown schema".into()],
    };
    let text = schema_text(kind).expect("known schema kind");
    let validator = cell.get_or_init(|| {
        let schema: Value = serde_json::from_str(text).expect("embedded schema is JSON");
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .should_validate_formats(formats)
            .build(&schema)
            .expect("embedded schema compiles")
    });
    // Avoid formatting the instance; error details can contain hidden inputs.
    validator
        .iter_errors(value)
        .map(|e| format!("{}: schema violation", e.instance_path()))
        .collect()
}
