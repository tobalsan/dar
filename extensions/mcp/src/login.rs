use std::{path::Path, time::Duration};

use anyhow::{bail, Context, Result};
use rmcp::transport::{AuthorizationManager, CredentialStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{credentials::FileCredentialStore, load, Server};

/// Run interactive OAuth authorization for one configured HTTP server.
pub async fn login(root: &Path, name: &str) -> Result<()> {
    login_with(root, name, Duration::from_secs(900), false).await
}

/// Auto-login at `dar run` boot: shorter wait, and give up at once if no
/// browser could be opened, so an unattended TTY never stalls boot for long.
pub async fn auto_login(root: &Path, name: &str) -> Result<()> {
    login_with(root, name, Duration::from_secs(180), true).await
}

async fn login_with(root: &Path, name: &str, wait: Duration, need_browser: bool) -> Result<()> {
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
    let (mut manager, store, auth_url) = begin_authorization(root, name, url, &redirect).await?;
    println!("Open this URL to authorize {name}:\n{auth_url}");
    if !open_browser(&auth_url) && need_browser {
        bail!("could not open a browser; run `dar mcp login {name}` or use the dashboard");
    }
    let minutes = wait.as_secs() / 60;

    tokio::time::timeout(wait, async {
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
            match exchange_and_save(&mut manager, &store, code, state, issuer).await {
                Ok(()) => {
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
                    break Err(error);
                }
            }
        }
    })
    .await
    .with_context(|| format!("OAuth callback timed out after {minutes} minutes"))??;
    println!("MCP login succeeded for {name}");
    Ok(())
}

pub(crate) async fn exchange_and_save(
    manager: &mut AuthorizationManager,
    store: &FileCredentialStore,
    code: &str,
    state: &str,
    issuer: Option<&str>,
) -> Result<()> {
    let _guard = store
        .acquire_refresh_guard()
        .await?
        .context("credential lock unavailable")?;
    manager
        .exchange_code_for_token_with_issuer(code, state, issuer)
        .await?;
    store
        .load()
        .await?
        .context("OAuth server returned no stored credentials")?;
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

/// OAuth discovery + dynamic client registration; returns the manager (holding
/// the PKCE verifier) and the provider authorization URL. Shared by the CLI and
/// dashboard flows.
pub(crate) async fn begin_authorization(
    root: &Path,
    name: &str,
    url: &str,
    redirect: &str,
) -> Result<(AuthorizationManager, FileCredentialStore, String)> {
    let store = FileCredentialStore::new(root, name)?;
    let mut manager = AuthorizationManager::new(url).await?;
    manager.set_credential_store(store.clone());
    let metadata = manager.resolve_metadata().await?;
    manager.set_metadata(metadata.metadata);
    manager.register_client("dar", redirect, &[]).await?;
    let auth_url = manager.get_authorization_url(&[]).await?;
    Ok((manager, store, auth_url))
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(not(target_os = "macos"))]
    let command = "xdg-open";
    std::process::Command::new(command)
        .arg(url)
        .spawn()
        .and_then(|mut child| child.wait())
        .is_ok_and(|status| status.success())
}
