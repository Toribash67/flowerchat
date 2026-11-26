//! The page's downloads, against a stand-in for GitHub's API.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use flowerchat_server::{Config, ReleasesConfig, app};
use serde_json::{Value, json};

const TOKEN: &str = "secret-token";
const REPO: &str = "someone/flowerchat";

/// What the stand-in GitHub knows and has been asked.
#[derive(Default)]
struct Github {
    /// The latest release, or none yet.
    release: Option<Value>,
    /// Whether only someone with the token may see the repository.
    private: bool,
    lookups: AtomicUsize,
}

impl Github {
    fn allows(&self, headers: &HeaderMap) -> bool {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        !self.private || token == Some(&format!("Bearer {TOKEN}"))
    }
}

async fn start_github(github: Github) -> (String, Arc<Github>) {
    let github = Arc::new(github);
    let router = Router::new()
        .route(
            "/repos/someone/flowerchat/releases/latest",
            get(
                |State(github): State<Arc<Github>>, headers: HeaderMap| async move {
                    github.lookups.fetch_add(1, Ordering::SeqCst);
                    match &github.release {
                        Some(release) if github.allows(&headers) => {
                            axum::Json(release.clone()).into_response()
                        }
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
        .route(
            "/repos/someone/flowerchat/releases/assets/{id}",
            get(
                |State(github): State<Arc<Github>>,
                 Path(id): Path<u64>,
                 headers: HeaderMap| async move {
                    let wants_file = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
                        == Some("application/octet-stream");
                    if !github.allows(&headers) || !wants_file {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    (
                        StatusCode::FOUND,
                        [(
                            header::LOCATION,
                            format!("https://objects.example.net/{id}?signature=abc"),
                        )],
                    )
                        .into_response()
                },
            ),
        )
        .with_state(github.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), github)
}

async fn start_server(releases: Option<ReleasesConfig>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        ice_servers: vec![],
        releases,
        invites: None,
        rooms: vec![],
    };
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });
    format!("http://{addr}")
}

fn release() -> Value {
    json!({
        "tag_name": "v0.2.0",
        "name": "",
        "published_at": "2026-11-20T18:00:00Z",
        "assets": [
            {
                "id": 11,
                "name": "flowerchat-0.2.0-setup.exe",
                "size": 123_456_789,
                "browser_download_url": "https://github.com/someone/flowerchat/releases/download/v0.2.0/flowerchat-0.2.0-setup.exe",
            },
            {
                "id": 12,
                "name": "SHA256SUMS",
                "size": 100,
                "browser_download_url": "https://github.com/someone/flowerchat/releases/download/v0.2.0/SHA256SUMS",
            },
        ],
    })
}

/// A client that reports redirects instead of following them.
fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn get_json(url: &str) -> (StatusCode, Value) {
    let response = client().get(url).send().await.unwrap();
    (response.status(), response.json().await.unwrap())
}

async fn redirect_of(url: &str) -> Option<String> {
    let response = client().get(url).send().await.unwrap();
    if !response.status().is_redirection() {
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{url}");
        return None;
    }
    Some(
        response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned(),
    )
}

async fn server_for(github: &str, token: Option<&str>) -> String {
    start_server(Some(ReleasesConfig {
        repo: REPO.into(),
        token: token.map(Into::into),
        api: github.into(),
    }))
    .await
}

#[tokio::test]
async fn the_page_lists_the_installers_of_the_latest_release() {
    let (github, _) = start_github(Github {
        release: Some(release()),
        ..Default::default()
    })
    .await;
    let server = server_for(&github, None).await;

    let (status, body) = get_json(&format!("{server}/api/release")).await;
    assert_eq!(status, StatusCode::OK);
    // A release without a name goes by its tag, and the checksums are not
    // an installer.
    assert_eq!(
        body,
        json!({
            "release": {
                "tag": "v0.2.0",
                "name": "v0.2.0",
                "published": "2026-11-20T18:00:00Z",
                "assets": [{
                    "name": "flowerchat-0.2.0-setup.exe",
                    "platform": "windows",
                    "size": 123_456_789,
                    "download": "/download/flowerchat-0.2.0-setup.exe",
                }],
            },
        })
    );
}

#[tokio::test]
async fn a_public_repository_is_downloaded_from_directly() {
    let (github, _) = start_github(Github {
        release: Some(release()),
        ..Default::default()
    })
    .await;
    let server = server_for(&github, None).await;

    let direct =
        "https://github.com/someone/flowerchat/releases/download/v0.2.0/flowerchat-0.2.0-setup.exe";
    let by_name = redirect_of(&format!("{server}/download/flowerchat-0.2.0-setup.exe")).await;
    assert_eq!(by_name.as_deref(), Some(direct));
    let by_platform = redirect_of(&format!("{server}/download/windows")).await;
    assert_eq!(by_platform.as_deref(), Some(direct));

    assert_eq!(redirect_of(&format!("{server}/download/linux")).await, None);
    assert_eq!(
        redirect_of(&format!("{server}/download/SHA256SUMS")).await,
        None
    );
}

#[tokio::test]
async fn a_private_repository_hands_out_signed_addresses() {
    let (github, _) = start_github(Github {
        release: Some(release()),
        private: true,
        ..Default::default()
    })
    .await;

    let server = server_for(&github, Some(TOKEN)).await;
    let (_, body) = get_json(&format!("{server}/api/release")).await;
    assert_eq!(body["release"]["tag"], "v0.2.0");
    assert_eq!(
        redirect_of(&format!("{server}/download/windows"))
            .await
            .as_deref(),
        Some("https://objects.example.net/11?signature=abc")
    );

    // Without the token, GitHub acts as if there were nothing there.
    let server = server_for(&github, None).await;
    let (status, body) = get_json(&format!("{server}/api/release")).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "release": null })));
    assert_eq!(
        redirect_of(&format!("{server}/download/windows")).await,
        None
    );
}

