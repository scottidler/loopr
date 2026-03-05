pub mod edit;
pub mod fetch;
pub mod find;
pub mod glob;
pub mod grep;
pub mod list;
pub mod plan;
pub mod read;
pub mod search;
pub mod shell;
pub mod slash;
pub mod todo;
pub mod tree;
pub mod write;

use crate::tools::traits::Tool;

/// Return all built-in tool instances.
pub fn all_builtins() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(read::ReadTool),
        Box::new(write::WriteTool),
        Box::new(edit::EditTool),
        Box::new(list::ListTool),
        Box::new(tree::TreeTool),
        Box::new(glob::GlobTool),
        Box::new(grep::GrepTool),
        Box::new(find::FindTool),
        Box::new(shell::ShellTool),
        Box::new(slash::SlashTool),
        Box::new(fetch::FetchTool),
        Box::new(search::SearchTool),
        Box::new(todo::TodoTool),
        Box::new(plan::PlanTool),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_builtins_count() {
        let tools = all_builtins();
        assert_eq!(tools.len(), 14);
    }

    #[test]
    fn test_all_builtins_unique_names() {
        let tools = all_builtins();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "tool names must be unique");
    }

    #[test]
    fn test_all_builtins_have_schemas() {
        let tools = all_builtins();
        for tool in &tools {
            let schema = tool.input_schema();
            assert_eq!(
                schema["type"],
                "object",
                "tool '{}' schema must be object type",
                tool.name()
            );
        }
    }
}
