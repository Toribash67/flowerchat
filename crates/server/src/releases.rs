//! The installers the web page offers: whatever the latest release of the
//! repository has attached, as GitHub lists it.
//!
//! The browser is sent to each file's address on GitHub; the file itself
//! never passes through here. A private repository's files cannot be had
//! that way, so with a token the server instead asks GitHub for an address
//! that works without signing in, valid for a few minutes, and sends the
//! browser there.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// How long a release that was looked up is shown without asking again.
/// Unauthenticated, GitHub answers 60 questions an hour.
const FRESH_FOR: Duration = Duration::from_secs(5 * 60);
/// When GitHub cannot be reached, how soon to try again while showing what
/// was found before.
const RETRY_AFTER: Duration = Duration::from_secs(30);
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct ReleasesConfig {
    /// The repository whose releases are offered, as `owner/name`.
    pub repo: String,
    /// A token that may read the repository. Only needed if it is private.
    pub token: Option<String>,
    /// Where GitHub's API is: `https://api.github.com`, or a stand-in.
    pub api: String,
}

/// What the page is told about the latest release.
#[derive(Debug, Clone, Serialize)]
pub struct Release {
    pub tag: String,
    pub name: String,
    /// When it was published, as GitHub writes it (RFC 3339).
    pub published: Option<String>,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Asset {
    pub name: String,
    pub platform: &'static str,
    /// In bytes.
    pub size: u64,
    /// Where the page links to, on this server.
    pub download: String,
    #[serde(skip)]
    id: u64,
    #[serde(skip)]
    public_url: String,
}

/// The parts of GitHub's answer that are used.
#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    published_at: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    id: u64,
    name: String,
    size: u64,
    browser_download_url: String,
}

pub struct Releases {
    config: ReleasesConfig,
    client: reqwest::Client,
    cache: Mutex<Option<Cached>>,
}

struct Cached {
    /// Until when it is shown without asking GitHub again.
    until: Instant,
    release: Option<Release>,
}

pub enum Lookup {
    Found(Release),
    /// The repository has no published release yet.
    None,
    Failed(String),
}

impl Releases {
    pub fn new(config: ReleasesConfig) -> Self {
        // rustls needs to be told which cryptography to use; it may have
        // been told already, by whoever made the first `Releases`.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .user_agent("flowerchat-server")
            .timeout(TIMEOUT)
            // A download address is handed on to the browser, not fetched.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the HTTP client's settings are fixed");
        Self {
            config,
            client,
            cache: Mutex::new(None),
        }
    }

    /// The latest release, from the cache while that is fresh.
    pub async fn latest(&self) -> Lookup {
        let stale = {
            let cache = self.cache.lock().unwrap();
            match &*cache {
                Some(cached) if Instant::now() < cached.until => {
                    return found(cached.release.clone());
                }
                Some(cached) => Some(cached.release.clone()),
                None => None,
            }
        };
        match self.fetch_latest().await {
            Ok(release) => {
                *self.cache.lock().unwrap() = Some(Cached {
                    until: Instant::now() + FRESH_FOR,
                    release: release.clone(),
                });
                found(release)
            }
            Err(err) => {
                warn!("cannot look up the latest release: {err}");
                let Some(release) = stale else {
                    return Lookup::Failed(err);
                };
                // What was found before is still the best guess. Ask again
                // soon, but not on every request.
                *self.cache.lock().unwrap() = Some(Cached {
                    until: Instant::now() + RETRY_AFTER,
                    release: release.clone(),
                });
                found(release)
            }
        }
    }

