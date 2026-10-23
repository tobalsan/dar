//! Runner-independent MCP Apps catalog and result transport.
use anyhow::Result;
use serde_json::Value;
use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub const MCP_APPS_SERVICE: &str = "mcp.apps";

/// Access to configured servers. Tool calls are limited to app-visible tools
/// on the named server; callers never receive transports or credentials.
pub trait McpApps: Send + Sync {
    /// Array of `{name, server, resourceUri, visibility, meta}` UI tools.
    fn list_tools(&self) -> BoxFuture<'_, Result<Value>>;
    /// Returns `{html, meta}`, preserving the resource's complete `_meta`.
    fn read_ui_resource<'a>(
        &'a self,
        server: &'a str,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value>>;
    fn call_tool<'a>(
        &'a self,
        server: &'a str,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value>>;
    /// Returns the unmodified MCP resources/read result.
    fn read_resource<'a>(&'a self, server: &'a str, uri: &'a str) -> BoxFuture<'a, Result<Value>>;
}

/// Full upstream output captured by the bridge, correlated by an opaque id.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ResultRecord {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_tool_name: Option<String>,
    pub server: String,
    pub resource_uri: String,
    pub args: Value,
    pub result: Value,
    pub created_at: u64,
}

fn valid_id(id: &str) -> bool {
    if id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return true;
    }
    id.split_once('~').is_some_and(|(first, second)| {
        [first, second]
            .iter()
            .all(|part| uuid::Uuid::parse_str(part).is_ok() && part.len() == 36)
    })
}

/// Hide marker substrings even in JSON-escaped runner output. Correlation is
/// allowed only for exactly one marker with a valid opaque id.
pub fn extract_marker(text: &str) -> (String, Option<String>) {
    const PREFIX: &str = "[mcp-app-result:";
    let mut clean = String::new();
    let mut rest = text;
    let mut markers = Vec::new();
    while let Some(start) = rest.find(PREFIX) {
        clean.push_str(&rest[..start]);
        let marker = &rest[start + PREFIX.len()..];
        let Some(end) = marker.find(']') else {
            clean.push_str(&rest[start..]);
            rest = "";
            break;
        };
        markers.push(marker[..end].to_owned());
        rest = &marker[end + 1..];
    }
    clean.push_str(rest);
    let id = if markers.len() == 1 && valid_id(&markers[0]) {
        markers.pop()
    } else {
        None
    };
    (clean.trim_end().to_owned(), id)
}

/// Codex adds the outer bridge's server name before the mapped tool name.
pub fn normalize_tool_name(name: &str) -> &str {
    match name.split_once('.') {
        Some((_, mapped)) if mapped.contains("__") => mapped,
        _ => name,
    }
}

/// Missing, invalid or mismatched records deliberately retain the text fallback.
pub fn read_result(
    root: &std::path::Path,
    id: &str,
    tool_name: &str,
) -> Result<Option<ResultRecord>> {
    if !valid_id(id) {
        return Ok(None);
    }
    let path = root
        .join("data/mcp-apps/results")
        .join(format!("{id}.json"));
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record: ResultRecord = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(_) => return Ok(None),
    };
    Ok((record.tool == normalize_tool_name(tool_name)
        || record.pi_tool_name.as_deref() == Some(tool_name))
    .then_some(record))
}

impl ResultRecord {
    /// Creates a private result file before the bridge emits its marker.
    pub fn write(&self, root: &std::path::Path) -> Result<String> {
        use std::io::Write;
        let directory = root.join("data/mcp-apps/results");
        std::fs::create_dir_all(&directory)?;
        // Two UUIDs retain more than 128 random bits and survive the bridge's
        // existing credential-shape scrubber, which recognizes UUID identifiers.
        let id = format!("{}~{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(directory.join(format!("{id}.json")))?;
        file.write_all(&serde_json::to_vec(self)?)?;
        Ok(id)
    }
}

/// Called at boot; old transcripts retain their ordinary text fallback.
pub fn prune_results(root: &std::path::Path) -> Result<()> {
    let directory = root.join("data/mcp-apps/results");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<ResultRecord>(&bytes) else {
            continue;
        };
        if now.saturating_sub(record.created_at) > 30 * 24 * 60 * 60 {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}
