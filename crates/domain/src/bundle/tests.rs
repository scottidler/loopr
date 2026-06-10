use super::*;

fn sample_bundle() -> Bundle {
    Bundle::new(
        WorkId::new(),
        "loopr/wk-abc12".to_string(),
        vec!["did the thing".to_string()],
    )
}

#[test]
fn model_round_trips() {
    let mut b = sample_bundle();
    b.model = Some("claude-sonnet-4-6-20260115".to_string());
    let json = serde_json::to_string(&b).unwrap();
    let back: Bundle = serde_json::from_str(&json).unwrap();
    assert_eq!(back.model.as_deref(), Some("claude-sonnet-4-6-20260115"));
}

#[test]
fn model_defaults_to_none_on_row_without_field() {
    // An old bundles.jsonl row written before model-pinning lacks the
    // `model` key; the additive `#[serde(default)]` must deserialize it
    // to None rather than (with deny_unknown_fields) failing the row.
    let b = sample_bundle();
    let mut value = serde_json::to_value(&b).unwrap();
    value.as_object_mut().unwrap().remove("model");
    assert!(value.get("model").is_none(), "precondition: model key removed");
    let back: Bundle = serde_json::from_value(value).unwrap();
    assert_eq!(back.model, None);
}

#[test]
fn new_bundle_has_no_model() {
    assert_eq!(sample_bundle().model, None);
}
