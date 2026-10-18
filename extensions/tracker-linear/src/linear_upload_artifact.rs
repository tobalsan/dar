//! Upload verified host artifacts without exposing signed upload URLs.
use std::{io::Read, sync::Arc, time::Duration};

use anyhow::Result;
use dar_artifacts::{
    ArtifactDeliveryTarget, ArtifactError, ArtifactId, ArtifactStore, DeliveryClaimResult,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tool_registry::{Redactor, ToolExecutor, ToolOutcome, ToolSpec};

use crate::linear_graphql::AuthSource;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadArgs {
    artifact: String,
    issue: String,
    title: Option<String>,
}

struct UploadError {
    code: &'static str,
    message: String,
}
type UploadResult<T> = std::result::Result<T, UploadError>;

impl UploadError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<ArtifactError> for UploadError {
    fn from(error: ArtifactError) -> Self {
        let code = match error {
            ArtifactError::NotFound => "artifact_not_found",
            ArtifactError::InvalidArtifact
            | ArtifactError::TooLarge
            | ArtifactError::VerificationFailed => "artifact_invalid",
            ArtifactError::InvalidSource | ArtifactError::Io => "artifact_store_error",
        };
        Self::new(code, error.to_string())
    }
}

pub struct LinearUploadArtifactTool {
    client: reqwest::Client,
    endpoint: String,
    auth: AuthSource,
    store: Arc<ArtifactStore>,
    deadline: Duration,
}

impl LinearUploadArtifactTool {
    pub fn new(endpoint: String, auth: AuthSource, store: Arc<ArtifactStore>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            endpoint,
            auth,
            store,
            deadline: Duration::from_secs(120),
        })
    }

    pub fn spec() -> ToolSpec {
        ToolSpec::new(
            "linear_upload_artifact",
            "Upload a verified dar-artifact:// URI to a Linear issue's Resources \
             using host-held auth. Returns assetUrl and attachmentId; signed \
             upload URLs remain inside the host.",
            json!({
                "type": "object",
                "properties": {
                    "artifact": { "type": "string" },
                    "issue": { "type": "string" },
                    "title": { "type": "string" }
                },
                "required": ["artifact", "issue"],
                "additionalProperties": false
            }),
        )
        .with_access(false, true)
    }

    async fn graphql(
        &self,
        auth: &str,
        redactor: &Redactor,
        query: &str,
        variables: Value,
    ) -> UploadResult<Value> {
        let response = self
            .client
            .post(&self.endpoint)
            .header("Authorization", auth)
            .json(&json!({"query":query,"variables":variables}))
            .send()
            .await
            .map_err(|_| UploadError::new("transport_error", "Linear GraphQL transport error"))?;
        let status = response.status();
        let text = response.text().await.map_err(|_| {
            UploadError::new(
                "response_read_error",
                "failed reading Linear GraphQL response",
            )
        })?;
        if !status.is_success() {
            return Err(UploadError::new(
                "http_error",
                format!(
                    "Linear HTTP {}: {}",
                    status.as_u16(),
                    redactor.redact(&text)
                ),
            ));
        }
        let response: Value = serde_json::from_str(&text).map_err(|_| {
            UploadError::new("non_json_response", "Linear returned a non-JSON response")
        })?;
        if let Some(errors) = response.get("errors") {
            if errors.as_array().map(|a| !a.is_empty()).unwrap_or(true) {
                return Err(UploadError::new(
                    "graphql_error",
                    redactor.redact(&errors.to_string()),
                ));
            }
        }
        Ok(response["data"].clone())
    }
}

#[async_trait::async_trait]
impl ToolExecutor for LinearUploadArtifactTool {
    async fn execute(&self, args: Value) -> Result<ToolOutcome> {
        Ok(match self.run(args).await {
            Ok(outcome) => outcome,
            Err(error) => ToolOutcome::error_code(error.code, error.message, None::<String>),
        })
    }
}

