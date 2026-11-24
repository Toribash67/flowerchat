//! What the interface shows about a room, built up from session events.
//!
//! The views only read this, so a model filled in by hand renders exactly
//! like a live one. The snapshot tests rely on that.

use std::time::{Duration, Instant};

use flowerchat_client::session::{ConnectionState, Event, PeerStats, Sharing, WatchStats};
use flowerchat_protocol::PeerId;

/// Louder than this counts as speaking.
const SPEAKING_ABOVE_DB: f32 = -40.0;
/// How long the speaking indicator stays lit after the last loud moment, so
/// it does not flicker between syllables.
const SPEAKING_HOLD: Duration = Duration::from_millis(300);
/// The bottom of the level meters. Quieter than this shows as empty.
const METER_FLOOR_DB: f32 = -60.0;

#[derive(Debug, Clone)]
pub struct RoomModel {
    pub room: String,
    /// Host name of the signaling server.
    pub server: String,
    pub in_room: Duration,
    pub me: Me,
    pub peers: Vec<PeerRow>,
    /// Chat messages and room events, oldest first.
    pub timeline: Vec<Entry>,
    /// The screen we are watching, if any.
    pub watching: Option<Watching>,
}

/// Someone's shared screen, open on ours.
#[derive(Debug, Clone, PartialEq)]
pub struct Watching {
    pub id: PeerId,
    /// How it has gone over the last second.
    pub stats: WatchStats,
}

/// Where sharing our own screen stands.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum OwnShare {
    #[default]
    NotSharing,
    /// Asked for, and not under way yet: the desktop may still be asking
    /// which screen it is to be.
    Starting,
    Sharing(Sharing),
}

#[derive(Debug, Clone)]
pub struct Me {
    pub name: String,
    pub muted: bool,
    pub deafened: bool,
    /// Microphone level from 0 (silent) to 1 (full scale).
    pub mic_level: f32,
    pub sending_kbps: Option<f64>,
    pub share: OwnShare,
}

#[derive(Debug, Clone)]
pub struct PeerRow {
    pub id: PeerId,
    pub name: String,
    pub state: ConnectionState,
    pub muted: bool,
    pub speaking: bool,
    pub stats: PeerStats,
    /// What this peer shares of its screen, if anything.
    pub sharing: Option<Sharing>,
    /// Playback gain, 1.0 being unchanged.
    pub volume: f32,
    /// When this peer was last loud enough to count as speaking.
    last_loud: Option<Instant>,
    /// Whether we have had a mute status from this peer yet. The first one
    /// is its starting state and not news.
    heard_status: bool,
}

impl PeerRow {
    pub fn new(id: PeerId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            state: ConnectionState::Connecting,
            muted: false,
            speaking: false,
            stats: PeerStats::default(),
            sharing: None,
            volume: 1.0,
            last_loud: None,
            heard_status: false,
        }
    }

    pub fn connected(&self) -> bool {
        self.state == ConnectionState::Connected
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Wall-clock time as `HH:MM`.
    pub time: String,
    pub kind: EntryKind,
}

#[derive(Debug, Clone)]
pub enum EntryKind {
    /// Something that happened in the room.
    Event(String),
    Message {
        from: String,
        text: String,
    },
}

impl RoomModel {
    pub fn new(room: &str, server: &str, my_name: &str) -> Self {
        Self {
            room: room.to_owned(),
            server: server.to_owned(),
            in_room: Duration::ZERO,
            me: Me {
                name: my_name.to_owned(),
                muted: false,
                deafened: false,
                mic_level: 0.0,
                sending_kbps: None,
                share: OwnShare::NotSharing,
            },
            peers: Vec::new(),
            timeline: Vec::new(),
            watching: None,
        }
    }

    pub fn peer(&self, id: PeerId) -> Option<&PeerRow> {
        self.peers.iter().find(|peer| peer.id == id)
    }

    /// How many of the others share a screen.
    pub fn sharing_count(&self) -> usize {
        self.peers
            .iter()
            .filter(|peer| peer.sharing.is_some())
            .count()
    }

