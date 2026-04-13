use std::collections::HashMap;

use serde_json::Value;

/// Resolve `$trigger.*` and `$context.*` references in a params map.
///
/// Each string value that starts with `$` is treated as a reference:
/// - `$trigger.scope-id` -> the current scope ID string
/// - `$trigger.event` -> the full trigger event payload
/// - `$trigger.event.{field}` -> a specific field from the event payload
/// - `$context.{step-name}.{field}` -> output from a named step
///
/// Non-string values and strings not starting with `$` are passed through unchanged.
/// References that fail to resolve return `Err` with a descriptive message.
pub fn resolve_params(
    params: &HashMap<String, Value>,
    scope_id: &str,
    trigger_payload: Option<&Value>,
    strategy_ctx: &HashMap<String, Value>,
) -> eyre::Result<Value> {
    let mut resolved = serde_json::Map::new();
    for (key, val) in params {
        let resolved_val = resolve_value(val, scope_id, trigger_payload, strategy_ctx)?;
        resolved.insert(key.clone(), resolved_val);
    }
    Ok(Value::Object(resolved))
}

fn resolve_value(
    val: &Value,
    scope_id: &str,
    trigger_payload: Option<&Value>,
    strategy_ctx: &HashMap<String, Value>,
) -> eyre::Result<Value> {
    match val {
        Value::String(s) if s.starts_with('$') => resolve_reference(s, scope_id, trigger_payload, strategy_ctx),
        Value::Array(arr) => {
            let resolved: Result<Vec<Value>, _> = arr
                .iter()
                .map(|v| resolve_value(v, scope_id, trigger_payload, strategy_ctx))
                .collect();
            Ok(Value::Array(resolved?))
        }
        Value::Object(map) => {
            let mut resolved = serde_json::Map::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_value(v, scope_id, trigger_payload, strategy_ctx)?);
            }
            Ok(Value::Object(resolved))
        }
        other => Ok(other.clone()),
    }
}

fn resolve_reference(
    reference: &str,
    scope_id: &str,
    trigger_payload: Option<&Value>,
    strategy_ctx: &HashMap<String, Value>,
) -> eyre::Result<Value> {
    if let Some(rest) = reference.strip_prefix("$trigger.") {
        resolve_trigger_ref(rest, scope_id, trigger_payload)
    } else if let Some(rest) = reference.strip_prefix("$context.") {
        resolve_context_ref(rest, strategy_ctx)
    } else if let Some(rest) = reference.strip_prefix("$config.") {
        // Config references are not yet implemented; return the path as a string
        // so strategies can be parsed without error. Phase 4 wires this to Config.
        Ok(Value::String(format!("$config.{rest}")))
    } else {
        eyre::bail!("unknown reference prefix: '{reference}'")
    }
}

fn resolve_trigger_ref(path: &str, scope_id: &str, trigger_payload: Option<&Value>) -> eyre::Result<Value> {
    match path {
        "scope-id" => Ok(Value::String(scope_id.to_owned())),
        "event" => trigger_payload
            .cloned()
            .ok_or_else(|| eyre::eyre!("$trigger.event: no event payload available")),
        _ if path.starts_with("event.") => {
            let field = &path["event.".len()..];
            let payload = trigger_payload.ok_or_else(|| eyre::eyre!("$trigger.event.{field}: no event payload"))?;
            // Normalize kebab-case field to snake_case for JSON lookup
            let json_key = field.replace('-', "_");
            payload
                .get(&json_key)
                .or_else(|| payload.get(field))
                .cloned()
                .ok_or_else(|| eyre::eyre!("$trigger.event.{field}: field not found in payload"))
        }
        _ => eyre::bail!("unknown $trigger path: '{path}'"),
    }
}

