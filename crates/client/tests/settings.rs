//! Changes audio settings in the middle of a call. Two clients time the
//! clicks they send each other, as in `selftest.rs`, so this needs no audio
//! device either.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use flowerchat_client::session::{self, AudioSettings, Command, Event, SessionConfig};
use flowerchat_server::{Config, app};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// Delays of the clicks a client has received, in milliseconds.
type Delays = Arc<Mutex<Vec<f64>>>;

struct Client {
    commands: UnboundedSender<Command>,
    delays: Delays,
    session: JoinHandle<anyhow::Result<()>>,
}

fn join(url: &str, name: &str) -> Client {
    let (commands, command_rx) = unbounded_channel();
    let (event_tx, mut events) = unbounded_channel();
    let config = SessionConfig {
        server: url.into(),
        room: "lounge".into(),
        name: name.into(),
        audio: AudioSettings::default(),
        transmitting: true,
        test_tone: true,
        measure: true,
        frames: None,
        incoming_loss: 0.0,
    };
    let session = tokio::spawn(session::run(config, command_rx, event_tx));
    let delays = Delays::default();
    let sink = delays.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let Event::ClickDelay(ns) = event {
                sink.lock().unwrap().push(ns as f64 / 1e6);
            }
        }
    });
    Client {
        commands,
        delays,
        session,
    }
}

/// The median delay of the clicks that arrive in the next few seconds.
async fn median_delay(client: &Client) -> f64 {
    client.delays.lock().unwrap().clear();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mut delays = std::mem::take(&mut *client.delays.lock().unwrap());
    assert!(delays.len() >= 2, "clicks keep arriving");
    delays.sort_by(f64::total_cmp);
    delays[delays.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
async fn the_receive_buffer_and_push_to_talk_change_mid_call() {
    gstreamer::init().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    // No STUN server: the two clients find each other on local addresses.
    let config = Config {
        ice_servers: vec![],
        releases: None,
    };
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });

    let first = join(&url, "first");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let second = join(&url, "second");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let before = median_delay(&first).await;

    // A bigger buffer on one side delays what that side hears, by as much
    // as the buffer grew, and nothing else.
    let bigger = AudioSettings {
        jitter_ms: 120,
        ..AudioSettings::default()
    };
    first.commands.send(Command::SetAudio(bigger)).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = median_delay(&first).await;
    let other_side = median_delay(&second).await;
    assert!(
        (after - before - 80.0).abs() < 15.0,
        "80 ms more buffer took the delay from {before:.1} to {after:.1} ms"
    );
    assert!(
        (other_side - before).abs() < 15.0,
        "the other direction stayed at {other_side:.1} ms"
    );

    // Without the push-to-talk key, nothing is sent.
    second
        .commands
        .send(Command::SetTransmitting(false))
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    first.delays.lock().unwrap().clear();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(first.delays.lock().unwrap().is_empty(), "the key is up");
    second
        .commands
        .send(Command::SetTransmitting(true))
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    median_delay(&first).await;

    for client in [first, second] {
        client.commands.send(Command::Leave).unwrap();
        client.session.await.unwrap().unwrap();
    }
}