    /// The names of those in `ids`, with ourselves left out: the list
    /// comes from someone who knows us by our id, which we do not show.
    fn names(&self, ids: &[PeerId]) -> Vec<String> {
        ids.iter()
            .filter_map(|id| self.peer(*id))
            .map(|peer| peer.name.clone())
            .collect()
    }

    /// Who watches the screen that `id` shares, for display. We come first
    /// if we are among them.
    pub fn watchers_of(&self, id: PeerId) -> Vec<String> {
        let Some(sharing) = self.peer(id).and_then(|peer| peer.sharing.as_ref()) else {
            return Vec::new();
        };
        let others = self.names(&sharing.watchers);
        let we_watch = self.watching.as_ref().is_some_and(|w| w.id == id);
        we_watch
            .then(|| "you".to_owned())
            .into_iter()
            .chain(others)
            .collect()
    }

    /// Who watches our screen, for display.
    pub fn my_watchers(&self) -> Vec<String> {
        match &self.me.share {
            OwnShare::Sharing(sharing) => self.names(&sharing.watchers),
            _ => Vec::new(),
        }
    }

    /// Opens `id`'s screen. The caller tells the session.
    pub fn watch(&mut self, id: PeerId, clock: &str) {
        let Some(name) = self.peer(id).map(|peer| peer.name.clone()) else {
            return;
        };
        self.watching = Some(Watching {
            id,
            stats: WatchStats::default(),
        });
        self.log(clock, format!("You started watching {name}'s screen"));
    }

    /// Closes the screen we are watching. The caller tells the session.
    pub fn stop_watching(&mut self, clock: &str) {
        if let Some(watching) = self.watching.take()
            && let Some(name) = self.peer(watching.id).map(|peer| peer.name.clone())
        {
            self.log(clock, format!("You stopped watching {name}'s screen"));
        }
    }

    pub fn connected_count(&self) -> usize {
        self.peers.iter().filter(|peer| peer.connected()).count()
    }

