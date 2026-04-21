use schemars::{JsonSchema, schema_for};
use serde::Serialize;

use crate::tool::Tool;

#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
}

pub fn for_tool<T: Tool>() -> ToolSchema {
    let root = schema_for!(T::Input);
    let value = serde_json::to_value(&root).expect("schemars output must be convertible to serde_json::Value");
    ToolSchema {
        name: T::name(),
        description: T::description(),
        input_schema: value,
    }
}

pub fn schema_value<I: JsonSchema>() -> serde_json::Value {
    let root = schema_for!(I);
    serde_json::to_value(&root).expect("schemars output must be convertible to serde_json::Value")
}

#[cfg(test)]
mod tests;
