//! Substrate extension: assemble and publish the agent's system-file identity.
//!
//! Resolves the agent folder's `AGENTS.md` + configured `system_files` (plus any
//! workspace `skills/`) into the retained [`SYSTEM_CONTEXT_TOPIC`] payload once
//! at boot. It runs *before* the orchestrator loop and every consumer (TUI chat,
//! issue runner, scheduler/IRC surfaces), so identity is published regardless of
//! whether the orchestration loop is enabled — passive agents get the same
//! identity as tracker-driven ones.
//!
//! This extension owns only identity-context policy. It does not touch the run
//! loop, tracker, or dispatch; the orchestrator reads the published context as a
//! consumer rather than resolving it itself. Non-agent surfaces (`dar dash`,
//! scaffolding, tests) can omit this extension entirely, keeping that policy out
//! of `dar-host` core.

use std::path::Path;

use anyhow::{Context, Result};
use host_api::{Extension, RegisterCtx, StartCtx};
use system_files::bus::{SystemContext, SYSTEM_CONTEXT_TOPIC};
use system_files::{ResolveError, SystemFileEntry};

mod resolver;

pub use system_files::bus::{SystemContext as SystemContextPayload, SYSTEM_CONTEXT_TOPIC as TOPIC};

pub struct SystemContextExtension;

impl Extension for SystemContextExtension {
    fn id(&self) -> &'static str {
        "system-context"
    }

    fn register<'a>(&'a self, ctx: &'a mut RegisterCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Register the retained topic with an inert empty default. Publishing
            // the assembled context happens in `start`, after every extension has
            // registered, but before consumers' `start` reads it.
            ctx.bus
                .register_retained(SYSTEM_CONTEXT_TOPIC, SystemContext::default())?;
            Ok(())
        })
    }

    fn start<'a>(&'a self, ctx: StartCtx) -> host_api::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let root = ctx.paths.root();
            let context = resolver::resolve_for(root);
            ctx.host
                .bus
                .publish(SYSTEM_CONTEXT_TOPIC, context)
                .context("publishing system context")?;
            Ok(())
        })
    }
}

/// Read the agent's declared `system_files` from `agent.yaml`, resolve them
/// (with `AGENTS.md` first and workspace skills appended), and project to the
/// retained-topic payload. A missing/unparseable `agent.yaml` or a resolution
/// error degrades to an empty context (boot continues; `doctor`/preflight gate
/// hard failures) — matching the prior orchestrator behaviour.
pub fn resolve_for(root: &Path) -> SystemContext {
    resolver::resolve_for(root)
}

/// Resolve `AGENTS.md` + the given `system_files` entries into the bus payload,
/// surfacing any [`ResolveError`]. Used by `doctor`/preflight as the hard gate
/// that fails on missing `required` files or containment violations.
pub fn resolve(
    root: &Path,
    entries: Option<&[SystemFileEntry]>,
) -> Result<SystemContext, ResolveError> {
    resolver::resolve(root, entries)
}
