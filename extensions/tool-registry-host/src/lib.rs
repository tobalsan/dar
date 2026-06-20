//! Host tool-registry extension.
//!
//! Publishes the single shared [`ToolRegistry`] as a service under
//! [`TOOL_REGISTRY_SERVICE`] during `register()`. It must run *before* any
//! tool-providing extension (so they can resolve it to register their tools)
//! and before the runner/bridge consumers that read it — i.e. early in the
//! composition list.
//!
//! This extension owns no tools itself; it is the registration substrate. The
//! host MCP bridge subcommand instantiates the same composition to obtain a
//! populated registry and serve it over MCP stdio.

use std::sync::Arc;

use anyhow::Result;
use host_api::{Extension, RegisterCtx};
use tool_registry::{ToolRegistry, ToolRegistryHandle, TOOL_REGISTRY_SERVICE};

pub struct ToolRegistryHostExtension;

impl Extension for ToolRegistryHostExtension {
    fn id(&self) -> &'static str {
        "tool-registry-host"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let registry: Arc<dyn ToolRegistryHandle> = Arc::new(ToolRegistry::new());
            ctx.services
                .service::<dyn ToolRegistryHandle>(TOOL_REGISTRY_SERVICE, registry)?;
            Ok(())
        })
    }
}
