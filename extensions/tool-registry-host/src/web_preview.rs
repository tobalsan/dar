use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use dar_extension_sdk::mcp_apps::ResultRecord;
use serde::Deserialize;
use serde_json::{json, Value};
use tool_registry::{ToolContent, ToolExecutor, ToolOutcome, ToolSpec};

pub(crate) fn spec() -> ToolSpec {
    ToolSpec::new(
        "web_preview",
        "Show a web page to the user in web chat, inline and in a side panel. Use it for a local dev server (`http://localhost:<port>`) or a public HTTPS page. The browser of the user loads the page. For an agent on another host, give a URL that the browser can reach, for example a tailnet HTTPS URL. Call it again with the same URL to update the view.",
        json!({
            "type": "object", "additionalProperties": false, "required": ["url"],
            "properties": {"url": {"type": "string"}, "title": {"type": "string"}}
        }),
    ).writes()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    url: String,
    title: Option<String>,
}

pub(crate) struct WebPreview {
    pub root: PathBuf,
}

#[async_trait::async_trait]
impl ToolExecutor for WebPreview {
    async fn execute(&self, input: Value) -> Result<ToolOutcome> {
        let args: Args = serde_json::from_value(input.clone())?;
        let url = reqwest::Url::parse(&args.url)
            .map_err(|_| anyhow::anyhow!("web_preview requires a valid HTTP or HTTPS URL"))?;
        validate_url(&url)?;
        let last_url = Arc::new(Mutex::new(url.clone()));
        let redirect_url = Arc::clone(&last_url);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > 5 || validate_url(attempt.url()).is_err() {
                    return attempt.error("web_preview redirect refused or exceeds five redirects");
                }
                *redirect_url.lock().expect("redirect URL lock") = attempt.url().clone();
                attempt.follow()
            }))
            .referer(false)
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        let (final_url, status, reason) = match client.get(url.clone()).send().await {
            Ok(response) => (
                response.url().as_str().to_owned(),
                Some(response.status().as_u16()),
                framing_refusal(response.headers()),
            ),
            Err(error) if error.is_redirect() => {
                bail!("web_preview redirect refused or exceeds five redirects");
            }
            Err(_) => (
                last_url.lock().expect("redirect URL lock").to_string(),
                None,
                Some("unreachable"),
            ),
        };
        let text = match reason {
            Some("unreachable") => format!(
                "web preview: {final_url} is not reachable from the agent host, link card shown"
            ),
            Some(_) => format!("web preview: {final_url} refuses framing, link card shown"),
            None => format!("web preview shown: {final_url}"),
        };
        let mut outcome = ToolOutcome::ok(text);
        let mut result = outcome.to_mcp_result();
        result["structuredContent"] = json!({
            "url": args.url, "finalUrl": final_url, "title": args.title,
            "frameable": reason.is_none(), "reason": reason, "status": status,
        });
        let record = ResultRecord {
            tool: "web_preview".into(),
            pi_tool_name: Some("mcp__dar__web_preview".into()),
            server: "dar".into(),
            resource_uri: "ui://dar/web-preview".into(),
            args: input,
            result,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        match record.write(&self.root) {
            Ok(id) => {
                outcome.text.push_str(&format!("\n[mcp-app-result:{id}]"));
                outcome.content[0] = ToolContent::Text {
                    text: outcome.text.clone(),
                };
            }
            Err(_) => dar_extension_sdk::log::event(
                "",
                "web-preview.capture-failed",
                "Web preview result capture failed; using text fallback",
            ),
        }
        Ok(outcome)
    }
}

fn validate_url(url: &reqwest::Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        bail!("web_preview URLs must not contain a user name or password");
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => Ok(()),
        _ => bail!("web_preview requires HTTPS, or HTTP on localhost, 127.0.0.1, or [::1]"),
    }
}

fn framing_refusal(headers: &reqwest::header::HeaderMap) -> Option<&'static str> {
    if headers.contains_key(reqwest::header::X_FRAME_OPTIONS) {
        return Some("x-frame-options");
    }
    for policy in headers.get_all(reqwest::header::CONTENT_SECURITY_POLICY) {
        for directive in policy.to_str().unwrap_or_default().split([';', ',']) {
            let mut words = directive.split_whitespace();
            if words
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("frame-ancestors"))
                && !(words.next() == Some("*") && words.next().is_none())
            {
                return Some("frame-ancestors");
            }
        }
    }
    None
}
