# flowerchat

Peer-to-peer voice chat and screen sharing for a small group of friends.
Media flows directly between peers over WebRTC; the server only helps them
find each other.

## Layout

- `crates/protocol`: the signaling messages shared by server and clients.
- `crates/server`: the signaling server. It also serves a browser test client.
- `crates/client`: the native client, a command-line program for now.

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

## Running the native client

The client needs GStreamer 1.28 with the base, good and bad plugin sets, the
PipeWire plugin and `gst-plugin-rsrtp`.

```sh
cargo run -p flowerchat-client -- --name martin
```

This joins the room `lounge` on the local server and talks to whoever is
there, including browser tabs. `--help` lists the options; the ones that
matter most:

| Option | Meaning |
| --- | --- |
| `--room`, `--server` | Where to join. |
| `--input`, `--output` | PipeWire/PulseAudio device names, instead of the defaults. |
| `--jitter-ms` | Receive buffer, 40 ms by default. Lower means less delay but less tolerance for network jitter. |
| `--raw-mic` | Skip echo cancellation, noise suppression and automatic gain. |

Set `RUST_LOG=flowerchat=debug` to see the SDP being exchanged.

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
`negotiated: true` and id 0, for small JSON messages such as `{"muted": true}`.

## Tests

```sh
cargo test --workspace
```

The client's end-to-end test starts a server and two clients and checks that
they measure a plausible latency. It needs a network interface other than
loopback.