impl LinearUploadArtifactTool {
    async fn run(&self, args: Value) -> UploadResult<ToolOutcome> {
        if args.get("title").is_some_and(|title| !title.is_string()) {
            return Err(UploadError::new("invalid_args", "title must be a string"));
        }
        let args: UploadArgs = serde_json::from_value(args).map_err(|_| {
            UploadError::new(
                "invalid_args",
                "requires artifact and issue strings, and optional title",
            )
        })?;
        let id: ArtifactId = args
            .artifact
            .strip_prefix("dar-artifact://")
            .unwrap_or("")
            .parse()
            .map_err(|_| {
                UploadError::new(
                    "invalid_args",
                    "artifact must be a dar-artifact:// UUID URI",
                )
            })?;
        if args.issue.trim().is_empty() {
            return Err(UploadError::new("invalid_args", "issue must not be empty"));
        }
        let auth = self.auth.resolve().ok_or_else(|| UploadError::new("missing_auth", "no Linear auth token: set LINEAR_OAUTH_TOKEN or LINEAR_API_KEY in the agent environment"))?;
        let mut secrets = match &self.auth {
            AuthSource::Env(Some(env)) => env.secret_values(),
            _ => Vec::new(),
        };
        secrets.push(auth.clone());
        if let Some(token) = auth.strip_prefix("Bearer ") {
            secrets.push(token.to_string());
        }
        let redactor = Redactor::from_secret_values(secrets.clone());
        let issue = tokio::time::timeout(
            Duration::from_secs(30),
            self.graphql(
                &auth,
                &redactor,
                "query($id: String!) { issue(id: $id) { id identifier } }",
                json!({"id":args.issue}),
            ),
        )
        .await
        .map_err(|_| UploadError::new("transport_error", "deadline exceeded"))??;
        if issue["issue"].is_null() {
            return Err(UploadError::new(
                "issue_not_found",
                "Linear issue not found",
            ));
        }
        let issue_id = required_string(&issue["issue"], "id")?;
        let identifier = required_string(&issue["issue"], "identifier")?;
        let claim = match self.store.claim_delivery(
            id,
            ArtifactDeliveryTarget {
                surface_id: "linear".into(),
                origin_destination: issue_id.into(),
            },
        )? {
            DeliveryClaimResult::Claimed(claim) => claim,
            DeliveryClaimResult::InProgress => {
                return Ok(ToolOutcome::error_code(
                    "upload_in_progress",
                    "upload in progress",
                    None::<String>,
                ))
            }
            DeliveryClaimResult::Delivered(delivery) => {
                let receipt: Value = serde_json::from_str(&delivery.remote_id).map_err(|_| {
                    UploadError::new("artifact_store_error", "invalid delivery receipt")
                })?;
                let metadata = self.store.metadata(id)?;
                return Ok(ToolOutcome::ok(json!({"assetUrl":receipt["assetUrl"],"attachmentId":receipt["attachmentId"],"issue":issue["issue"]["identifier"],"filename":metadata.filename,"bytes":metadata.bytes,"deduplicated":true}).to_string()));
            }
        };
        let deadline = tokio::time::Instant::now() + self.deadline;
        let result = tokio::time::timeout_at(deadline, async {
            let mut artifact = self.store.open_verified(id)?;
            let metadata = artifact.metadata().clone();
            if metadata.bytes > 25 * 1024 * 1024 {
                return Err(UploadError::new("artifact_invalid", "artifact exceeds 25 MiB upload limit"));
            }
            let mut bytes = Vec::new();
            artifact.read_to_end(&mut bytes).map_err(|_| {
                UploadError::new("artifact_store_error", "failed reading verified artifact")
            })?;
            if tokio::time::Instant::now() >= deadline {
                return Err(UploadError::new("transport_error", "deadline exceeded"));
            }
            let media_type = metadata.media_type.as_deref().unwrap_or("application/octet-stream");
            let upload = self.graphql(
                &auth,
                &redactor,
                "mutation($contentType: String!, $filename: String!, $size: Int!) { \
                 fileUpload(contentType: $contentType, filename: $filename, size: $size) { \
                 success uploadFile { uploadUrl assetUrl headers { key value } } } }",
                json!({"contentType":media_type,"filename":metadata.filename,"size":metadata.bytes}),
            ).await?;
            if upload["fileUpload"]["success"] != true {
                return Err(UploadError::new("invalid_response", "fileUpload was not successful"));
            }
            let upload = &upload["fileUpload"]["uploadFile"];
            let upload_url = required_string(upload, "uploadUrl")?;
            let asset_url = required_string(upload, "assetUrl")?;
            secrets.push(upload_url.into());
            secrets.extend(upload_url.split(['/', '?', '&', '=']).map(str::to_string));
            let upload_redactor = Redactor::from_secret_values(secrets);
            let mut request = self.client.put(upload_url);
            let headers = upload["headers"].as_array().ok_or_else(|| {
                UploadError::new("invalid_response", "upload headers missing")
            })?;
            for header in headers {
                request = request.header(required_string(header, "key")?, required_string(header, "value")?);
            }
            let response = request
                .header("Content-Type", media_type)
                .header("Cache-Control", "public, max-age=31536000")
                .body(bytes)
                .send()
                .await
                .map_err(|error| UploadError::new("upload_failed", if error.is_connect() {
                    "upload connection error"
                } else if error.is_timeout() {
                    "upload timeout"
                } else {
                    "upload request error"
                }))?;
            if !response.status().is_success() {
                return Err(UploadError::new("upload_failed", format!("upload HTTP {}", response.status().as_u16())));
            }
            let attachment = self.graphql(
                &auth,
                &upload_redactor,
                "mutation($input: AttachmentCreateInput!) { \
                 attachmentCreate(input: $input) { success attachment { id } } }",
                json!({"input":{
                    "issueId":issue_id,
                    "url":asset_url,
                    "title":args.title.unwrap_or(metadata.filename.clone()),
                    "subtitle":metadata.caption
                }}),
            ).await?;
            if attachment["attachmentCreate"]["success"] != true {
                return Err(UploadError::new("invalid_response", "attachmentCreate was not successful"));
            }
            let attachment_id = required_string(&attachment["attachmentCreate"]["attachment"], "id")?;
            // A failed attachment can leave an orphan asset. A lost attachment
            // response or failed receipt commit can also duplicate it on retry.
            self.store.complete_delivery(
                claim.clone(),
                json!({"assetUrl":asset_url,"attachmentId":attachment_id}).to_string(),
            ).map_err(|error| UploadError::new("artifact_store_error", error.to_string()))?;
            Ok(ToolOutcome::ok(json!({
                "assetUrl":asset_url,
                "attachmentId":attachment_id,
                "issue":identifier,
                "filename":metadata.filename,
                "bytes":metadata.bytes,
                "deduplicated":false
            }).to_string()))
        })
        .await
        .unwrap_or_else(|_| Err(UploadError::new("transport_error", "deadline exceeded")));
        if result.is_err() {
            if let Err(error) = self.store.release_delivery(claim) {
                tracing::warn!(%error, "failed releasing Linear artifact delivery claim");
            }
        }
        result
    }
}

