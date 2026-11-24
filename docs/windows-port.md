# Porting the client to Windows

Notes for whoever picks this up on Windows, written on Linux on 2026-11-24
at commit `55c0ded`. Everything so far was built and tested on one Arch
Linux machine (Hyprland, PipeWire 1.6, GStreamer 1.28, RTX 2070 SUPER).
Nothing in here has been tried on Windows: where a Windows element or
property is named, it is where to start looking, not a tested answer.

## Where the port stands

Added on Windows 11 on 2026-11-24, on a machine with an RTX 2070 SUPER and
GStreamer 1.28.9. The rest of this file is the notes as they were written
on Linux beforehand; where they disagree with this section, this section
is what was found.

Working, and how each was checked:

- The workspace builds, and `cargo test --workspace` passes, including the
  app's end-to-end test of sharing and watching.
- Microphone and speakers (`wasapi2src`, `wasapi2sink`, both with
  `low-latency=true`): a call between two copies of the CLI with a real
  microphone had the same 90 ms of delay as with the test tone. Nobody has
  listened to it.
- The device list: the real devices under their own names, with the one
  that is the system default marked. Windows also lists "the default" as a
  device of its own and the speakers as something to record from; both are
  left out (`listed` in `audio.rs`).
- Screen capture (`d3d11screencapturesrc`): a 2560x1440 monitor reached a
  viewer at 60 fps with nothing dropped. The CLI takes `--share-monitor
  NUMBER`; the app always shares the main monitor, until there is a picker.
- Stream sound (`sound/loopback.rs`): a tone played by another program
  reached the viewer. That our own playback is left out has not been
  checked by ear.
- Push-to-talk outside the window: the key chosen with "Set key" is
  followed everywhere, by asking Windows every 10 ms whether it is down
  (`talk.rs`). Not tried on a real key yet.

Found on the way:

- The OpenGL elements exist on Windows and pass `gpu_conversion_works`,
  but when the size asked of them changes in the middle of a stream, no
  more frames come out. Windows therefore prepares frames with
  `d3d11upload ! d3d11convert`, and `tests/sharing.rs` now changes the
  size while watching.
- `d3d11screencapturesrc` delivers frames at a steady rate that it has to
  be told, so a `capsfilter` named `capture_rate` follows it.
- One snapshot differs from the Linux image in four pixels of one line of
  text, so the snapshot tests allow 16 differing pixels on Windows.
- `gst-plugin-rsrtp` is not needed: nothing was installed for it.

Still to do:

- A picker for which monitor or window to share. `ScreenSource` can
  already name either; the picker is new UI and wants a proposal.
- Capturing a window (`ScreenSource::Window`) has not been run at all.
- Push-to-talk on a mouse button, which is what many would bind, is not
  possible on either system: "Set key" only takes keyboard keys.
- Encoding a 1440p screen takes 13 to 18 ms, most of it bringing each frame
  into ordinary memory and back to the card. `nvd3d11h264enc` fed straight
  from the capture would avoid that (step 6 below).
- Two copies of flowerchat on one machine record each other's side of the
  call as stream sound. Windows leaves out one program, not two.
- The installer.

## Why Windows comes next

The app works on Linux and the server is live, but everyone who would use
it besides Martin runs Windows. Until there is a Windows client, nobody
else can join.

## What is already decided

- Standard WebRTC in a full mesh, rooms of at most 8 (usually 2 to 4).
  Linux and Windows; no Mac.
- Rust on GStreamer, using `webrtcbin` directly. `webrtcsink` encodes once
  per viewer; here one encode fans out to everyone who watches.
- Everyone has an NVIDIA card, so video is NVENC and NVDEC only: H.264 and
  HEVC. AV1 is left out because the 2070 SUPER cannot encode it; `Codec` in
  `crates/client/src/video.rs` is a table, so adding it is one more entry
  plus one more video line in the SDP.
- The window is egui 0.36 through eframe (pinned by `egui-phosphor` 0.14).
  Martin wants desktop density, live figures shown rather than hidden, and
  to be shown a proposal before a change of look or layout is built.
