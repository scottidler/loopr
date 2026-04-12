use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use super::registry::PrimitiveRegistry;
use super::types::{Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput};

/// Minimal test primitive for registry tests.
struct EchoPrimitive;

impl Primitive for EchoPrimitive {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            Ok(PrimitiveOutput {
                values: {
                    let mut m = HashMap::new();
                    m.insert("echo".to_string(), params);
                    m
                },
                summary: "echoed params".to_string(),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "echo".to_string(),
            field_type: OutputType::Json,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "message".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// A second test primitive for duplicate-name detection.
struct AnotherEcho;

impl Primitive for AnotherEcho {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a mut PrimitiveContext<'_>,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async {
            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: String::new(),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Query-only primitive with no inputs/outputs.
struct NoopPrimitive;

impl Primitive for NoopPrimitive {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a mut PrimitiveContext<'_>,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async {
            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: "did nothing".to_string(),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

// --- Registry tests ---

#[test]
fn test_registry_new_is_empty() {
    let reg = PrimitiveRegistry::new();
    assert!(reg.is_empty());
    assert_eq!(reg.len(), 0);
}

#[test]
fn test_registry_register_and_get() {
    let mut reg = PrimitiveRegistry::new();
    reg.register(Box::new(EchoPrimitive)).unwrap();
    assert_eq!(reg.len(), 1);
    assert!(!reg.is_empty());

    let prim = reg.get("echo");
    assert!(prim.is_some());
    assert_eq!(prim.unwrap().name(), "echo");
}

#[test]
fn test_registry_get_nonexistent() {
    let reg = PrimitiveRegistry::new();
    assert!(reg.get("nonexistent").is_none());
}

#[test]
fn test_registry_duplicate_name_rejected() {
    let mut reg = PrimitiveRegistry::new();
    reg.register(Box::new(EchoPrimitive)).unwrap();
    let result = reg.register(Box::new(AnotherEcho));
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("duplicate primitive name: 'echo'"));
}

#[test]
fn test_registry_multiple_primitives() {
    let mut reg = PrimitiveRegistry::new();
    reg.register(Box::new(EchoPrimitive)).unwrap();
    reg.register(Box::new(NoopPrimitive)).unwrap();
    assert_eq!(reg.len(), 2);
    assert!(reg.get("echo").is_some());
    assert!(reg.get("noop").is_some());
}

#[test]
fn test_registry_validate_references_all_present() {
    let mut reg = PrimitiveRegistry::new();
    reg.register(Box::new(EchoPrimitive)).unwrap();
    reg.register(Box::new(NoopPrimitive)).unwrap();

    let missing = reg.validate_references(&["echo".to_string(), "noop".to_string()]);
    assert!(missing.is_empty());
}

#[test]
fn test_registry_validate_references_some_missing() {
    let mut reg = PrimitiveRegistry::new();
    reg.register(Box::new(EchoPrimitive)).unwrap();

    let missing = reg.validate_references(&["echo".to_string(), "missing-one".to_string(), "missing-two".to_string()]);
    assert_eq!(missing.len(), 2);
    assert!(missing.contains(&"missing-one".to_string()));
    assert!(missing.contains(&"missing-two".to_string()));
}

#[test]
fn test_registry_validate_references_empty() {
    let reg = PrimitiveRegistry::new();
    let missing = reg.validate_references(&[]);
    assert!(missing.is_empty());
}

#[test]
fn test_registry_names() {
    let mut reg = PrimitiveRegistry::new();
    reg.register(Box::new(EchoPrimitive)).unwrap();
    reg.register(Box::new(NoopPrimitive)).unwrap();

    let mut names: Vec<&str> = reg.names().collect();
    names.sort();
    assert_eq!(names, vec!["echo", "noop"]);
}

// --- Type tests ---

#[test]
fn test_output_type_compatible_with_self() {
    assert!(OutputType::String.compatible_with(&OutputType::String));
    assert!(OutputType::U32.compatible_with(&OutputType::U32));
    assert!(OutputType::U64.compatible_with(&OutputType::U64));
    assert!(OutputType::F64.compatible_with(&OutputType::F64));
    assert!(OutputType::Bool.compatible_with(&OutputType::Bool));
    assert!(OutputType::StringArray.compatible_with(&OutputType::StringArray));
    assert!(OutputType::Json.compatible_with(&OutputType::Json));
}

#[test]
fn test_output_type_incompatible() {
    assert!(!OutputType::String.compatible_with(&OutputType::U32));
    assert!(!OutputType::Bool.compatible_with(&OutputType::Json));
    assert!(!OutputType::U32.compatible_with(&OutputType::U64));
}

#[test]
fn test_validate_params_required_present() {
    let prim = EchoPrimitive;
    let params = serde_json::json!({"message": "hello"});
    assert!(prim.validate_params(&params).is_ok());
}

#[test]
fn test_validate_params_required_missing() {
    let prim = EchoPrimitive;
    let params = serde_json::json!({});
    let result = prim.validate_params(&params);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("required param 'message' is missing"));
}

#[test]
fn test_validate_params_no_required_fields() {
    let prim = NoopPrimitive;
    let params = serde_json::json!({});
    assert!(prim.validate_params(&params).is_ok());
}

#[test]
fn test_primitive_output_serde_roundtrip() {
    let output = PrimitiveOutput {
        values: {
            let mut m = HashMap::new();
            m.insert("id".to_string(), serde_json::json!("abc-123"));
            m
        },
        summary: "created record".to_string(),
    };
    let json = serde_json::to_string(&output).unwrap();
    let deserialized: PrimitiveOutput = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.summary, "created record");
    assert_eq!(deserialized.values["id"], "abc-123");
}

#[test]
fn test_idempotency_serde_roundtrip() {
    let val = Idempotency::GuardRequired;
    let json = serde_json::to_string(&val).unwrap();
    let deserialized: Idempotency = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, Idempotency::GuardRequired);
}

#[test]
fn test_output_field_serde_roundtrip() {
    let field = OutputField {
        name: "session-id".to_string(),
        field_type: OutputType::String,
    };
    let json = serde_json::to_string(&field).unwrap();
    let deserialized: OutputField = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.name, "session-id");
    assert_eq!(deserialized.field_type, OutputType::String);
}

#[test]
fn test_input_field_serde_roundtrip() {
    let field = InputField {
        name: "work-id".to_string(),
        field_type: OutputType::String,
        required: true,
    };
    let json = serde_json::to_string(&field).unwrap();
    let deserialized: InputField = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.name, "work-id");
    assert!(deserialized.required);
}

#[test]
fn test_requires_git_lock_default_false() {
    let prim = EchoPrimitive;
    assert!(!prim.requires_git_lock());
}

#[test]
fn test_default_registry() {
    let reg = PrimitiveRegistry::default();
    assert!(reg.is_empty());
}
