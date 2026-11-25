# flowerchat

Peer-to-peer voice chat and screen sharing for a small group of friends.
Media flows directly between peers over WebRTC; the server only helps them
find each other.

## Layout

- `crates/protocol`: the signaling messages shared by server and clients.
- `crates/server`: the signaling server. It also serves the web page: a
  smaller flowerchat for the browser, and the downloads of the app.
- `crates/client`: the client engine (signaling, calls, audio, screen
  sharing) and a command-line client built on it.
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
| `FLOWERCHAT_RELEASES` | `Toribash67/flowerchat` | GitHub repository, as `owner/name`, whose latest release the page offers. Empty offers none. |
| `FLOWERCHAT_GITHUB_TOKEN` | none | A token that may read that repository. Only needed if it is private. |

Browsers only allow microphone access on `localhost` or over HTTPS, so anything
beyond local testing needs to sit behind a TLS reverse proxy.

### The web page

The page is flowerchat for those who have not installed it, or are not at
their own computer: voice and chat with whoever is in a room, app or
browser, a volume per person with the same figures for each connection as
the app, mute and deafen, and the choice of microphone and speakers with
the browser's echo cancellation, noise suppression and automatic volume.
Screens are not shared or watched there; it says who shares one, and links
to the app. Below 900 px wide it goes to one column with tabs, like the app,
and it follows the system in choosing a light or a dark palette.

Before joining, it also offers the installers of the latest release on
GitHub, with the one for the visitor's system first. `/download/windows`
always leads to the latest Windows installer, whatever its version, which
makes a link to hand out. Which release is latest is asked of GitHub at
most every five minutes, and a download goes straight to GitHub; the
installer does not pass through the server. A release is made by pushing a
tag like `v0.1.0`, which builds the Windows installer and attaches it.
Until there is one, the page says that nothing has been published yet.

Were the repository private again, its files could only be had with a
token: a fine-grained one for this repository alone, with read access to
its contents and nothing else, given to the server as
`FLOWERCHAT_GITHUB_TOKEN`. The server then gets an address valid for a few
minutes from GitHub for every download. Without the token, a private
repository looks to the server as if it had no release.

`?room=…` fills in the room, which is what the Invite button copies.
`?name=…` fills in the name, and `&autojoin` joins without clicking, for
scripted tests.

Two things about the app shape what the page sends. Whoever was in the room
first makes the offer, the browser included. And GStreamer, under the app,
turns down an answer in which a media section the browser refused lacks ICE
credentials or a DTLS role, as browsers write them; Firefox refuses the
app's HEVC video, for one. The page copies those into such sections of what
it sends.

## Running the app

The app and the command-line client run on Linux and on Windows. They need
GStreamer 1.28 with the base, good and bad plugin sets, and on Linux the
PipeWire plugin. Sharing a screen and watching one need an NVIDIA graphics
card: the picture is encoded and decoded as H.264 or HEVC on the card (NVENC
and NVDEC), and nothing else is tried.

What follows describes Linux. Where Windows differs, a paragraph says so;
`docs/windows-port.md` has how to build there and what is still missing.

On Windows nobody but a developer needs any of that: `installer\build.ps1`
makes one installer that brings along the parts of GStreamer flowerchat
uses.

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

The palette button beside the app's name, or in the bar at the top of a
narrow window, changes the colours of the interface: flowerchat's own, the
Gruvbox, One Half, Monokai and Catppuccin families, and a dozen more. A
click on one applies it at once and leaves the list open, so that they can
be tried one after the other. The choice is remembered too. A new palette
is one more entry in `crates/app/src/palettes.rs`, where a test holds every
palette to text that can be read on what it is written on.

`--name`, `--room` and `--server` prefill the form and `--autojoin` skips it.
`--input`, `--output`, `--jitter-ms` and `--raw-mic` set the corresponding
audio settings, as in the command-line client below.

### Sharing a screen

"Share screen" asks the desktop which screen or window to share, through the
ScreenCast portal, and the desktop shows its own picker. Sharing sends
nothing by itself: a share shows up for the others as a Watch button in the
Screen column, and the picture goes only to those who press it. It is
encoded once, however many watch.

On Windows, where any program may look at the screen, flowerchat asks
itself: a panel over the window lists the screens with their sizes and the
open windows with their programs, and a click on one starts the share.
Escape or "Cancel" closes it. Minimized windows are not listed, because
they have no picture to send.

Watching opens the screen in the middle of the window, with the people on
the left and the chat or the audio panel on the right. Under the picture
are its resolution, frame rate and bitrate as they arrive, the delay from
the sharer's screen to ours (encoding, half a round trip, the receive buffer
and decoding; what the capture and the display add is not known), and the
share of frames dropped here and of packets lost on the way. "Maximize"
gives the picture the whole window and "Pop out" a window of its own. In a
narrow window all of this is on the Screens tab.

Packets that go missing are sent again, which keeps a stream whole through
a few percent of loss. When that is not enough, the viewer asks for a frame
to start over from, and the picture freezes for a moment.

The first two pickers above the button set the tallest picture to send and
the frames per second, also in the middle of a share, where the picture
changes over without a break. A screen taller than the limit is scaled down;
a smaller one is sent as it is, up to 4096 pixels a side in H.264 and 8192
in HEVC.