fn resolve_context_ref(path: &str, strategy_ctx: &HashMap<String, Value>) -> eyre::Result<Value> {
    let dot_pos = path
        .find('.')
        .ok_or_else(|| eyre::eyre!("malformed $context reference: '$context.{path}' (missing .field suffix)"))?;
    let step_name = &path[..dot_pos];
    let field = &path[dot_pos + 1..];

    let step_output = strategy_ctx
        .get(step_name)
        .ok_or_else(|| eyre::eyre!("$context.{step_name}.{field}: step '{step_name}' has no output"))?;

    let obj = step_output
        .as_object()
        .ok_or_else(|| eyre::eyre!("$context.{step_name}.{field}: step output is not an object"))?;

    // Try both kebab-case and snake_case
    let json_key = field.replace('-', "_");
    obj.get(&json_key)
        .or_else(|| obj.get(field))
        .cloned()
        .ok_or_else(|| eyre::eyre!("$context.{step_name}.{field}: field not found in step output"))
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_trigger_scope_id() {
        let params: HashMap<String, Value> =
            [("work-id".to_owned(), Value::String("$trigger.scope-id".to_owned()))].into();
        let resolved = resolve_params(&params, "wi-123", None, &HashMap::new()).unwrap();
        assert_eq!(resolved["work-id"], "wi-123");
    }

    #[test]
    fn resolve_trigger_event_field() {
        let payload = serde_json::json!({"work_id": "wi-456", "status": "failed"});
        let params: HashMap<String, Value> =
            [("id".to_owned(), Value::String("$trigger.event.work-id".to_owned()))].into();
        let resolved = resolve_params(&params, "s-1", Some(&payload), &HashMap::new()).unwrap();
        assert_eq!(resolved["id"], "wi-456");
    }

    #[test]
    fn resolve_trigger_event_whole() {
        let payload = serde_json::json!({"status": "failed"});
        let params: HashMap<String, Value> = [("data".to_owned(), Value::String("$trigger.event".to_owned()))].into();
        let resolved = resolve_params(&params, "s-1", Some(&payload), &HashMap::new()).unwrap();
        assert_eq!(resolved["data"], payload);
    }

    #[test]
    fn resolve_context_ref_step_output() {
        let mut ctx = HashMap::new();
        ctx.insert(
            "threshold-check".to_owned(),
            serde_json::json!({"exceeded": true, "count": 3}),
        );
        let params: HashMap<String, Value> = [(
            "exceeded".to_owned(),
            Value::String("$context.threshold-check.exceeded".to_owned()),
        )]
        .into();
        let resolved = resolve_params(&params, "wi-1", None, &ctx).unwrap();
        assert_eq!(resolved["exceeded"], true);
    }

    #[test]
    fn resolve_literal_passthrough() {
        let params: HashMap<String, Value> = [
            ("name".to_owned(), Value::String("literal".to_owned())),
            ("count".to_owned(), serde_json::json!(42)),
            ("flag".to_owned(), serde_json::json!(true)),
        ]
        .into();
        let resolved = resolve_params(&params, "wi-1", None, &HashMap::new()).unwrap();
        assert_eq!(resolved["name"], "literal");
        assert_eq!(resolved["count"], 42);
        assert_eq!(resolved["flag"], true);
    }

    #[test]
    fn resolve_missing_trigger_payload_fails() {
        let params: HashMap<String, Value> = [("data".to_owned(), Value::String("$trigger.event".to_owned()))].into();
        let result = resolve_params(&params, "wi-1", None, &HashMap::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no event payload"));
    }

    #[test]
    fn resolve_missing_context_step_fails() {
        let params: HashMap<String, Value> =
            [("val".to_owned(), Value::String("$context.nonexistent.field".to_owned()))].into();
        let result = resolve_params(&params, "wi-1", None, &HashMap::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no output"));
    }

    #[test]
    fn resolve_nested_array_values() {
        let params: HashMap<String, Value> =
            [("ids".to_owned(), serde_json::json!(["$trigger.scope-id", "literal"]))].into();
        let resolved = resolve_params(&params, "wi-99", None, &HashMap::new()).unwrap();
        let arr = resolved["ids"].as_array().unwrap();
        assert_eq!(arr[0], "wi-99");
        assert_eq!(arr[1], "literal");
    }
}
