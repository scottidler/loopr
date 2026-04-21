use super::*;

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct DummyInput {
    path: String,
    offset: Option<u64>,
}

#[test]
fn schema_value_for_dummy_is_object() {
    let v = schema_value::<DummyInput>();
    assert!(v.is_object(), "schema root should be an object");
    let obj = v.as_object().unwrap();
    assert!(
        obj.contains_key("properties")
            || obj.contains_key("$defs")
            || obj.contains_key("definitions")
            || obj.contains_key("type"),
        "schema must expose structural keys (got: {:?})",
        obj.keys().collect::<Vec<_>>()
    );
}

#[test]
fn schema_value_names_fields() {
    let v = schema_value::<DummyInput>();
    let s = v.to_string();
    assert!(s.contains("path"), "schema must mention `path`: {s}");
    assert!(s.contains("offset"), "schema must mention `offset`: {s}");
}