The two below them set the bitrate and the codec, and can be changed in the
middle of a share as well: viewers carry on watching without doing anything.
Left on "Auto bitrate", the bitrate follows from size and frame rate: about
8.7 Mbit/s for 1080p at 60 frames per second and about 35 Mbit/s for 4K in
H.264, and three quarters of that in HEVC, which needs fewer bits for the
same picture and takes somewhat longer to encode. Otherwise it is held to
the figure picked, from 1 to 50 Mbit/s. Every viewer gets the whole stream,
so what leaves the machine is the bitrate times the number of viewers.

While sharing, the block about yourself shows what is being sent, the codec
and the bitrate aimed for, how long encoding a frame takes, and who watches.
`--share-height` (0 for the size the screen has) and `--share-fps` set the
first two pickers when starting the app.

With "With sound" ticked, viewers also hear what the machine plays: games,
videos, music, whatever starts while sharing. What flowerchat itself plays
is left out, so nobody gets the call back, and the microphone is not part
of it either. The sound travels as a stream of its own, in stereo, and each
viewer sets its volume apart from the voices, under the picture. For this,
flowerchat opens a recording stream in PipeWire and connects every other
program's playback to it, with PipeWire's own `pw-dump` and `pw-link`. Of a
surround mix, only the front channels are taken. On Windows, the system
records what plays and leaves flowerchat out, which takes Windows 11 or a
late build of Windows 10.

For trying things out without a second machine, `--share-test 1920x1080`
makes "Share screen" share a moving test picture of that size, with a tone
for its sound, without asking the desktop, and `--lose 2` loses 2% of the packets that arrive.

### Push-to-talk

On Windows, the key chosen with "Set key" works in every window, if it is
a letter, a digit, a function key or one of the keys for moving about.
Other keys work while the window has focus. The rest of this section is
about Linux.

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
| `--input`, `--output` | PipeWire/PulseAudio device names, instead of the defaults. On Windows, the ids `gst-device-monitor-1.0` shows as `device.id`. |
| `--jitter-ms` | Receive buffer, 40 ms by default. Lower means less delay but less tolerance for network jitter. |
| `--raw-mic` | Skip echo cancellation, noise suppression and automatic gain. |
| `--stats` | Print each connection's statistics and levels once a second. |
| `--say` | Send a chat message to each peer once connected. |
| `--share-test` | Share a moving test picture of this size, such as `1920x1080`. |
| `--share-node` | Linux: share a PipeWire video stream that is open to everyone, by its node id. |
| `--share-monitor` | Windows: share a monitor, by its number from zero. |
| `--share-height`, `--share-fps` | The tallest picture to send (0 for the size it has), and how many frames per second. |
| `--share-silent` | Share the picture without the sound that goes with it. |
| `--share-codec` | Codec to share in: `h264` (the default) or `hevc`. |
| `--share-kbps` | Bitrate to share at, in kbit/s. Without it, the bitrate follows size and frame rate. |
| `--watch` | Watch every screen that is shared, without showing it, and print how it goes. |
| `--lose` | Lose this many percent of the packets that arrive, to see what a poor network does. |

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
`negotiated: true` and id 0, for small JSON messages:

| Message | Meaning |
| --- | --- |
| `{"muted": true}` | The sender muted or unmuted. |
| `{"chat": "…"}` | A chat message. |
| `{"sharing": {…}}` | What the sender shares of its screen: `width`, `height`, `fps`, `codec` (`h264` or `hevc`) and `kbps` as set, `sentFps` and `encodeMs` as measured, `sound` for whether sound comes with it, and `watchers`, the ids of those it sends to. Repeated about once a second. |
| `{"sharing": null}` | The sender stopped sharing. |
| `{"watch": true}` | The sender wants the receiver's share, or with `false` no longer does. |

Native clients offer and accept video on every connection from the start,
one stream for H.264 and one for HEVC, each with retransmission, and a
second Opus stream for the sound of a shared screen, so that sharing,
watching and changing codec need no new negotiation. Nothing is sent on any
of them until a peer asks to watch, and then only on the stream of the codec
the sharer has picked. A browser
that offers only audio gets only audio.

## Deployment

The signaling server runs as a container; `deploy/dockge` has the stack and
what the reverse proxy needs. Pushes to `main` publish the image, and
Watchtower on the host picks it up.

The app and the command-line client reach a server behind TLS with a
`wss://` address, such as `wss://chat.example.org/ws`.

## Tests

```sh
cargo test --workspace
```

The client's end-to-end tests start a server and two clients, check that
they measure a plausible latency, and change settings in the middle of the
call. They need a network interface other than loopback, but no audio device.
The sharing tests, in the client and in the app, also need an NVIDIA
graphics card: one client shares a test picture, others watch it, one of
them through 2% packet loss, and the voice must stay as quick as it was. In
the app's test, the app itself shares and watches through its buttons,
without showing a window.

One test is left out of that, and is run on request:

```sh
cargo test -p flowerchat-app -- --ignored
```

It runs the whole app without showing a window, joins a room, and works the
audio panel like a user would, checking what another participant and the
sound devices get to hear. For that it adds three virtual sound devices to
the system with `pactl` and removes them when it ends. It takes a minute, and
neither plays on nor listens to a real device.
