use std::env;
use std::error::Error;

use flowerchat_protocol::IceServer;
use flowerchat_server::{Config, ReleasesConfig, app};
use tracing::info;

/// Address to listen on. Defaults to loopback; set to `0.0.0.0:8080` in a
/// container or to reach the server from other machines.
const LISTEN_VAR: &str = "FLOWERCHAT_LISTEN";
/// JSON array of ICE servers in `RTCIceServer` form, for example
/// `[{"urls":["turn:example.org:3478"],"username":"u","credential":"p"}]`.
const ICE_SERVERS_VAR: &str = "FLOWERCHAT_ICE_SERVERS";
/// The GitHub repository, as `owner/name`, whose latest release the page
/// offers to download. Set it empty to offer nothing.
const RELEASES_VAR: &str = "FLOWERCHAT_RELEASES";
/// A token that may read that repository's contents. Only needed if the
/// repository is private.
const TOKEN_VAR: &str = "FLOWERCHAT_GITHUB_TOKEN";

const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302";
const DEFAULT_RELEASES: &str = "Toribash67/flowerchat";
const GITHUB_API: &str = "https://api.github.com";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let ice_servers = match env::var(ICE_SERVERS_VAR) {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|err| format!("{ICE_SERVERS_VAR} is not a valid ICE server list: {err}"))?,
        Err(_) => vec![IceServer {
            urls: vec![DEFAULT_STUN.to_owned()],
            username: None,
            credential: None,
        }],
    };
    let repo = env::var(RELEASES_VAR).unwrap_or_else(|_| DEFAULT_RELEASES.to_owned());
    let releases = if repo.trim().is_empty() {
        None
    } else {
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
            return Err(format!("{RELEASES_VAR} must be written owner/name, not {repo:?}").into());
        }
        Some(ReleasesConfig {
            repo,
            token: env::var(TOKEN_VAR)
                .ok()
                .filter(|token| !token.trim().is_empty()),
            api: GITHUB_API.to_owned(),
        })
    };
    let listen = env::var(LISTEN_VAR).unwrap_or_else(|_| DEFAULT_LISTEN.to_owned());

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|err| format!("cannot listen on {listen}: {err}"))?;
    info!("listening on http://{}", listener.local_addr()?);

    axum::serve(
        listener,
        app(Config {
            ice_servers,
            releases,
        }),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