- Stream sound means "everything the machine plays except the call itself",
  as in Discord.

## What should work unchanged

- `crates/protocol`, `crates/server`: plain Rust, no media.
- Signaling in the client, including `wss://`: `tokio-tungstenite` uses
  `native-tls`, which is SChannel on Windows.
- The whole WebRTC part of `crates/client/src/call.rs`: `webrtcbin`, Opus,
  retransmission, the per-peer gates, the data channel.
- NVENC and NVDEC (`nvh264enc`, `nvh265enc`, `nvh264dec`, `nvh265dec`,
  `cudaupload`), which are in the same `nvcodec` plugin on Windows. Check
  with `gst-inspect-1.0 nvcodec` that all five are there.
- The window itself, and push-to-talk on a key while the window has focus.
- `--share-test WIDTHxHEIGHT` on the app and the CLI shares a test picture
  with no capture at all, and `--test-tone --measure` runs a call with no
  audio devices. Between them they exercise everything in this list, so
  they are the first thing to get running.

## What is Linux-only and has to be replaced

| What | Where | Linux | Windows starting point |
| --- | --- | --- | --- |
| Microphone and speakers | `crates/client/src/audio.rs`, `call.rs` (the pipeline description near the top of `Call::new`) | `pulsesrc`, `pulsesink` with 20 ms buffers | `wasapi2src`, `wasapi2sink` with `low-latency=true` |
| Device list | `DeviceWatch` in `audio.rs` | `gst::DeviceMonitor`, devices named by PipeWire's `node.name` | The monitor is cross-platform, but the property that names a device is not; for WASAPI it is the device id that the element's `device` property takes |
| Screen capture | `Encoder::start` in `video.rs` (`ShareSource::Screen`) | `pipewiresrc` on a node the portal hands out | `d3d11screencapturesrc` (or the `d3d12` one), with `monitor-handle` or `window-handle` |
| Picking what to share | `crates/app/src/screen.rs` | The desktop's own picker through the ScreenCast portal (`ashpd`) | A picker of our own: Windows gives a list of monitors and windows and nothing more. This is new UI, so it wants a proposal first |
| Stream sound | `crates/client/src/sound.rs` | A recording stream that `pw-link` connects to every other program's output, found with `pw-dump --monitor` | `wasapi2src loopback=true` with `loopback-mode=exclude-process-tree` and `loopback-target-pid` set to our own process. Needs a recent Windows 10 build or Windows 11 |
| Push-to-talk while the window is in the background | `crates/app/src/talk.rs` | GlobalShortcuts portal | A global hotkey or low-level keyboard hook. It must report the release as well as the press |
| Preparing frames for display | `Decoder::start` in `video.rs` | `glupload ! glcolorconvert ! glcolorscale ! gldownload` | `d3d11upload ! d3d11convert ! d3d11download`, or leave it: see below |

The Linux halves of `screen.rs` and `talk.rs` are behind
`cfg(target_os = "linux")`, with stubs for everything else that return
"not supported yet". `ashpd` is a Linux-only dependency already.

Things to know before starting on each:

