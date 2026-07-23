//! Runtime delivery contract for communication-channel extensions.
//!
//! Register a `dyn DeliverySink` in the host service registry under the
//! extension's stable id (for example `"slack"`). Scheduler jobs then resolve
//! that id and deliver their completed result without involving an agent tool.

use async_trait::async_trait;

/// A scheduler-owned, channel-neutral destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub channel: Option<String>,
    pub user: Option<String>,
}

/// A communication extension's deterministic result-delivery path.
#[async_trait]
pub trait DeliverySink: Send + Sync {
    async fn deliver(&self, dest: &Destination, text: &str) -> anyhow::Result<()>;
}