    /// Folds one session event into the model. `clock` is the current
    /// wall-clock time as `HH:MM`, for the timeline.
    pub fn apply(&mut self, event: Event, now: Instant, clock: &str) {
        match event {
            Event::Joined { .. } => {
                let text = format!("You joined {}", self.room);
                self.log(clock, text);
            }
            Event::PeerJoined { id, name } => {
                self.log(clock, format!("{name} joined, connecting"));
                self.peers.push(PeerRow::new(id, name));
            }
            Event::PeerLeft { id } => {
                if let Some(index) = self.peers.iter().position(|peer| peer.id == id) {
                    let peer = self.peers.remove(index);
                    self.log(clock, format!("{} left", peer.name));
                }
                if self.watching.as_ref().is_some_and(|w| w.id == id) {
                    self.watching = None;
                }
            }
            Event::PeerState { id, state } => {
                let Some(peer) = self.peer_mut(id) else {
                    return;
                };
                if peer.state == state {
                    return;
                }
                peer.state = state;
                let name = peer.name.clone();
                let what = match state {
                    ConnectionState::Connected => "connected",
                    ConnectionState::Disconnected => "lost connection",
                    ConnectionState::Failed => "could not be reached",
                    ConnectionState::Connecting | ConnectionState::Closed => return,
                };
                self.log(clock, format!("{name} {what}"));
            }
            Event::PeerMuted { id, muted } => {
                let Some(peer) = self.peer_mut(id) else {
                    return;
                };
                let news = peer.heard_status && peer.muted != muted;
                peer.heard_status = true;
                peer.muted = muted;
                if news {
                    let verb = if muted { "muted" } else { "unmuted" };
                    let text = format!("{} {verb}", peer.name);
                    self.log(clock, text);
                }
            }
            Event::PeerLevel { id, db } => {
                if let Some(peer) = self.peer_mut(id)
                    && db > SPEAKING_ABOVE_DB
                {
                    peer.last_loud = Some(now);
                }
            }
            Event::MicLevel { db } => self.me.mic_level = meter_level(db),
            Event::PeerStats { id, stats } => {
                if stats.sending_kbps.is_some() {
                    self.me.sending_kbps = stats.sending_kbps;
                }
                if let Some(peer) = self.peer_mut(id) {
                    peer.stats = stats;
                }
            }
            Event::Chat { from, text } => {
                let from = self
                    .peers
                    .iter()
                    .find(|peer| peer.id == from)
                    .map_or_else(|| format!("peer {from}"), |peer| peer.name.clone());
                self.say(clock, from, text);
            }
            Event::ClickDelay(_) => {}
            Event::OwnShare(Some(sharing)) => {
                let before = match &self.me.share {
                    OwnShare::Sharing(before) => before.watchers.clone(),
                    _ => {
                        self.log(clock, "You started sharing your screen".into());
                        Vec::new()
                    }
                };
                for (ids, others, what) in [
                    (&sharing.watchers, &before, "started watching your screen"),
                    (&before, &sharing.watchers, "stopped watching your screen"),
                ] {
                    for id in ids.iter().filter(|id| !others.contains(id)) {
                        // Someone who has left is not also reported to
                        // have stopped watching.
                        if let Some(name) = self.peer(*id).map(|peer| peer.name.clone()) {
                            self.log(clock, format!("{name} {what}"));
                        }
                    }
                }
                self.me.share = OwnShare::Sharing(sharing);
            }
            Event::OwnShare(None) => {
                if matches!(self.me.share, OwnShare::Sharing(_)) {
                    self.log(clock, "You stopped sharing your screen".into());
                }
                self.me.share = OwnShare::NotSharing;
            }
            Event::ShareFailed(reason) => {
                self.me.share = OwnShare::NotSharing;
                self.log(clock, format!("Your screen could not be shared: {reason}"));
            }
            Event::PeerSharing { id, sharing } => {
                let Some(peer) = self.peer_mut(id) else {
                    return;
                };
                let (was, is) = (peer.sharing.is_some(), sharing.is_some());
                peer.sharing = sharing;
                let name = peer.name.clone();
                if is && !was {
                    self.log(clock, format!("{name} started sharing a screen"));
                } else if was && !is {
                    self.log(clock, format!("{name} stopped sharing"));
                    if self.watching.as_ref().is_some_and(|w| w.id == id) {
                        self.watching = None;
                    }
                }
            }
            Event::Watching { id, stats } => {
                if let Some(watching) = &mut self.watching
                    && watching.id == id
                {
                    watching.stats = stats;
                }
            }
            Event::WatchFailed { id, reason } => {
                if self.watching.as_ref().is_some_and(|w| w.id == id) {
                    self.watching = None;
                }
                let name = self
                    .peer(id)
                    .map_or_else(|| format!("peer {id}"), |peer| peer.name.clone());
                self.log(clock, format!("{name}'s screen cannot be shown: {reason}"));
            }
        }
    }

    /// Updates everything that changes with time alone.
    pub fn tick(&mut self, now: Instant, joined_at: Instant) {
        self.in_room = now.saturating_duration_since(joined_at);
        for peer in &mut self.peers {
            let recently_loud = peer
                .last_loud
                .is_some_and(|at| now.saturating_duration_since(at) < SPEAKING_HOLD);
            peer.speaking = recently_loud && !peer.muted && peer.connected();
        }
    }

    pub fn say(&mut self, clock: &str, from: String, text: String) {
        self.timeline.push(Entry {
            time: clock.to_owned(),
            kind: EntryKind::Message { from, text },
        });
    }

    /// Records something that happened in the room.
    pub fn log(&mut self, clock: &str, text: String) {
        self.timeline.push(Entry {
            time: clock.to_owned(),
            kind: EntryKind::Event(text),
        });
    }

    fn peer_mut(&mut self, id: PeerId) -> Option<&mut PeerRow> {
        self.peers.iter_mut().find(|peer| peer.id == id)
    }
}

