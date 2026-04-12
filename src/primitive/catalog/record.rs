use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Read/filter records from a collection. Pure query, no side effects.
pub struct QueryRecords;

impl Primitive for QueryRecords {
    fn name(&self) -> &'static str {
        "query-records"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let collection = params["collection"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'collection'"))?;
            let filters = params.get("filters");

            debug!("query-records: collection={} filters={:?}", collection, filters);

            let records = match collection {
                "plan" => to_json_array(&*ctx.stores.read_plans()?, filters),
                "spec" => to_json_array(&*ctx.stores.read_specs()?, filters),
                "phase" => to_json_array(&*ctx.stores.read_phases()?, filters),
                "work" => to_json_array(&*ctx.stores.read_works()?, filters),
                "bundle" => to_json_array(&*ctx.stores.read_bundles()?, filters),
                "tick" => to_json_array(&*ctx.stores.read_ticks()?, filters),
                "learning" => to_json_array(&*ctx.stores.read_learnings()?, filters),
                "lock" => to_json_array(&*ctx.stores.read_locks()?, filters),
                other => {
                    eyre::bail!("unknown collection: '{}'", other)
                }
            };

            let count = records.as_array().map(|a| a.len()).unwrap_or(0);
            let mut values = HashMap::new();
            values.insert("records".to_string(), records);

            Ok(PrimitiveOutput {
                values,
                summary: format!("queried {}: {} records", collection, count),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "records".to_string(),
            field_type: OutputType::Json,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "collection".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "filters".to_string(),
                field_type: OutputType::Json,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Read a single record by ID. Pure query, no side effects.
pub struct GetRecord;

impl Primitive for GetRecord {
    fn name(&self) -> &'static str {
        "get-record"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let collection = params["collection"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'collection'"))?;
            let id = params["id"].as_str().ok_or_else(|| eyre::eyre!("missing 'id'"))?;

            debug!("get-record: collection={} id={}", collection, id);

            let record = match collection {
                "plan" => get_one(&*ctx.stores.read_plans()?, id),
                "spec" => get_one(&*ctx.stores.read_specs()?, id),
                "phase" => get_one(&*ctx.stores.read_phases()?, id),
                "work" => get_one(&*ctx.stores.read_works()?, id),
                "bundle" => get_one(&*ctx.stores.read_bundles()?, id),
                "tick" => get_one(&*ctx.stores.read_ticks()?, id),
                "learning" => get_one(&*ctx.stores.read_learnings()?, id),
                "lock" => get_one(&*ctx.stores.read_locks()?, id),
                other => {
                    eyre::bail!("unknown collection: '{}'", other)
                }
            };

            let record = record.ok_or_else(|| eyre::eyre!("{} '{}' not found", collection, id))?;

            let mut values = HashMap::new();
            values.insert("record".to_string(), record);

            Ok(PrimitiveOutput {
                values,
                summary: format!("fetched {} '{}'", collection, id),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "record".to_string(),
            field_type: OutputType::Json,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "collection".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

/// Serialize all records in a HashMap to a JSON array, optionally
/// filtering by field-value pairs in `filters`.
fn to_json_array<V: serde::Serialize>(
    map: &HashMap<String, V>,
    filters: Option<&serde_json::Value>,
) -> serde_json::Value {
    let records: Vec<serde_json::Value> = map
        .values()
        .filter_map(|v| serde_json::to_value(v).ok())
        .filter(|v| matches_filters(v, filters))
        .collect();
    serde_json::Value::Array(records)
}

/// Check if a JSON value matches all filter key-value pairs.
fn matches_filters(record: &serde_json::Value, filters: Option<&serde_json::Value>) -> bool {
    let Some(filter_obj) = filters.and_then(|f| f.as_object()) else {
        return true;
    };
    let Some(record_obj) = record.as_object() else {
        return false;
    };
    for (key, expected) in filter_obj {
        match record_obj.get(key) {
            Some(actual) if actual == expected => {}
            _ => return false,
        }
    }
    true
}

/// Look up a single record by ID from a HashMap, serializing to JSON.
fn get_one<V: serde::Serialize>(map: &HashMap<String, V>, id: &str) -> Option<serde_json::Value> {
    map.get(id).and_then(|v| serde_json::to_value(v).ok())
}
