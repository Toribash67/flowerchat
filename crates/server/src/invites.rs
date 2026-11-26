//! Who may join, on a server that only lets invited people in.
//!
//! Each invited person gets a token: a long random string that their client
//! sends with its `join`. The server keeps a file of the names it has
//! invited, and with each a hash of the token rather than the token itself,
//! so that the file is of no use to someone who reads it.
//!
//! The file is read again at every join. Inviting someone or taking an
//! invite back therefore needs no restart, which would drop everyone from
//! their calls.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

/// Random bytes in a token. It is written in hex, so twice as many letters.
const TOKEN_BYTES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// Whom the invite is for. Only the server's owner sees it: in the file,
    /// in the list and in the log. It is not the name shown in a room.
    pub name: String,
    /// SHA-256 of the token, in hex.
    pub token_sha256: String,
    /// The rooms the invite is good for. Empty means every room.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rooms: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    invites: Vec<Invite>,
}

/// Why a join was turned away. The text is shown to whoever tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Unknown,
    OtherRoom,
    /// The file could not be read. Nobody gets in until it can.
    Unreadable,
}

impl Refusal {
    pub fn message(self) -> &'static str {
        match self {
            Self::Missing => "this server needs an invite",
            Self::Unknown => "this invite is not valid, or no longer",
            Self::OtherRoom => "this invite is not for this room",
            Self::Unreadable => "the server cannot check invites right now",
        }
    }
}

/// The file of invites of a running server.
pub struct Invites {
    path: PathBuf,
}

impl Invites {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The name of the invite that `token` belongs to, if it lets its
    /// holder into `room`.
    pub fn check(&self, token: Option<&str>, room: &str) -> Result<String, Refusal> {
        let invite = self.find(token)?;
        if !invite.rooms.is_empty() && !invite.rooms.iter().any(|allowed| allowed == room) {
            return Err(Refusal::OtherRoom);
        }
        Ok(invite.name)
    }

    /// The invite that `token` belongs to.
    pub fn find(&self, token: Option<&str>) -> Result<Invite, Refusal> {
        let token = token.map(clean).filter(|token| !token.is_empty());
        let token = token.ok_or(Refusal::Missing)?;
        let file = read(&self.path).map_err(|err| {
            tracing::warn!(path = %self.path.display(), %err, "cannot read the invites");
            Refusal::Unreadable
        })?;
        let hash = hash(token);
        file.invites
            .into_iter()
            .find(|invite| invite.token_sha256 == hash)
            .ok_or(Refusal::Unknown)
    }
}

/// Invites `name`, to `rooms` or, if there are none, to every room, and
/// returns the token to hand over. Whoever had an invite under that name
/// has it replaced: the old token stops working.
pub fn invite(path: &Path, name: &str, rooms: &[String]) -> io::Result<String> {
    let mut file = read_or_empty(path)?;
    let mut bytes = [0u8; TOKEN_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("the system gave no random numbers"))?;
    let token = hex(&bytes);
    file.invites.retain(|invite| invite.name != name);
    file.invites.push(Invite {
        name: name.to_owned(),
        token_sha256: hash(&token),
        rooms: rooms.to_vec(),
    });
    write(path, &file)?;
    Ok(token)
}

/// Takes back the invite under `name`. Returns whether there was one.
pub fn revoke(path: &Path, name: &str) -> io::Result<bool> {
    let mut file = read_or_empty(path)?;
    let before = file.invites.len();
    file.invites.retain(|invite| invite.name != name);
    if file.invites.len() == before {
        return Ok(false);
    }
    write(path, &file)?;
    Ok(true)
}

/// Everyone invited, in the order they were.
pub fn list(path: &Path) -> io::Result<Vec<Invite>> {
    Ok(read_or_empty(path)?.invites)
}

/// The token in what someone pasted: the token itself, or a link to the
/// page that carries it as `invite=`.
pub fn clean(pasted: &str) -> &str {
    let pasted = pasted.trim();
    let token = pasted
        .split_once("invite=")
        .map_or(pasted, |(_, rest)| rest);
    token.split(['&', '#']).next().unwrap_or(token)
}

fn read(path: &Path) -> io::Result<File> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// A file that is not there yet is one with nobody in it.
fn read_or_empty(path: &Path) -> io::Result<File> {
    match read(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(File::default()),
        other => other,
    }
}

/// Writes beside the file and then moves it into place, so that a server
/// reading at that moment sees the old list or the new one, never half.
fn write(path: &Path, file: &File) -> io::Result<()> {
    let mut text = serde_json::to_string_pretty(file).expect("invites always serialize");
    text.push('\n');
    let mut beside = path.as_os_str().to_owned();
    beside.push(".new");
    let beside = PathBuf::from(beside);
    fs::write(&beside, text)?;
    fs::rename(&beside, path)
}

fn hash(token: &str) -> String {
    hex(digest(&SHA256, token.as_bytes()).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a string cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flowerchat-invites-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn an_invite_lets_its_holder_in_until_it_is_taken_back() {
        let path = scratch("basic.json");
        let token = invite(&path, "anna", &[]).unwrap();
        let invites = Invites::new(path.clone());

        assert_eq!(invites.check(Some(&token), "lounge"), Ok("anna".into()));
        assert_eq!(invites.check(None, "lounge"), Err(Refusal::Missing));
        assert_eq!(invites.check(Some("  "), "lounge"), Err(Refusal::Missing));
        assert_eq!(
            invites.check(Some("guess"), "lounge"),
            Err(Refusal::Unknown)
        );

        assert!(revoke(&path, "anna").unwrap());
        assert!(!revoke(&path, "anna").unwrap());
        assert_eq!(invites.check(Some(&token), "lounge"), Err(Refusal::Unknown));
    }

    #[test]
    fn an_invite_can_be_for_some_rooms_only() {
        let path = scratch("rooms.json");
        let token = invite(&path, "bo", &["games".into()]).unwrap();
        let invites = Invites::new(path);

        assert_eq!(invites.check(Some(&token), "games"), Ok("bo".into()));
        assert_eq!(
            invites.check(Some(&token), "lounge"),
            Err(Refusal::OtherRoom)
        );
    }

    #[test]
    fn inviting_again_replaces_the_old_token() {
        let path = scratch("again.json");
        let old = invite(&path, "anna", &[]).unwrap();
        let new = invite(&path, "anna", &[]).unwrap();
        let invites = Invites::new(path.clone());

        assert_ne!(old, new);
        assert_eq!(invites.check(Some(&old), "lounge"), Err(Refusal::Unknown));
        assert_eq!(invites.check(Some(&new), "lounge"), Ok("anna".into()));
        assert_eq!(list(&path).unwrap().len(), 1);
    }

    #[test]
    fn the_file_holds_no_token() {
        let path = scratch("hashed.json");
        let token = invite(&path, "anna", &[]).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains(&token));
    }

    #[test]
    fn nobody_gets_in_while_the_file_cannot_be_read() {
        let invites = Invites::new(scratch("missing.json"));
        assert_eq!(
            invites.check(Some("any"), "lounge"),
            Err(Refusal::Unreadable)
        );
    }

    #[test]
    fn a_pasted_link_gives_its_token() {
        assert_eq!(clean(" abc123 \n"), "abc123");
        assert_eq!(clean("https://chat.example.org/?invite=abc123"), "abc123");
        assert_eq!(
            clean("https://chat.example.org/?invite=abc123&room=lounge"),
            "abc123"
        );
    }
}
