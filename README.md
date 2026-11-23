# flowerchat

Peer-to-peer voice chat and screen sharing for a small group of friends.
Media flows directly between peers over WebRTC; the server only helps them
find each other.

## Layout

- `crates/protocol`: the signaling messages shared by server and clients.
- `crates/server`: the signaling server. It also serves a browser test client.

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

## Tests

```sh
cargo test --workspace
```