- **It will not compile as it is.** `video.rs` uses `std::os::fd::OwnedFd`
  in `ScreenSource` (the portal's PipeWire connection) and in `Encoder`.
  `ScreenSource` has to become something that can also name a Windows
  monitor or window. `sound.rs` is PipeWire throughout and needs a Windows
  twin behind `cfg`.
- **Frame conversion may need nothing.** `gpu_conversion_works()` runs the
  GL chain once at start-up and falls back to `videoconvertscale` on the
  processor if it fails. So watching should work on Windows either way;
  D3D11 is about cost (on Linux the GL path took a third of the processor
  time of the CPU path).
- **Audio processing.** Echo cancellation, noise suppression and automatic
  gain are `webrtcdsp` with a `webrtcechoprobe` on the playback side. Check
  that the Windows GStreamer build ships them. `webrtcdsp` insists on the
  exact audio format in `RAW_AUDIO` (`audio.rs`).
- **Clocks.** The speakers' element provides the pipeline clock and is
  reopened in place when the device changes; the microphone never provides
  it (`provide-clock=false`). Sinks have `processing-deadline` lowered from
  the 20 ms default, because every millisecond of it is playback delay.
- **The encoder input.** `cudaupload` sits ahead of the encoder on purpose
  (see `Codec::encoder_description`). On Windows the capture source
  produces D3D11 textures; `nvd3d11h264enc` and `nvd3d11h265enc` take those
  without a round trip through system memory, and would be the efficient
  path. Whichever is used, the stage must keep the element named `encoder`
  and the properties set on it in `EncoderState::configure`.
- **Stream sound must leave our own playback out**, or everyone hears
  themselves back. On Linux that is done by process id and by binary name.
- **The push-to-talk identity.** The Linux side registers as
  `se.flowerboi.flowerchat`. That name was picked by Claude and not
  confirmed by Martin.

## Building on Windows

What was installed, all with `winget`:

```powershell
winget install Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools --override "--passive --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install gstreamerproject.gstreamer
```

GStreamer 1.28 comes as one installer for runtime and development files,
and its default selection had every element used here. It goes into
`%LOCALAPPDATA%\Programs\gstreamer\1.0\msvc_x86_64` and sets no variables.
Building needs its `pkg-config`, and running needs its DLLs, so in the
shell that runs `cargo`:

```powershell
$gst = "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64"
$env:PATH = "$gst\bin;$env:PATH"
$env:PKG_CONFIG_PATH = "$gst\lib\pkgconfig"
```

Still open:

- An installer that brings GStreamer along, since friends will not install
  it themselves. Undecided how.
- The app's default server is the live one, `wss://chat.flowerboi.se/ws`.
  The CLI still defaults to `ws://127.0.0.1:8080/ws`, so that test runs do
  not land in the real room; pass `--server` to either to override.

## How the engine is put together

`crates/client/src/call.rs` starts with a diagram. In short:

- One pipeline per call holds the voice path and every peer's `webrtcbin`.
- The video encoder and each decoder run in pipelines of their own, joined
  to the call by `appsrc` and `appsink`. That keeps voice at its delay
  (85 ms end to end at the default 40 ms receive buffer) whatever the video
  does.
- One encode goes through a `tee`. A gate per peer opens at a keyframe when
  that peer sends `{"watch":true}` on the data channel. The sharer
  broadcasts `{"sharing":{…}}` once a second.
- Every connection negotiates one video line per codec from the start
  (payload type 96 for H.264, 98 for HEVC, retransmission on 97 and 99), so
  changing codec mid-share needs no renegotiation. The encoder swaps its
  encoding stage between two frames; the viewer swaps its decoder when a
  keyframe arrives in the other codec.
- Stream sound is a second Opus stream (payload type 112, stereo), gated
  like the video and played through the voice mix.

## GStreamer facts that cost time to find

All of these were found on Linux. Most are about `webrtcbin` and will hold
on Windows too.

- `webrtcbin`'s statistics say `kind: "audio"` for incoming video. Tell the
  streams apart by the codec's mime type.
- The top-level `packets-lost` misses most losses; use
  `gst-rtpsource-stats`. Its `bitrate` is a slow average, so video bitrates
  are computed from `bytes-received` and `bytes-sent` between reports.
- The video SSRC has to be fixed in the pad's caps, or the SDP lacks it.
- `webrtcbin` sets retransmission for every stream of a connection by what
  its last transceiver wants. `tune_feedback` in `call.rs` therefore sets
  `do-retransmission` per jitter buffer, by the first packet's payload
  type. Adding a stream after the video once turned video retransmission
  off without a word.
- Feedback is rate-limited by the bandwidth the RTP session assumes, so
  `rtpsession bandwidth` is set to 4 Mbit/s. Without it, requests for lost
  packets come too late for seconds after a stream starts.
- `rtpjitterbuffer` with retransmission guesses when the next packet is
  due. When a stream pauses (nobody watching, or the codec changed) it
  declares the next packet lost and then drops the real one as late. For
  HEVC that packet carries the parameter sets, so a fresh decoder never
  started. Fixed with `rtx-next-seqnum=false`.
- `webrtcbin` matches an answerer's transceivers to the offer's lines by
  caps, so the per-codec pads must carry the offer's payload types.
- `opusdec use-inband-fec=true` reports 120 ms of latency, so it is off and
  loss concealment does the work.
- A description passed to `parse::bin_from_description` must not end in
  bare caps; use `capsfilter caps=`.
- `nvh264dec` rounds odd sizes up, so sizes are kept even (`fit` in
  `video.rs`). H.264 is limited to 4096 pixels a side.
- `audiotestsrc wave=silence` marks its buffers as gaps, which
  `volume mute` skips.
- `nicesrc` pushes buffer lists, not single buffers.
- At a set bitrate, a picture that is all noise overshoots: 62 Mbit/s
  arrived at a setting of 50.
- A source that hands out pooled frames can be a trap when another thread
  gives them back. `pipewiresrc` deadlocked that way (commit `55c0ded`).
  If a share on Windows ever freezes with the buttons dead, take the stacks
  of every thread of the stuck process before anything else.

## Testing

- `cargo test --workspace` needs an NVIDIA card but no screen, window or
  audio device. `crates/client/tests/sharing.rs` is the broad one: share,
  watch, change codec and bitrate, a viewer that loses packets, sound.
- `cargo test -p flowerchat-client --test stress -- --ignored --nocapture`
  pushes a share at 50 Mbit/s through changes of bitrate and codec.
- The app has snapshot tests (`crates/app/tests/snapshots`) and end-to-end
  tests in `crates/app/src/e2e.rs` that drive the real window without
  showing it. The audio one creates virtual devices with `pactl` and is
  Linux-only. `screens_are_shared_and_watched_through_the_interface` fails
  about one run in eight at "frames arrive"; that was there before the
  port and is not understood.
- The CLI (`flowerchat-cli`) prints once a second with `--stats`;
  `--watch` watches every share without showing it; `--lose N` drops that
  percentage of arriving packets.
- Rendering tests and the app need a GPU backend for wgpu; on Windows that
  is D3D12 or Vulkan, untested.

## The server

Live at `wss://chat.flowerboi.se/ws`, on Martin's NAS behind nginx, from
the image that `.github/workflows/server-image.yml` publishes
(`deploy/dockge` has the stack). It only does signaling; media goes
directly between clients. It has no access control yet: Martin's plan is
Google sign-in with a list of allowed accounts. There is no TURN server, so
two homes whose routers will not let them connect directly cannot talk;
that is put off until it happens.

## What nobody has verified yet

- Two clients in two different homes. Every test so far had both ends on
  one machine.
- How it sounds: echo cancellation, noise suppression, switching devices,
  stream sound with a real game, lip sync.
- Push-to-talk on a real bound key, including the release.
- Picture quality by eye at a given bitrate, and what a codec change looks
  like to a viewer.
- The pop-out window, and a share of a still screen.
- The fix for the frozen share on a real screen. It was proven against a
  fake 170 Hz screen (see the commit message of `55c0ded`).
- A decoder that cannot keep up lets its input queue grow; there is no
  guard. There is no bitrate adaptation either.

## A possible order of work

1. Build the workspace on Windows and get `flowerchat-cli --test-tone
   --measure --share-test 1920x1080` talking to a second copy with
   `--watch`. That proves GStreamer, WebRTC and NVENC without touching any
   of the replacements.
2. Microphone, speakers and the device list. Then the app is a working
   voice client.
3. Screen capture with a fixed monitor, then the picker.
4. Stream sound.
5. Global push-to-talk.
6. D3D11 for encoder input and frame conversion, if the figures call for it.
7. The installer.
