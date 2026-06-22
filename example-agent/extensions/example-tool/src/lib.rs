//! `example-tool` — a toy background extension used as a tool-registry FIXTURE.
//!
//! It registers a single `echo_upper` tool (uppercase the input text) against
//! the host [`ToolRegistry`]. This is the registration-surface analogue of the
//! `fake` runner: it proves the minimal "extension registers a tool" contract
//! and powers the codex end-to-end run and the manual verification in the issue.
//!
//! It is wired only into the `example-agent` composition (a local discovered
//! extension), never the shipped `dist` tool set.
//!
//! The tool deliberately reads its own `extensions.example-tool` config during
//! `register()` (a `suffix` appended to the uppercased text). That makes it a
//! genuine guard for config parity: a registry built with an empty config store
//! (as a naive bridge would) produces a *different* result than one fed the real
//! `agent.yaml` config, so the codex e2e fails loudly if the host MCP bridge
//! ever stops threading extension config into the `register()` pass.

use std::sync::Arc;

use anyhow::{Context, Result};
use host_api::{Extension, RegisterCtx};
use serde::Deserialize;
use serde_json::{json, Value};
use tool_registry::{
    ToolExecutor, ToolOutcome, ToolRegistryHandle, ToolSpec, TOOL_REGISTRY_SERVICE,
};

pub fn extension() -> Box<dyn Extension> {
    Box::new(ExampleToolExtension)
}

pub struct ExampleToolExtension;

/// Per-extension config read from `extensions.example-tool` in `agent.yaml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExampleToolConfig {
    /// Appended verbatim to the uppercased text. Lets the e2e prove that the
    /// host MCP bridge fed real config into the tool's `register()` pass.
    #[serde(default)]
    pub suffix: String,
}

impl Extension for ExampleToolExtension {
    fn id(&self) -> &'static str {
        "example-tool"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let config = match ctx.config.get(self.id()) {
                Some(value) => serde_json::from_value::<ExampleToolConfig>(value.clone())
                    .context("parsing extensions.example-tool config")?,
                None => ExampleToolConfig::default(),
            };
            let registry = ctx
                .services
                .get_named::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE)
                .context("example-tool requires the tool-registry-host extension")?;
            register_into_with_config(registry.as_ref(), config)
        })
    }
}

/// Register the toy `echo_upper` tool with default (empty) config. Shared by
/// tests that don't exercise the config path.
pub fn register_into(registry: &dyn ToolRegistryHandle) -> Result<()> {
    register_into_with_config(registry, ExampleToolConfig::default())
}

/// Register the toy `echo_upper` tool, baking the resolved config into the
/// executor. Shared by the extension `register()` pass and the standalone
/// example bridge binary/tests.
pub fn register_into_with_config(
    registry: &dyn ToolRegistryHandle,
    config: ExampleToolConfig,
) -> Result<()> {
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
        Arc::new(EchoUpper {
            suffix: config.suffix,
        }),
    )
}

struct EchoUpper {
    suffix: String,
}

#[async_trait::async_trait]
impl ToolExecutor for EchoUpper {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            // Bad arguments are a structured failure, not a host fault.
            return Ok(ToolOutcome::error(
                "echo_upper requires a 'text' string argument",
            ));
        };
        Ok(ToolOutcome::ok(format!(
            "{}{}",
            text.to_uppercase(),
            self.suffix
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_upper_uppercases() {
        let out = EchoUpper {
            suffix: String::new(),
        }
        .execute(json!({ "text": "hello from spike" }))
        .await
        .unwrap();
        assert_eq!(out, ToolOutcome::ok("HELLO FROM SPIKE"));
    }

    #[tokio::test]
    async fn echo_upper_missing_arg_is_structured_error() {
        let out = EchoUpper {
            suffix: String::new(),
        }
        .execute(json!({}))
        .await
        .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn echo_upper_appends_configured_suffix() {
        // The config read during register() must reach the executor — this is
        // the contract the host MCP bridge's config parity guarantees.
        let out = EchoUpper {
            suffix: " [cfg]".to_string(),
        }
        .execute(json!({ "text": "hi" }))
        .await
        .unwrap();
        assert_eq!(out, ToolOutcome::ok("HI [cfg]"));
    }
}
