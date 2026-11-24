# flowerchat

Peer-to-peer voice chat and screen sharing for a small group of friends.
Media flows directly between peers over WebRTC; the server only helps them
find each other.

## Layout

- `crates/protocol`: the signaling messages shared by server and clients.
- `crates/server`: the signaling server. It also serves a browser test client.
- `crates/client`: the client engine (signaling, calls, audio) and a
  command-line client built on it.
- `crates/app`: the desktop app, a window on top of the same engine.

## Running the server

```sh
cargo run -p flowerchat-server
```

Then open <http://127.0.0.1:8080> in two browser tabs, join the same room in
both, and talk. Use headphones, or the two tabs will feed back into each other.

Configuration is through environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `FLOWERCHAT_LISTEN` | `127.0.0.1:8080` | Address to listen on. |
| `FLOWERCHAT_ICE_SERVERS` | a public STUN server | JSON array of ICE servers in `RTCIceServer` form, sent to every client. |

Browsers only allow microphone access on `localhost` or over HTTPS, so anything
beyond local testing needs to sit behind a TLS reverse proxy.

The test page accepts `?name=…&room=…` to prefill the form, and `&autojoin` to
join without clicking.

## Running the app

The app and the command-line client need GStreamer 1.28 with the base, good
and bad plugin sets, the PipeWire plugin and `gst-plugin-rsrtp`.

```sh
cargo run -p flowerchat-app
```

Enter a name, a room and the server's address, and join. The window shows
who is in the room with each connection's route, delay, loss, jitter and
bitrate, a volume slider per person, and a chat whose timeline also records
what happens in the room. Below 900 px wide the columns collapse into one.

The audio panel is on the join screen, where a meter shows that the
microphone works, and under the list of people once in a room (behind the
Audio button in a narrow window). Everything on it takes effect at once,
also in the middle of a call:

- the microphone and the speakers, with a button that plays a test sound;
- echo cancellation, noise suppression and automatic volume, each on its own;
- the receive buffer: lower means less delay, higher tolerates more jitter;
- whether voice is sent always or only while a key is held.

A device that was chosen and is then unplugged gives way to the system
default, and is used again when it comes back. The name, room, server and
audio settings are remembered between runs.

`--name`, `--room` and `--server` prefill the form and `--autojoin` skips it.
`--input`, `--output`, `--jitter-ms` and `--raw-mic` set the corresponding
audio settings, as in the command-line client below.

### Push-to-talk

The key chosen with "Set key" works while the window has focus. For a key
that works everywhere, the app registers a shortcut named `push-to-talk` with
the desktop, through the GlobalShortcuts portal, the first time push-to-talk
is turned on. Which key triggers it is then set in the desktop, not in the
app.

Desktops only give such shortcuts to apps they can identify, so the launcher
entry has to be installed, under exactly this name:

```sh
install -Dm644 crates/app/assets/se.flowerboi.flowerchat.desktop \
    ~/.local/share/applications/se.flowerboi.flowerchat.desktop
```

Its `Exec` line assumes `flowerchat` is on the `PATH`; change it if not.
Without the entry, the audio panel says that the desktop does not know the
app, and only the key chosen in the window works.

On Hyprland the shortcut is bound with the `global` dispatcher, as
`se.flowerboi.flowerchat:push-to-talk`; `hyprctl globalshortcuts` lists it
while the app runs.

The interface is drawn from a plain data model, so `crates/app/tests`
renders every screen from fixed data and compares the result with the images
in `crates/app/tests/snapshots`. After an intended visual change, accept the
new images with `UPDATE_SNAPSHOTS=1 cargo test -p flowerchat-app`.

## Running the command-line client

```sh
cargo run -p flowerchat-client -- --name martin
```

This joins the room `lounge` on the local server and talks to whoever is
there, including browser tabs and the app. `--help` lists the options; the
ones that matter most:

| Option | Meaning |
| --- | --- |
| `--room`, `--server` | Where to join. |
| `--input`, `--output` | PipeWire/PulseAudio device names, instead of the defaults. |
| `--jitter-ms` | Receive buffer, 40 ms by default. Lower means less delay but less tolerance for network jitter. |
| `--raw-mic` | Skip echo cancellation, noise suppression and automatic gain. |
| `--stats` | Print each connection's statistics and levels once a second. |
| `--say` | Send a chat message to each peer once connected. |

Set `RUST_LOG=flowerchat_client=debug` to see the SDP being exchanged.

### Measuring latency

Two clients on the same machine can time the audio path between them without
any audio device:

```sh
cargo run -p flowerchat-client -- --name a --test-tone --measure
cargo run -p flowerchat-client -- --name b --test-tone --measure
```

`--test-tone` replaces the microphone with one click per second, aligned to
whole seconds of the system's monotonic clock. `--measure` replaces the
speakers with a detector that reports how old each click is when it would be
played. The figure covers encoding, the network, the receive buffer, decoding
and mixing. It does not include the buffers of the microphone and speaker
devices, or anything the hardware adds.

## Signaling protocol

Clients connect to `/ws` and exchange JSON messages, each with a `type` field.

| Direction | Message | Purpose |
| --- | --- | --- |
| client to server | `{"type":"join","room":"…","name":"…"}` | Must be sent first. |
| client to server | `{"type":"signal","to":2,"data":{…}}` | Relay `data` to a peer in the same room. |
| server to client | `{"type":"welcome","id":3,"peers":[…],"iceServers":[…]}` | Join accepted; lists the peers already present. |
| server to client | `{"type":"peer-joined","id":4,"name":"…"}` | Someone joined the room. |
| server to client | `{"type":"peer-left","id":4}` | Someone left the room. |
| server to client | `{"type":"signal","from":2,"data":{…}}` | Relayed from another peer. |
| server to client | `{"type":"error","message":"…"}` | Sent before the server closes the connection. |

`data` carries either `{"description": …}` (an SDP offer or answer) or
`{"candidate": …}` (an ICE candidate). The server does not inspect it.
A room holds at most 8 peers.

Of two peers, the one with the lower id makes the offer. Each connection also
carries a data channel named `control`, created by both sides with
`negotiated: true` and id 0, for small JSON messages: `{"muted": true}` for
status and `{"chat": "…"}` for a chat message.

## Tests

```sh
cargo test --workspace
```

The client's end-to-end tests start a server and two clients, check that
they measure a plausible latency, and change settings in the middle of the
call. They need a network interface other than loopback, but no audio device.

One test is left out of that, and is run on request:

```sh
cargo test -p flowerchat-app -- --ignored
```

It runs the whole app without showing a window, joins a room, and works the
audio panel like a user would, checking what another participant and the
sound devices get to hear. For that it adds three virtual sound devices to
the system with `pactl` and removes them when it ends. It takes a minute, and
neither plays on nor listens to a real device.
