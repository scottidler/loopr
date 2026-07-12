use super::*;

#[test]
fn from_texts_mints_sequential_ids() {
    let ac = AcceptanceCriteria::from_texts(vec!["first".to_string(), "second".to_string(), "third".to_string()]);
    assert_eq!(
        ac.0,
        vec![
            Criterion {
                id: 1,
                text: "first".to_string()
            },
            Criterion {
                id: 2,
                text: "second".to_string()
            },
            Criterion {
                id: 3,
                text: "third".to_string()
            },
        ]
    );
}

#[test]
fn serialize_is_bare_object_array() {
    let ac = AcceptanceCriteria::from_texts(vec!["a".to_string(), "b".to_string()]);
    let json = serde_json::to_string(&ac).unwrap();
    assert_eq!(json, r#"[{"id":1,"text":"a"},{"id":2,"text":"b"}]"#);
}

#[test]
fn roundtrip_structured_form() {
    let ac = AcceptanceCriteria::from_texts(vec!["a".to_string(), "b".to_string()]);
    let json = serde_json::to_string(&ac).unwrap();
    let back: AcceptanceCriteria = serde_json::from_str(&json).unwrap();
    assert_eq!(ac, back);
}

#[test]
fn backcompat_deserializes_old_string_array() {
    // Break-to-prove back-compat: a pre-Phase-8 `works.jsonl` stored the
    // criteria as a bare string array. This exact on-disk shape MUST still
    // load, with entries becoming Criteria carrying sequential 1-based ids.
    let old = r#"["module exists","tests pass"]"#;
    let ac: AcceptanceCriteria = serde_json::from_str(old).unwrap();
    assert_eq!(
        ac.0,
        vec![
            Criterion {
                id: 1,
                text: "module exists".to_string()
            },
            Criterion {
                id: 2,
                text: "tests pass".to_string()
            },
        ]
    );
}

#[test]
fn structured_form_preserves_explicit_ids() {
    let structured = r#"[{"id":7,"text":"seven"},{"id":9,"text":"nine"}]"#;
    let ac: AcceptanceCriteria = serde_json::from_str(structured).unwrap();
    assert_eq!(
        ac.0,
        vec![
            Criterion {
                id: 7,
                text: "seven".to_string()
            },
            Criterion {
                id: 9,
                text: "nine".to_string()
            },
        ]
    );
}

#[test]
fn empty_array_deserializes_empty() {
    let ac: AcceptanceCriteria = serde_json::from_str("[]").unwrap();
    assert!(ac.is_empty());
    assert_eq!(ac.len(), 0);
}
