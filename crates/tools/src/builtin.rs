pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
mod path;
pub mod read;
pub mod write;

pub struct Read;
pub struct Write;
pub struct Edit;
pub struct Bash;
pub struct Grep;
pub struct Glob;

use crate::error::ToolError;
use crate::lane::Lane;
use crate::schema::{ToolSchema, for_tool};
use crate::tool::{Tool, ToolContext};

impl Tool for Read {
    type Input = read::Input;
    type Output = read::Output;
    type Error = read::Error;

    fn name() -> &'static str {
        "read"
    }
    fn description() -> &'static str {
        read::DESCRIPTION
    }
    fn lane() -> Lane {
        Lane::Local
    }
    fn schema() -> ToolSchema {
        for_tool::<Self>()
    }
    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error> {
        read::execute(input, ctx).await
    }
}

impl Tool for Write {
    type Input = write::Input;
    type Output = write::Output;
    type Error = write::Error;

    fn name() -> &'static str {
        "write"
    }
    fn description() -> &'static str {
        write::DESCRIPTION
    }
    fn lane() -> Lane {
        Lane::Local
    }
    fn schema() -> ToolSchema {
        for_tool::<Self>()
    }
    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error> {
        write::execute(input, ctx).await
    }
}

impl Tool for Edit {
    type Input = edit::Input;
    type Output = edit::Output;
    type Error = edit::Error;

    fn name() -> &'static str {
        "edit"
    }
    fn description() -> &'static str {
        edit::DESCRIPTION
    }
    fn lane() -> Lane {
        Lane::Local
    }
    fn schema() -> ToolSchema {
        for_tool::<Self>()
    }
    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error> {
        edit::execute(input, ctx).await
    }
}

impl Tool for Bash {
    type Input = bash::Input;
    type Output = bash::Output;
    type Error = ToolError;

    fn name() -> &'static str {
        "bash"
    }
    fn description() -> &'static str {
        bash::DESCRIPTION
    }
    fn lane() -> Lane {
        Lane::Net
    }
    fn schema() -> ToolSchema {
        for_tool::<Self>()
    }
    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error> {
        bash::execute(input, ctx).await
    }
}

impl Tool for Grep {
    type Input = grep::Input;
    type Output = grep::Output;
    type Error = ToolError;

    fn name() -> &'static str {
        "grep"
    }
    fn description() -> &'static str {
        grep::DESCRIPTION
    }
    fn lane() -> Lane {
        Lane::Local
    }
    fn schema() -> ToolSchema {
        for_tool::<Self>()
    }
    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error> {
        grep::execute(input, ctx).await
    }
}

impl Tool for Glob {
    type Input = glob::Input;
    type Output = glob::Output;
    type Error = glob::Error;

    fn name() -> &'static str {
        "glob"
    }
    fn description() -> &'static str {
        glob::DESCRIPTION
    }
    fn lane() -> Lane {
        Lane::Local
    }
    fn schema() -> ToolSchema {
        for_tool::<Self>()
    }
    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error> {
        glob::execute(input, ctx).await
    }
}
