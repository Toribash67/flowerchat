//! Translates the ICE server list from the signaling server into the URI
//! forms `webrtcbin` expects.

use flowerchat_protocol::IceServer;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct IceConfig {
    /// `stun://host:port`. `webrtcbin` takes a single STUN server.
    pub stun: Option<String>,
    /// `turn://user:pass@host:port`, one per TURN server.
    pub turn: Vec<String>,
}

impl IceConfig {
    pub fn from_servers(servers: &[IceServer]) -> Self {
        let mut config = Self::default();
        for server in servers {
            for url in &server.urls {
                let Some((scheme, rest)) = url.split_once(':') else {
                    continue;
                };
                match scheme {
                    "stun" | "stuns" if config.stun.is_none() => {
                        config.stun = Some(format!("{scheme}://{rest}"));
                    }
                    "turn" | "turns" => {
                        let user = server.username.as_deref().unwrap_or_default();
                        let pass = server.credential.as_deref().unwrap_or_default();
                        config.turn.push(format!(
                            "{scheme}://{}:{}@{rest}",
                            percent_encode(user),
                            percent_encode(pass)
                        ));
                    }
                    _ => {}
                }
            }
        }
        config
    }
}

/// Escapes everything outside RFC 3986's unreserved set, so that user names
/// and passwords cannot be mistaken for URI delimiters.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(urls: &[&str], username: Option<&str>, credential: Option<&str>) -> IceServer {
        IceServer {
            urls: urls.iter().map(|u| u.to_string()).collect(),
            username: username.map(str::to_owned),
            credential: credential.map(str::to_owned),
        }
    }

    #[test]
    fn converts_browser_style_urls() {
        let config = IceConfig::from_servers(&[
            server(&["stun:stun.example.org:3478"], None, None),
            server(
                &["turn:turn.example.org:3478?transport=udp"],
                Some("1700000000:martin"),
                Some("p@ss/word"),
            ),
        ]);
        assert_eq!(config.stun.as_deref(), Some("stun://stun.example.org:3478"));
        assert_eq!(
            config.turn,
            vec!["turn://1700000000%3Amartin:p%40ss%2Fword@turn.example.org:3478?transport=udp"]
        );
    }

    #[test]
    fn keeps_only_the_first_stun_server() {
        let config = IceConfig::from_servers(&[server(&["stun:a:1", "stun:b:2"], None, None)]);
        assert_eq!(config.stun.as_deref(), Some("stun://a:1"));
        assert!(config.turn.is_empty());
    }
}
