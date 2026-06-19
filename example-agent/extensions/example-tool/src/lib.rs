//! `example-tool` — a toy background extension used as a tool-registry FIXTURE.
//!
//! It registers a single `echo_upper` tool (uppercase the input text) against
//! the host [`ToolRegistry`]. This is the registration-surface analogue of the
//! `fake` runner: it proves the minimal "extension registers a tool" contract
//! and powers the codex end-to-end run and the manual verification in the issue.
//!
//! It is wired only into the `example-agent` composition (a local discovered
//! extension), never the shipped `dist` tool set.

use std::sync::Arc;

use anyhow::{Context, Result};
use host_api::{Extension, RegisterCtx};
use serde_json::{json, Value};
use tool_registry::{
    ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec, TOOL_REGISTRY_SERVICE,
};

pub fn extension() -> Box<dyn Extension> {
    Box::new(ExampleToolExtension)
}

pub struct ExampleToolExtension;

impl Extension for ExampleToolExtension {
    fn id(&self) -> &'static str {
        "example-tool"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let registry = ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
                .context("example-tool requires the tool-registry-host extension")?;
            register_into(registry.as_ref())
        })
    }
}

/// Register the toy `echo_upper` tool into a registry. Shared by the extension
/// `register()` pass and the standalone example bridge binary/tests.
pub fn register_into(registry: &dyn ToolRegistryHandle) -> Result<()> {
    registry.register_tool(
        ToolSpec::new(
            "echo_upper",
            "Return the input text uppercased. A toy host tool used to \
             prove the extension tool registry + MCP bridge end to end.",
            json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to uppercase.",
                    }
                },
                "required": ["text"],
                "additionalProperties": false,
            }),
        ),
        Arc::new(EchoUpper),
    )
}

struct EchoUpper;

#[async_trait::async_trait]
impl ToolExecutor for EchoUpper {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            // Bad arguments are a structured failure, not a host fault.
            return Ok(ToolOutcome::error(
                "echo_upper requires a 'text' string argument",
            ));
        };
        Ok(ToolOutcome::ok(text.to_uppercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_upper_uppercases() {
        let out = EchoUpper
            .execute(json!({ "text": "hello from spike" }))
            .await
            .unwrap();
        assert_eq!(out, ToolOutcome::ok("HELLO FROM SPIKE"));
    }

    #[tokio::test]
    async fn echo_upper_missing_arg_is_structured_error() {
        let out = EchoUpper.execute(json!({})).await.unwrap();
        assert!(out.is_error);
    }
}
