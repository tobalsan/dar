use std::{path::Path, time::Duration};

use anyhow::{bail, Context, Result};
use rmcp::transport::{AuthorizationManager, CredentialStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{credentials::FileCredentialStore, load, Server};

/// Run interactive OAuth authorization for one configured HTTP server.
pub async fn login(root: &Path, name: &str) -> Result<()> {
    let config = load(root)?;
    let server = config
        .mcp_servers
        .get(name)
        .with_context(|| format!("MCP server {name:?} is not configured"))?;
    let Server::Http { url, headers } = server else {
        bail!("MCP server {name:?} uses stdio and does not need OAuth")
    };
    if headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("authorization"))
    {
        bail!("MCP server {name:?} has a static Authorization header")
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://{}/callback", listener.local_addr()?);
    let store = FileCredentialStore::new(root, name)?;
    let mut manager = AuthorizationManager::new(url).await?;
    manager.set_credential_store(store.clone());
    let metadata = manager.resolve_metadata().await?;
    manager.set_metadata(metadata.metadata);
    manager.register_client("dar", &redirect, &[]).await?;
    let auth_url = manager.get_authorization_url(&[]).await?;
    println!("Open this URL to authorize {name}:\n{auth_url}");
    open_browser(&auth_url);

    tokio::time::timeout(Duration::from_secs(900), async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            // A stalled connection must not block the real callback.
            let request = match tokio::time::timeout(
                Duration::from_secs(10),
                read_headers(&mut stream),
            )
            .await
            {
                Ok(Ok(request)) => request,
                Ok(Err(error)) => {
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        &format!("Invalid callback: {error}"),
                    )
                    .await?;
                    continue;
                }
                Err(_) => continue,
            };
            let mut parts = request
                .lines()
                .next()
                .unwrap_or_default()
                .split_whitespace();
            let method = parts.next().unwrap_or_default();
            let target = parts.next().unwrap_or_default();
            if method != "GET" || !target.starts_with("/callback?") {
                respond(&mut stream, "404 Not Found", "Not found").await?;
                continue;
            }
            let callback = format!("{}{}", redirect.trim_end_matches("/callback"), target);
            let parsed = url::Url::parse(&callback)?;
            let params = parsed
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            let code = params.get("code").context("OAuth callback missing code")?;
            let state = params
                .get("state")
                .context("OAuth callback missing state")?;
            let issuer = params.get("iss").map(|value| value.as_ref());
            let _guard = store
                .acquire_refresh_guard()
                .await?
                .context("credential lock unavailable")?;
            match manager
                .exchange_code_for_token_with_issuer(code, state, issuer)
                .await
            {
                Ok(_) => {
                    store
                        .load()
                        .await?
                        .context("OAuth server returned no stored credentials")?;
                    respond(
                        &mut stream,
                        "200 OK",
                        "Authorization complete. You may close this tab.",
                    )
                    .await?;
                    break Result::<()>::Ok(());
                }
                Err(error) => {
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "Authorization failed. Return to terminal.",
                    )
                    .await?;
                    break Err(error.into());
                }
            }
        }
    })
    .await
    .context("OAuth callback timed out after 15 minutes")??;
    println!("MCP login succeeded for {name}");
    Ok(())
}

async fn read_headers(stream: &mut tokio::net::TcpStream) -> Result<String> {
    let mut bytes = Vec::with_capacity(1024);
    while bytes.len() < 8192 {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes).context("callback headers are not UTF-8");
        }
    }
    bail!("callback headers incomplete or exceed 8 KiB")
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) -> Result<()> {
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(not(target_os = "macos"))]
    let command = "xdg-open";
    let _ = std::process::Command::new(command).arg(url).spawn();
}