/// Maps decibels to a meter position from 0 to 1.
pub fn meter_level(db: f32) -> f32 {
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> RoomModel {
        let mut room = RoomModel::new("lounge", "example.org", "robin");
        let now = Instant::now();
        room.apply(
            Event::PeerJoined {
                id: 2,
                name: "alex".into(),
            },
            now,
            "21:00",
        );
        room.apply(
            Event::PeerState {
                id: 2,
                state: ConnectionState::Connected,
            },
            now,
            "21:00",
        );
        room
    }

    fn events(room: &RoomModel) -> Vec<&str> {
        room.timeline
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Event(text) => Some(text.as_str()),
                EntryKind::Message { .. } => None,
            })
            .collect()
    }

    #[test]
    fn speaking_lights_up_and_fades() {
        let mut room = room();
        let start = Instant::now();
        room.apply(Event::PeerLevel { id: 2, db: -20.0 }, start, "21:00");
        room.tick(start + Duration::from_millis(100), start);
        assert!(room.peers[0].speaking);
        room.tick(start + Duration::from_millis(500), start);
        assert!(!room.peers[0].speaking);
    }

    #[test]
    fn quiet_audio_is_not_speaking() {
        let mut room = room();
        let start = Instant::now();
        room.apply(Event::PeerLevel { id: 2, db: -55.0 }, start, "21:00");
        room.tick(start, start);
        assert!(!room.peers[0].speaking);
    }

    #[test]
    fn a_peers_first_mute_status_is_not_logged() {
        let mut room = room();
        let now = Instant::now();
        let muted = |muted| Event::PeerMuted { id: 2, muted };
        room.apply(muted(false), now, "21:00");
        assert_eq!(events(&room), ["alex joined, connecting", "alex connected"]);
        room.apply(muted(true), now, "21:01");
        room.apply(muted(false), now, "21:02");
        assert_eq!(events(&room)[2..], ["alex muted", "alex unmuted"]);
    }

    #[test]
    fn chat_is_attributed_by_name() {
        let mut room = room();
        let chat = Event::Chat {
            from: 2,
            text: "hi".into(),
        };
        room.apply(chat, Instant::now(), "21:03");
        let last = room.timeline.last().unwrap();
        assert!(
            matches!(&last.kind, EntryKind::Message { from, text } if from == "alex" && text == "hi")
        );
    }

    fn sharing(watchers: &[PeerId]) -> Sharing {
        Sharing {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 8700,
            sent_fps: 60.0,
            encode_ms: 4.0,
            watchers: watchers.to_vec(),
        }
    }

    #[test]
    fn a_share_is_logged_once_however_often_it_is_reported() {
        let mut room = room();
        let now = Instant::now();
        let report = |watchers: &[PeerId]| Event::PeerSharing {
            id: 2,
            sharing: Some(sharing(watchers)),
        };
        room.apply(report(&[]), now, "21:04");
        room.apply(report(&[3]), now, "21:04");
        assert_eq!(events(&room)[2..], ["alex started sharing a screen"]);
        assert_eq!(room.sharing_count(), 1);

        room.watch(2, "21:05");
        assert_eq!(room.watchers_of(2), ["you"]);
        let stopped = Event::PeerSharing {
            id: 2,
            sharing: None,
        };
        room.apply(stopped, now, "21:06");
        assert_eq!(room.watching, None, "nothing is left to watch");
        assert_eq!(
            events(&room)[3..],
            ["You started watching alex's screen", "alex stopped sharing"]
        );
    }

    #[test]
    fn our_own_watchers_come_and_go_in_the_timeline() {
        let mut room = room();
        let now = Instant::now();
        room.me.share = OwnShare::Starting;
        room.apply(Event::OwnShare(Some(sharing(&[]))), now, "21:04");
        room.apply(Event::OwnShare(Some(sharing(&[2]))), now, "21:05");
        room.apply(Event::OwnShare(Some(sharing(&[2]))), now, "21:05");
        assert_eq!(room.my_watchers(), ["alex"]);
        room.apply(Event::OwnShare(Some(sharing(&[]))), now, "21:06");
        room.apply(Event::OwnShare(None), now, "21:07");
        assert_eq!(
            events(&room)[2..],
            [
                "You started sharing your screen",
                "alex started watching your screen",
                "alex stopped watching your screen",
                "You stopped sharing your screen"
            ]
        );
        assert_eq!(room.me.share, OwnShare::NotSharing);
    }

    #[test]
    fn meter_runs_from_floor_to_full_scale() {
        assert_eq!(meter_level(-90.0), 0.0);
        assert_eq!(meter_level(-30.0), 0.5);
        assert_eq!(meter_level(0.0), 1.0);
    }
}