#[tokio::test]
async fn a_repository_without_releases_offers_nothing() {
    let (github, _) = start_github(Github::default()).await;
    let server = server_for(&github, None).await;

    let (status, body) = get_json(&format!("{server}/api/release")).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "release": null })));
    assert_eq!(
        redirect_of(&format!("{server}/download/windows")).await,
        None
    );
}

#[tokio::test]
async fn github_is_asked_once_for_many_visitors() {
    let (github, state) = start_github(Github {
        release: Some(release()),
        ..Default::default()
    })
    .await;
    let server = server_for(&github, None).await;

    for _ in 0..5 {
        let (status, _) = get_json(&format!("{server}/api/release")).await;
        assert_eq!(status, StatusCode::OK);
        redirect_of(&format!("{server}/download/windows")).await;
    }
    assert_eq!(state.lookups.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unreachable_github_is_reported() {
    // Nothing listens on a port that was just given up.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let github = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let server = server_for(&github, None).await;

    let (status, body) = get_json(&format!("{server}/api/release")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("GitHub"), "{body}");
}

#[tokio::test]
async fn a_server_without_releases_says_so() {
    let server = start_server(None).await;
    let (status, body) = get_json(&format!("{server}/api/release")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn the_page_and_its_parts_are_served() {
    let server = start_server(None).await;
    for (path, kind) in [
        ("/", "text/html"),
        ("/style.css", "text/css"),
        ("/app.js", "text/javascript"),
        ("/icon.png", "image/png"),
    ] {
        let reply = client()
            .get(format!("{server}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::OK, "{path}");
        let content_type = reply.headers()[header::CONTENT_TYPE].to_str().unwrap();
        assert!(content_type.starts_with(kind), "{path}: {content_type}");
        assert!(!reply.bytes().await.unwrap().is_empty(), "{path} is empty");
    }
}