    async fn fetch_latest(&self) -> Result<Option<Release>, String> {
        let url = format!(
            "{}/repos/{}/releases/latest",
            self.config.api, self.config.repo
        );
        let response = self
            .github(&url)
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|err| format!("GitHub cannot be reached: {err}"))?;
        // Also what GitHub says about a private repository to someone who
        // may not read it, which a missing or expired token makes us.
        if response.status() == StatusCode::NOT_FOUND {
            info!(
                repo = self.config.repo,
                "no release published, or the repository is private and needs \
                 FLOWERCHAT_GITHUB_TOKEN"
            );
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(format!("GitHub answered {}", response.status()));
        }
        let release: GithubRelease = response
            .json()
            .await
            .map_err(|err| format!("GitHub's answer was not understood: {err}"))?;
        Ok(Some(Release {
            name: release
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| release.tag_name.clone()),
            tag: release.tag_name,
            published: release.published_at,
            assets: release
                .assets
                .into_iter()
                .filter_map(|asset| {
                    Some(Asset {
                        platform: platform_of(&asset.name)?,
                        download: format!("/download/{}", asset.name),
                        name: asset.name,
                        size: asset.size,
                        id: asset.id,
                        public_url: asset.browser_download_url,
                    })
                })
                .collect(),
        }))
    }

    /// Sends the browser to the file called `wanted` in the latest release,
    /// or to the one for the platform called that.
    pub async fn download(&self, wanted: &str) -> Response {
        let release = match self.latest().await {
            Lookup::Found(release) => release,
            Lookup::None => return not_found(),
            Lookup::Failed(err) => return (StatusCode::BAD_GATEWAY, err).into_response(),
        };
        let Some(asset) = release
            .assets
            .iter()
            .find(|asset| asset.name == wanted)
            .or_else(|| release.assets.iter().find(|asset| asset.platform == wanted))
        else {
            return not_found();
        };
        if self.config.token.is_none() {
            return Redirect::to(&asset.public_url).into_response();
        }

        let url = format!(
            "{}/repos/{}/releases/assets/{}",
            self.config.api, self.config.repo, asset.id
        );
        let response = match self
            .github(&url)
            .header(header::ACCEPT, "application/octet-stream")
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                warn!("cannot ask GitHub for {}: {err}", asset.name);
                return (StatusCode::BAD_GATEWAY, "GitHub cannot be reached").into_response();
            }
        };
        if response.status().is_redirection()
            && let Some(location) = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
        {
            return Redirect::to(location).into_response();
        }
        if response.status().is_success() {
            // GitHub may also hand over the file itself, which then has to
            // be passed on.
            return pass_on(&asset.name, response);
        }
        warn!(
            "GitHub answered {} when asked for {}",
            response.status(),
            asset.name
        );
        (
            StatusCode::BAD_GATEWAY,
            "GitHub would not hand over the file",
        )
            .into_response()
    }

    fn github(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self
            .client
            .get(url)
            .header("X-GitHub-Api-Version", "2022-11-28");
        match &self.config.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}

fn found(release: Option<Release>) -> Lookup {
    match release {
        Some(release) => Lookup::Found(release),
        None => Lookup::None,
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "No such download").into_response()
}

fn pass_on(name: &str, response: reqwest::Response) -> Response {
    let length = response.content_length();
    let mut reply = Body::from_stream(response.bytes_stream()).into_response();
    let headers = reply.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Some(length) = length {
        headers.insert(header::CONTENT_LENGTH, length.into());
    }
    // Asset names are what GitHub accepted as file names; anything that
    // will not go in a header is left to the browser to name.
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    reply
}

/// Which platform a release file is for, judging by its name. Files for
/// none that is known, such as checksums, are not offered.
pub fn platform_of(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    let ends = |suffixes: &[&str]| suffixes.iter().any(|suffix| name.ends_with(suffix));
    if ends(&[".exe", ".msi", ".msix"]) {
        Some("windows")
    } else if ends(&[
        ".appimage",
        ".deb",
        ".rpm",
        ".flatpak",
        ".pkg.tar.zst",
        ".pkg.tar.xz",
    ]) || (name.contains("linux") && ends(&[".tar.gz", ".tar.xz", ".tar.zst", ".zip"]))
    {
        Some("linux")
    } else if ends(&[".dmg", ".pkg"]) {
        Some("macos")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::platform_of;

    #[test]
    fn installers_are_told_apart_by_their_names() {
        assert_eq!(platform_of("flowerchat-0.1.0-setup.exe"), Some("windows"));
        assert_eq!(
            platform_of("flowerchat-0.1.0-x86_64.AppImage"),
            Some("linux")
        );
        assert_eq!(platform_of("flowerchat_0.1.0_amd64.deb"), Some("linux"));
        assert_eq!(platform_of("flowerchat-0.1.0-linux.tar.gz"), Some("linux"));
        assert_eq!(
            platform_of("flowerchat-0.1.0-1-x86_64.pkg.tar.zst"),
            Some("linux")
        );
        assert_eq!(
            platform_of("flowerchat-0.1.0-1-x86_64.pkg.tar.zst.sig"),
            None
        );
        assert_eq!(platform_of("flowerchat-0.1.0.tar.gz"), None);
        assert_eq!(platform_of("SHA256SUMS"), None);
        assert_eq!(platform_of("flowerchat-0.1.0-setup.exe.sha256"), None);
    }
}
