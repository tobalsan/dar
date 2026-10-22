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
    // Bound the whole flow (discovery, registration, browser, callback).
    let limit = Duration::from_secs(180);
    tokio::time::timeout(limit, login_with(root, name, limit, true))
        .await
        .context("MCP login timed out after 3 minutes")?
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
    let expected_state = url::Url::parse(&auth_url)?
        .query_pairs()
        .find(|(key, _)| key == "state")
        .context("authorization URL missing state")?
        .1
        .into_owned();
    println!("Open this URL to authorize {name}:\n{auth_url}");
    if !open_browser(&auth_url).await && need_browser {
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
            let state = params
                .get("state")
                .context("OAuth callback missing state")?;
            if state.as_ref() != expected_state {
                respond(&mut stream, "400 Bad Request", "Unknown OAuth state").await?;
                continue;
            }
            if let Some(error) = params.get("error") {
                let reason = authorization_rejected(root, name, error)?;
                respond(&mut stream, "400 Bad Request", reason).await?;
                break Err(anyhow::anyhow!(reason));
            }
            let code = params.get("code").context("OAuth callback missing code")?;
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
    if let Err(error) = manager.register_client("dar", redirect, &[]).await {
        if matches!(&error, rmcp::transport::AuthError::RegistrationFailed(reason)
            if ["HTTP 400 ", "HTTP 401 ", "HTTP 403 "].iter().any(|status| reason.starts_with(status)))
        {
            remember_rejection(root, name, "registration")?;
            bail!(REGISTRATION_REJECTED);
        }
        return Err(error.into());
    }
    let rejection = root.join("data/mcp-auth").join(format!("{name}.rejection"));
    match std::fs::remove_file(rejection) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let auth_url = manager.get_authorization_url(&[]).await?;
    Ok((manager, store, auth_url))
}

async fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(not(target_os = "macos"))]
    let command = "xdg-open";
    let Ok(mut child) = tokio::process::Command::new(command)
        .arg(url)
        .kill_on_drop(true)
        .spawn()
    else {
        return false;
    };
    // A hung opener counts as failure (and is killed on drop).
    matches!(
        tokio::time::timeout(Duration::from_secs(10), child.wait()).await,
        Ok(Ok(status)) if status.success()
    )
}

const REGISTRATION_REJECTED: &str = "Vendor rejected Dar OAuth client registration; check the vendor OAuth policy and allowed redirects; MCP App view hosting has not started";
const AUTHORIZATION_REJECTED: &str = "Vendor rejected Dar OAuth authorization; check the vendor OAuth policy and account access; MCP App view hosting has not started";

pub(crate) fn oauth_rejection(root: &Path, name: &str) -> Option<&'static str> {
    let path = root.join("data/mcp-auth").join(format!("{name}.rejection"));
    match std::fs::read(path).ok()?.as_slice() {
        b"registration" => Some(REGISTRATION_REJECTED),
        b"authorization" => Some(AUTHORIZATION_REJECTED),
        _ => None,
    }
}

pub(crate) fn authorization_rejected(root: &Path, name: &str, error: &str) -> Result<&'static str> {
    if matches!(
        error,
        "unauthorized_client" | "invalid_client" | "access_denied" | "invalid_redirect_uri"
    ) {
        remember_rejection(root, name, "authorization")?;
        Ok(AUTHORIZATION_REJECTED)
    } else {
        Ok("OAuth authorization failed; retry the vendor login")
    }
}

fn remember_rejection(root: &Path, name: &str, kind: &str) -> Result<()> {
    use std::io::Write;
    let directory = root.join("data/mcp-auth");
    // FileCredentialStore::new has already checked/created this directory.
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(kind.as_bytes())?;
    file.persist(directory.join(format!("{name}.rejection")))?;
    Ok(())
}