fn required_string<'a>(value: &'a Value, key: &str) -> UploadResult<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            UploadError::new("invalid_response", format!("Linear response missing {key}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dar_artifacts::{ArtifactMetadataInput, ExportRoot};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const ISSUE: &str = "368553da-e6cc-41af-8a71-1ccc5ce9753a";
    const ASSET: &str = "https://uploads.linear.app/asset/proof.txt";
    const SECRET: &str = "signed-upload-secret";

    struct Request {
        headers: String,
        body: Vec<u8>,
    }

    type Override = Arc<dyn Fn(usize, &str, &[u8]) -> Option<String> + Send + Sync>;

    async fn server() -> (String, Arc<Mutex<Vec<Request>>>) {
        server_with(Arc::new(|_, _, _| None)).await
    }

    async fn server_with(override_reply: Override) -> (String, Arc<Mutex<Vec<Request>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let upload = format!("{endpoint}/upload?signature={SECRET}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|s| s.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let body = bytes[header_end..].to_vec();
                let response = if headers.starts_with("PUT ") {
                    json!({})
                } else {
                    let payload: Value = serde_json::from_slice(&body).unwrap();
                    let query = payload["query"].as_str().unwrap();
                    if query.contains("fileUpload") {
                        json!({"data":{"fileUpload":{"success":true,"uploadFile":{"uploadUrl":upload,"assetUrl":ASSET,"headers":[{"key":"x-upload-proof","value":"proof-value"}]}}}})
                    } else if query.contains("attachmentCreate") {
                        json!({"data":{"attachmentCreate":{"success":true,"attachment":{"id":"attachment-1"}}}})
                    } else if payload["variables"]["id"] == "ALG-512" {
                        json!({"data":{"issue":{"id":"acb78045-568f-45df-802a-b5206238aaca","identifier":"ALG-512"}}})
                    } else {
                        json!({"data":{"issue":{"id":ISSUE,"identifier":"ALG-511"}},"errors":[]})
                    }
                };
                let replacement = override_reply(captured.lock().unwrap().len(), &headers, &body);
                captured.lock().unwrap().push(Request { headers, body });
                let body = response.to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let reply = match replacement {
                    Some(delay) if delay.starts_with("DELAY:") => {
                        tokio::time::sleep(Duration::from_millis(delay[6..].parse().unwrap()))
                            .await;
                        reply
                    }
                    Some(raw) => raw,
                    None => reply,
                };
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (endpoint, requests)
    }

    fn artifact() -> (tempfile::TempDir, Arc<ArtifactStore>, ArtifactId) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("exports")).unwrap();
        std::fs::write(
            dir.path().join("exports/proof.txt"),
            b"verified proof\0\xff",
        )
        .unwrap();
        let store =
            Arc::new(ArtifactStore::open(dir.path().join("vault"), 25 * 1024 * 1024).unwrap());
        let metadata = store
            .stage_from_export_root(
                &ExportRoot::open(dir.path().join("exports")).unwrap(),
                "proof.txt",
                ArtifactMetadataInput {
                    filename: "proof.txt".into(),
                    media_type: Some("text/plain".into()),
                    caption: Some("Validation proof".into()),
                },
            )
            .unwrap();
        (dir, store, metadata.id)
    }

    #[tokio::test]
    async fn store_failures_and_active_claims_return_structured_errors_without_uploading() {
        let (dir, store, id) = artifact();
        let (endpoint, requests) = server().await;
        let tool = LinearUploadArtifactTool::new(
            endpoint,
            AuthSource::Static("test-key".into()),
            store.clone(),
        )
        .unwrap();
        let args = json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"});
        let unknown = tool.execute(json!({"artifact":format!("dar-artifact://{}",ArtifactId::new()),"issue":"ALG-511"})).await.unwrap();
        assert_eq!(unknown.error.unwrap().code, "artifact_not_found");
        let claim = match store
            .claim_delivery(
                id,
                ArtifactDeliveryTarget {
                    surface_id: "linear".into(),
                    origin_destination: ISSUE.into(),
                },
            )
            .unwrap()
        {
            DeliveryClaimResult::Claimed(claim) => claim,
            _ => panic!("expected new claim"),
        };
        assert_eq!(
            tool.execute(args.clone())
                .await
                .unwrap()
                .error
                .unwrap()
                .code,
            "upload_in_progress"
        );
        store.release_delivery(claim).unwrap();
        std::fs::write(
            dir.path().join("vault").join(id.to_string()).join("data"),
            b"modified proof\0\xff",
        )
        .unwrap();
        assert_eq!(
            tool.execute(args.clone())
                .await
                .unwrap()
                .error
                .unwrap()
                .code,
            "artifact_invalid"
        );
        assert_eq!(requests.lock().unwrap().len(), 3);
        std::fs::write(
            dir.path().join("vault").join(id.to_string()).join("data"),
            b"verified proof\0\xff",
        )
        .unwrap();
        let out = tool.execute(args).await.unwrap();
        assert!(!out.is_error, "{}", out.text);

        let (dir, store, id) = artifact();
        std::fs::create_dir(
            dir.path()
                .join("vault")
                .join(id.to_string())
                .join("delivery.lock"),
        )
        .unwrap();
        let (endpoint, requests) = server().await;
        let tool =
            LinearUploadArtifactTool::new(endpoint, AuthSource::Static("test-key".into()), store)
                .unwrap();
        let out = tool
            .execute(json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"}))
            .await
            .unwrap();
        assert_eq!(out.error.unwrap().code, "artifact_store_error");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn expired_claim_commit_errors_and_release_errors_preserve_original_outcomes() {
        for (step, expected) in [(3, "artifact_store_error"), (2, "upload_failed")] {
            let (dir, store, id) = artifact();
            let delivery_path = dir
                .path()
                .join("vault")
                .join(id.to_string())
                .join("deliveries.json");
            let (endpoint, _) = server_with(Arc::new(move |index, _, _| {
                if index != step {
                    return None;
                }
                let mut claims: Value =
                    serde_json::from_slice(&std::fs::read(&delivery_path).unwrap()).unwrap();
                claims[0]["Claimed"]["claimed_at_unix_ms"] = json!(0);
                std::fs::write(&delivery_path, claims.to_string()).unwrap();
                if step == 2 {
                    Some(
                        "HTTP/1.1 500 Failure\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into(),
                    )
                } else {
                    None
                }
            }))
            .await;
            let tool = LinearUploadArtifactTool::new(
                endpoint,
                AuthSource::Static("test-key".into()),
                store,
            )
            .unwrap();
            let args = json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"});
            let out = tool.execute(args.clone()).await.unwrap();
            assert_eq!(out.error.unwrap().code, expected);
            assert!(!out.text.contains(SECRET));
            let retry = tool.execute(args).await.unwrap();
            assert!(!retry.is_error, "{}", retry.text);
        }
    }

    #[tokio::test]
    async fn put_transport_and_echoed_signed_url_errors_remain_private_and_retryable() {
        for transport in [false, true] {
            let (_dir, store, id) = artifact();
            let (endpoint, _) = server_with(Arc::new(move |index, _, _| {
                let body = if transport && index == 1 {
                    Some(json!({"data":{"fileUpload":{"success":true,"uploadFile":{"uploadUrl":format!("http://127.0.0.1:1/upload?signature={SECRET}"),"assetUrl":ASSET,"headers":[]}}}}).to_string())
                } else if !transport && index == 3 {
                    Some(json!({"errors":[{"message":format!("denied signed-upload-secret /upload?signature={SECRET}") }]}).to_string())
                } else { None };
                body.map(|body| format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()))
            })).await;
            let tool = LinearUploadArtifactTool::new(
                endpoint,
                AuthSource::Static("test-key".into()),
                store,
            )
            .unwrap();
            let args = json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"});
            let out = tool.execute(args.clone()).await.unwrap();
            assert_eq!(
                out.error.unwrap().code,
                if transport {
                    "upload_failed"
                } else {
                    "graphql_error"
                }
            );
            assert!(!out.text.contains(SECRET));
            assert!(!out.text.contains("/upload"));
            assert!(!tool.execute(args).await.unwrap().is_error);
        }
    }

    #[tokio::test]
    async fn larger_store_policy_cannot_bypass_the_25_mib_upload_limit() {
        let (dir, _store, _) = artifact();
        let store =
            Arc::new(ArtifactStore::open(dir.path().join("vault"), 30 * 1024 * 1024).unwrap());
        let file = std::fs::File::create(dir.path().join("exports/large.bin")).unwrap();
        file.set_len(25 * 1024 * 1024 + 1).unwrap();
        let metadata = store
            .stage_from_export_root(
                &ExportRoot::open(dir.path().join("exports")).unwrap(),
                "large.bin",
                ArtifactMetadataInput {
                    filename: "large.bin".into(),
                    media_type: None,
                    caption: None,
                },
            )
            .unwrap();
        let (endpoint, requests) = server().await;
        let tool =
            LinearUploadArtifactTool::new(endpoint, AuthSource::Static("test-key".into()), store)
                .unwrap();
        let out = tool
            .execute(
                json!({"artifact":format!("dar-artifact://{}", metadata.id),"issue":"ALG-511"}),
            )
            .await
            .unwrap();
        assert_eq!(
            out.error.as_ref().map(|e| e.code.as_str()),
            Some("artifact_invalid"),
            "{}",
            out.text
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_deadline_bounds_hangs_and_cumulative_upload_work_then_allows_retry() {
        for cumulative in [false, true] {
            let (_dir, store, id) = artifact();
            let (endpoint, _) = server_with(Arc::new(move |index, _, _| {
                if index == 1 {
                    Some(if cumulative { "DELAY:35" } else { "DELAY:150" }.into())
                } else if cumulative && index == 2 {
                    Some("DELAY:35".into())
                } else {
                    None
                }
            }))
            .await;
            let mut tool = LinearUploadArtifactTool::new(
                endpoint,
                AuthSource::Static("test-key".into()),
                store,
            )
            .unwrap();
            tool.deadline = Duration::from_millis(60);
            let args = json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"});
            let out = tool.execute(args.clone()).await.unwrap();
            assert_eq!(
                out.error.as_ref().map(|e| e.code.as_str()),
                Some("transport_error"),
                "{}",
                out.text
            );
            assert!(out.text.contains("deadline exceeded"));
            tool.deadline = Duration::from_secs(1);
            let retry = tool.execute(args).await.unwrap();
            assert!(!retry.is_error, "{}", retry.text);
        }
    }

    #[tokio::test]
    async fn failed_upload_steps_return_one_error_and_release_claim_for_retry() {
        for (step, body, expected) in [
            (
                1,
                r#"{"data":{"fileUpload":{"success":false}}}"#,
                "invalid_response",
            ),
            (
                1,
                r#"{"data":{"fileUpload":{"success":true,"uploadFile":null}}}"#,
                "invalid_response",
            ),
            (
                1,
                r#"{"data":{"fileUpload":{"success":true,"uploadFile":{"assetUrl":"asset"}}}}"#,
                "invalid_response",
            ),
            (2, "failed", "upload_failed"),
            (
                3,
                r#"{"data":{"attachmentCreate":{"success":false}}}"#,
                "invalid_response",
            ),
            (
                3,
                r#"{"data":{"attachmentCreate":{"success":true,"attachment":{}}}}"#,
                "invalid_response",
            ),
            (3, r#"{"errors":[{"message":"denied"}]}"#, "graphql_error"),
        ] {
            let (_dir, store, id) = artifact();
            let (endpoint, _) = server_with(Arc::new(move |index, _, _| {
                if index == step {
                    Some(format!(
                        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        if step == 2 { "500 Failure" } else { "200 OK" },
                        body.len()
                    ))
                } else {
                    None
                }
            }))
            .await;
            let tool = LinearUploadArtifactTool::new(
                endpoint,
                AuthSource::Static("test-key".into()),
                store,
            )
            .unwrap();
            let args = json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"});
            let out = tool.execute(args.clone()).await.unwrap();
            assert_eq!(
                out.error.as_ref().map(|e| e.code.as_str()),
                Some(expected),
                "{}",
                out.text
            );
            assert!(!out.text.contains(SECRET));
            assert!(!out.text.contains("/upload"));
            let retry = tool.execute(args).await.unwrap();
            assert!(!retry.is_error, "{}", retry.text);
        }
    }

    #[tokio::test]
    async fn invalid_arguments_auth_and_graphql_failures_return_structured_errors() {
        let (_dir, store, id) = artifact();
        let args = json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"});
        for bad in [
            json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511","title":null}),
            json!({}),
            json!({"artifact":id.to_string(),"issue":"ALG-511"}),
            json!({"artifact":"dar-artifact://bad","issue":"ALG-511"}),
            json!({"artifact":format!("dar-artifact://{id}"),"issue":""}),
            json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511","other":true}),
        ] {
            let tool = LinearUploadArtifactTool::new(
                "http://127.0.0.1:1".into(),
                AuthSource::Static("test-key".into()),
                store.clone(),
            )
            .unwrap();
            let out = tool.execute(bad).await.unwrap();
            assert_eq!(out.error.unwrap().code, "invalid_args");
        }
        let tool = LinearUploadArtifactTool::new(
            "http://127.0.0.1:1".into(),
            AuthSource::Static(String::new()),
            store.clone(),
        )
        .unwrap();
        assert_eq!(
            tool.execute(args.clone())
                .await
                .unwrap()
                .error
                .unwrap()
                .code,
            "missing_auth"
        );
        let tool = LinearUploadArtifactTool::new(
            "http://127.0.0.1:1".into(),
            AuthSource::Static("test-key".into()),
            store.clone(),
        )
        .unwrap();
        assert_eq!(
            tool.execute(args.clone())
                .await
                .unwrap()
                .error
                .unwrap()
                .code,
            "transport_error"
        );
        for (status, body, expected) in [
            ("200 OK", r#"{"data":{"issue":null}}"#, "issue_not_found"),
            ("200 OK", r#"{"data":{}}"#, "issue_not_found"),
            (
                "200 OK",
                r#"{"errors":[{"message":"denied test-key"}]}"#,
                "graphql_error",
            ),
            ("403 Forbidden", "denied test-key", "http_error"),
            ("200 OK", "no json", "non_json_response"),
        ] {
            let (endpoint, _) = server_with(Arc::new(move |_, _, _| {
                Some(format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ))
            }))
            .await;
            let tool = LinearUploadArtifactTool::new(
                endpoint,
                AuthSource::Static("test-key".into()),
                store.clone(),
            )
            .unwrap();
            let out = tool.execute(args.clone()).await.unwrap();
            assert_eq!(out.error.unwrap().code, expected);
            assert!(!out.text.contains("test-key"));
        }
        let (endpoint, _) = server_with(Arc::new(|_, _, _| {
            Some("HTTP/1.1 200 OK\r\nContent-Length: 9999\r\nConnection: close\r\n\r\nshort".into())
        }))
        .await;
        let tool =
            LinearUploadArtifactTool::new(endpoint, AuthSource::Static("test-key".into()), store)
                .unwrap();
        assert_eq!(
            tool.execute(args).await.unwrap().error.unwrap().code,
            "response_read_error"
        );
    }

    #[tokio::test]
    async fn repeat_uploads_deduplicate_identifiers_and_uuids_but_allow_other_issues() {
        let (_dir, store, id) = artifact();
        let (endpoint, requests) = server().await;
        let tool =
            LinearUploadArtifactTool::new(endpoint, AuthSource::Static("test-key".into()), store)
                .unwrap();
        let args = json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"});
        assert!(!tool.execute(args.clone()).await.unwrap().is_error);
        let mut uuid_args = args.clone();
        uuid_args["issue"] = json!(ISSUE);
        for retry_args in [args, uuid_args] {
            let result = tool.execute(retry_args).await.unwrap();
            assert!(!result.is_error, "{}", result.text);
            let output: Value = serde_json::from_str(&result.text).unwrap();
            assert_eq!(output["deduplicated"], true);
            assert_eq!(output["assetUrl"], ASSET);
            assert_eq!(output["attachmentId"], "attachment-1");
        }
        assert_eq!(requests.lock().unwrap().len(), 6);
        let other = tool.execute(json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-512","title":"Shared proof"})).await.unwrap();
        assert!(!other.is_error, "{}", other.text);
        let output: Value = serde_json::from_str(&other.text).unwrap();
        assert_eq!(output["issue"], "ALG-512");
        assert_eq!(output["deduplicated"], false);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 10);
        let attachment: Value = serde_json::from_slice(&requests[9].body).unwrap();
        assert_eq!(
            attachment["variables"]["input"]["issueId"],
            "acb78045-568f-45df-802a-b5206238aaca"
        );
        assert_eq!(attachment["variables"]["input"]["title"], "Shared proof");
    }

    #[tokio::test]
    async fn uploads_verified_bytes_and_attaches_to_issue_without_exposing_upload_url() {
        let (_dir, store, id) = artifact();
        let (endpoint, requests) = server().await;
        let tool =
            LinearUploadArtifactTool::new(endpoint, AuthSource::Static("test-key".into()), store)
                .unwrap();
        let result = tool
            .execute(json!({"artifact":format!("dar-artifact://{id}"),"issue":"ALG-511"}))
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.text);
        let output: Value = serde_json::from_str(&result.text).unwrap();
        assert_eq!(
            output,
            json!({"assetUrl":ASSET,"attachmentId":"attachment-1","issue":"ALG-511","filename":"proof.txt","bytes":16,"deduplicated":false})
        );
        assert!(!result.text.contains(SECRET));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let put = &requests[2];
        assert_eq!(put.body, b"verified proof\0\xff");
        let headers = put.headers.to_ascii_lowercase();
        assert!(headers.contains("x-upload-proof: proof-value"));
        assert!(headers.contains("content-type: text/plain"));
        assert!(headers.contains("cache-control: public, max-age=31536000"));
        let attachment: Value = serde_json::from_slice(&requests[3].body).unwrap();
        assert_eq!(
            attachment["variables"]["input"],
            json!({"issueId":ISSUE,"url":ASSET,"title":"proof.txt","subtitle":"Validation proof"})
        );
    }
}
