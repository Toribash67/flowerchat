use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};

use flowerchat_protocol::IceServer;
use flowerchat_server::{Config, ReleasesConfig, app, invites};
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
/// The file of invites. With it set, only invited people may join a room,
/// and `flowerchat-server invite <name>` invites one. Without it, anyone
/// who reaches the server may join.
const INVITES_VAR: &str = "FLOWERCHAT_INVITES";

/// The rooms that are always there to join, separated by commas, in the
/// order clients list them. Any other room exists while someone is in it.
const ROOMS_VAR: &str = "FLOWERCHAT_ROOMS";

const DEFAULT_ROOMS: &str = "lounge";
const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302";
const DEFAULT_RELEASES: &str = "Toribash67/flowerchat";
const GITHUB_API: &str = "https://api.github.com";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let invites = env::var(INVITES_VAR)
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    let args: Vec<String> = env::args().skip(1).collect();
    if !args.is_empty() {
        // Printed here, as the plain text it is, rather than returned: an
        // error returned from `main` is printed in quotes, on one line.
        if let Err(err) = manage_invites(invites.as_deref(), &args) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        return Ok(());
    }

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
    match &invites {
        Some(path) => info!("only letting in those invited in {}", path.display()),
        None => info!("letting in anyone: {INVITES_VAR} is not set"),
    }

    axum::serve(
        listener,
        app(Config {
            ice_servers,
            releases,
            invites,
            rooms: env::var(ROOMS_VAR)
                .unwrap_or_else(|_| DEFAULT_ROOMS.to_owned())
                .split(',')
                .map(|room| room.trim().to_owned())
                .filter(|room| !room.is_empty())
                .collect(),
        }),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

const USAGE: &str = "flowerchat-server                           run the server
flowerchat-server invite <name> [room...]   invite someone, to those rooms or to all
flowerchat-server revoke <name>             take an invite back
flowerchat-server invites                   list who is invited";

/// Runs one of the commands that change or show who is invited, on the same
/// file as the running server reads. The server need not be restarted.
fn manage_invites(path: Option<&Path>, args: &[String]) -> Result<(), Box<dyn Error>> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if matches!(args[..], ["help" | "--help" | "-h"]) {
        println!("{USAGE}");
        return Ok(());
    }
    let path = path.ok_or_else(|| {
        format!("set {INVITES_VAR} to the file of invites, for this command and for the server")
    })?;
    let failed = |err| format!("{}: {err}", path.display());
    match args[..] {
        ["invite", name, ref rooms @ ..] if !name.trim().is_empty() => {
            let rooms: Vec<String> = rooms.iter().map(|room| (*room).to_owned()).collect();
            let token = invites::invite(path, name, &rooms).map_err(failed)?;
            match rooms.is_empty() {
                true => println!("Invited {name} to every room. Their token, shown only now:"),
                false => println!(
                    "Invited {name} to {}. Their token, shown only now:",
                    rooms.join(", ")
                ),
            }
            println!(
                "
    {token}
"
            );
            println!("In the app it goes in the Invite field. For the browser, send them");
            println!("the address of the page with ?invite={token} at the end.");
        }
        ["revoke", name] => match invites::revoke(path, name).map_err(failed)? {
            true => println!(
                "{name} is no longer invited. If they are in a room, they stay until they leave it."
            ),
            false => return Err(format!("nobody is invited as {name}").into()),
        },
        ["invites"] => {
            let invited = invites::list(path).map_err(failed)?;
            if invited.is_empty() {
                println!("Nobody is invited.");
            }
            for invite in invited {
                match invite.rooms.is_empty() {
                    true => println!("{}", invite.name),
                    false => println!("{}  (only {})", invite.name, invite.rooms.join(", ")),
                }
            }
        }
        _ => {
            return Err(format!(
                "not a command of this server

{USAGE}"
            )
            .into());
        }
    }
    Ok(())
}
