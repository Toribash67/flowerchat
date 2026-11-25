// flowerchat in the browser: voice and text chat with the people in a room,
// whether they use the app or a browser. Screens are not shared or watched
// here; the page says who shares one, and where to get the app for it.
//
// It speaks the same protocol as the app (see the README): the server only
// introduces peers, and each pair of peers has one RTCPeerConnection with
// the voice and a data channel named "control" for mute state and chat.
'use strict';

const $ = (id) => document.getElementById(id);

/** Quieter than this, in dBFS, is not speaking. The app's threshold. */
const SPEAKING_ABOVE_DB = -40;
/** How long "Speaking" stays after the last loud moment. */
const SPEAKING_HOLD_MS = 300;
/** The bottom of the level meters. */
const METER_FLOOR_DB = -60;
/** The server's limit for names and rooms, which it counts in bytes. */
const MAX_NAME_BYTES = 64;
const MAX_CHAT_CHARS = 2000;
const MAX_TIMELINE = 500;

// What is remembered between visits. Storage may be unavailable (private
// windows, blocked site data), in which case nothing is remembered.
const settings = Object.assign(
  { name: '', room: 'lounge', mic: '', speaker: '', echo: true, noise: true, gain: true, volumes: {} },
  readSettings(),
);

function readSettings() {
  try {
    return JSON.parse(localStorage.getItem('flowerchat') || '{}');
  } catch {
    return {};
  }
}

function saveSettings() {
  try {
    localStorage.setItem('flowerchat', JSON.stringify(settings));
  } catch {
    // Not remembered, then.
  }
}

// ---------------------------------------------------------------------------
// Sound: the microphone, the levels, and where sound is played.

let audioContext = null;

// A page that joined by itself, from ?autojoin, starts its sound on the
// first click instead.
document.addEventListener('pointerdown', () => {
  if (audioContext && audioContext.state === 'suspended') audioContext.resume().catch(() => {});
});

/** Made on the first click, as browsers only start sound after one. */
function audio() {
  if (!audioContext) audioContext = new AudioContext();
  if (audioContext.state === 'suspended') audioContext.resume().catch(() => {});
  return audioContext;
}

/** Measures how loud a stream is, without playing it. */
function levelOf(stream) {
  const analyser = audio().createAnalyser();
  analyser.fftSize = 1024;
  const source = audio().createMediaStreamSource(stream);
  source.connect(analyser);
  const samples = new Float32Array(analyser.fftSize);
  return {
    db() {
      analyser.getFloatTimeDomainData(samples);
      let sum = 0;
      for (const s of samples) sum += s * s;
      const rms = Math.sqrt(sum / samples.length);
      return rms > 0 ? 20 * Math.log10(rms) : -Infinity;
    },
    stop() {
      source.disconnect();
    },
  };
}

function meterWidth(db) {
  const level = (db - METER_FLOOR_DB) / -METER_FLOOR_DB;
  return `${Math.round(Math.min(1, Math.max(0, level)) * 100)}%`;
}

/**
 * What we send: the microphone's track, or silence when there is no
 * microphone. Every connection is given `mic.track`, and gets the new one
 * whenever the microphone changes.
 */
const mic = {
  stream: null,
  track: null,
  level: null,
  /** Whether `track` is a real microphone rather than silence. */
  live: false,
  /** Why there is no microphone, if there is none. */
  problem: null,
};

function micConstraints() {
  return {
    audio: {
      deviceId: settings.mic ? { exact: settings.mic } : undefined,
      echoCancellation: settings.echo,
      noiseSuppression: settings.noise,
      autoGainControl: settings.gain,
    },
  };
}

/**
 * Opens the microphone, or opens it again after a setting changed. Falls
 * back to the system default when the chosen one is gone. Returns whether
 * a microphone is open.
 */
async function openMic() {
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia(micConstraints());
  } catch (err) {
    if (settings.mic && (err.name === 'OverconstrainedError' || err.name === 'NotFoundError')) {
      try {
        stream = await navigator.mediaDevices.getUserMedia({
          audio: { ...micConstraints().audio, deviceId: undefined },
        });
      } catch (again) {
        err = again;
      }
    }
    if (!stream) {
      useMicStream(null, micProblem(err));
      return false;
    }
  }
  useMicStream(stream, null);
  listDevices();
  return true;
}

function micProblem(err) {
  if (!navigator.mediaDevices) return 'This page has to be opened over HTTPS to use a microphone.';
  switch (err && err.name) {
    case 'NotAllowedError':
      return 'The browser was not allowed to use the microphone. It can be allowed in the address bar.';
    case 'NotFoundError':
      return 'No microphone was found.';
    case 'NotReadableError':
      return 'The microphone is busy or could not be opened.';
    default:
      return `The microphone could not be opened: ${err ? err.message : 'unknown reason'}.`;
  }
}

/** Puts `stream` in use, or silence when it is null. */
function useMicStream(stream, problem) {
  const old = mic.stream;
  if (mic.level) mic.level.stop();
  if (stream) {
    mic.stream = stream;
    mic.track = stream.getAudioTracks()[0];
    mic.live = true;
    // Unplugged: carry on with whatever the system uses now.
    mic.track.onended = () => {
      if (mic.track && mic.track.readyState === 'ended') openMic();
    };
  } else {
    // Something to send all the same, so that a microphone that turns up
    // later is only a track swapped in, without negotiating again.
    const silence = audio().createMediaStreamDestination();
    mic.stream = silence.stream;
    mic.track = silence.stream.getAudioTracks()[0];
    mic.live = false;
  }
  mic.problem = problem;
  mic.level = mic.live ? levelOf(mic.stream) : null;
  mic.track.enabled = !call.muted;
  if (old && old !== mic.stream) {
    for (const track of old.getTracks()) track.stop();
  }
  for (const peer of call.peers.values()) {
    if (peer.sender) peer.sender.replaceTrack(mic.track).catch(() => {});
  }
  renderMic();
}

function closeMic() {
  if (mic.level) mic.level.stop();
  if (mic.stream) for (const track of mic.stream.getTracks()) track.stop();
  Object.assign(mic, { stream: null, track: null, level: null, live: false, problem: null });
  renderMic();
}

function renderMic() {
  $('mic-try').textContent = mic.live ? 'Stop trying' : 'Try the microphone';
  $('mic-try').hidden = call.joined;
  $('mic-note').hidden = !mic.problem;
  $('mic-note').textContent = mic.problem || '';
  if (call.joined) renderMe();
}

/** Lists the microphones and speakers in the audio panel. */
async function listDevices() {
  if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return;
  let devices;
  try {
    devices = await navigator.mediaDevices.enumerateDevices();
  } catch {
    return;
  }
  fillDevices($('mic'), devices.filter((d) => d.kind === 'audioinput'), settings.mic);
  const outputs = devices.filter((d) => d.kind === 'audiooutput');
  // Choosing where sound goes takes setSinkId, and the browser has to say
  // which outputs there are; not every browser does both.
  $('speaker').disabled = !canChooseSpeakers() || outputs.length === 0;
  fillDevices($('speaker'), canChooseSpeakers() ? outputs : [], settings.speaker);

  // A microphone that was chosen and has come back is used again.
  if (mic.live && settings.mic && mic.track.getSettings().deviceId !== settings.mic
      && devices.some((d) => d.kind === 'audioinput' && d.deviceId === settings.mic)) {
    openMic();
  }
}

function canChooseSpeakers() {
  return typeof HTMLMediaElement.prototype.setSinkId === 'function';
}

/** Fills a device list. The first entry is always the system's default. */
function fillDevices(select, devices, chosen) {
  // Chrome lists the default under the id "default", with the name of the
  // device it is at the moment.
  const fallback = devices.find((d) => d.deviceId === 'default');
  const named = devices.filter((d) => d.deviceId && d.deviceId !== 'default' && d.deviceId !== 'communications');
  const options = [['', fallback && fallback.label ? `System default (${fallback.label.replace(/^Default - /, '')})` : 'System default']];
  for (const device of named) {
    options.push([device.deviceId, device.label || 'Unnamed device']);
  }
  // Something chosen that is gone for now stays chosen, for when it returns.
  if (chosen && !options.some(([id]) => id === chosen)) {
    options.push([chosen, 'Not connected']);
  }
  select.replaceChildren(...options.map(([id, label]) => new Option(label, id)));
  select.value = chosen;
}

/** Sends every voice, and the test sound, where the speakers setting says. */
function routeSound(element) {
  if (canChooseSpeakers()) {
    element.setSinkId(settings.speaker || '').catch(() => {
      // A device that went away: back to the default.
      element.setSinkId('').catch(() => {});
    });
  }
}

/** Two short notes, made here rather than downloaded. */
function testSound() {
  const rate = 48000;
  const notes = [[659.25, 0.18], [987.77, 0.32]];
  const total = notes.reduce((sum, [, length]) => sum + length, 0);
  const count = Math.floor(rate * total);
  const view = new DataView(new ArrayBuffer(44 + count * 2));
  const text = (offset, s) => [...s].forEach((c, i) => view.setUint8(offset + i, c.charCodeAt(0)));
  text(0, 'RIFF'); view.setUint32(4, 36 + count * 2, true); text(8, 'WAVE');
  text(12, 'fmt '); view.setUint32(16, 16, true); view.setUint16(20, 1, true); view.setUint16(22, 1, true);
  view.setUint32(24, rate, true); view.setUint32(28, rate * 2, true); view.setUint16(32, 2, true); view.setUint16(34, 16, true);
  text(36, 'data'); view.setUint32(40, count * 2, true);
  let i = 0;
  for (const [frequency, length] of notes) {
    const n = Math.floor(rate * length);
    for (let k = 0; k < n && i < count; k++, i++) {
      const envelope = Math.min(1, k / 400) * Math.exp((-4 * k) / n);
      view.setInt16(44 + i * 2, Math.sin((2 * Math.PI * frequency * k) / rate) * envelope * 0.3 * 32767, true);
    }
  }
  return URL.createObjectURL(new Blob([view], { type: 'audio/wav' }));
}

let testSoundUrl = null;

$('speaker-test').addEventListener('click', () => {
  testSoundUrl = testSoundUrl || testSound();
  const player = new Audio(testSoundUrl);
  routeSound(player);
  player.play().catch(() => {});
});

$('mic-try').addEventListener('click', async () => {
  audio();
  if (mic.live) closeMic();
  else await openMic();
});

$('mic').addEventListener('change', () => {
  settings.mic = $('mic').value;
  saveSettings();
  if (mic.stream) openMic();
});

$('speaker').addEventListener('change', () => {
  settings.speaker = $('speaker').value;
  saveSettings();
  for (const peer of call.peers.values()) routeSound(peer.audio);
});

for (const [id, key] of [['echo', 'echo'], ['noise', 'noise'], ['gain', 'gain']]) {
  $(id).checked = settings[key];
  $(id).addEventListener('change', () => {
    settings[key] = $(id).checked;
    saveSettings();
    // Browsers differ in what they change on a running track, so the
    // microphone is opened anew with the new setting.
    if (mic.live) openMic();
  });
}

if (navigator.mediaDevices) {
  navigator.mediaDevices.addEventListener('devicechange', listDevices);
}

// ---------------------------------------------------------------------------
// The call.

const call = {
  ws: null,
  joined: false,
  myId: null,
  myName: '',
  room: '',
  iceServers: [],
  muted: false,
  deafened: false,
  joinedAt: 0,
  peers: new Map(),
  /** Why the last attempt to be in a room ended, if not by leaving. */
  ending: null,
};

function sendToServer(msg) {
  if (call.ws && call.ws.readyState === WebSocket.OPEN) call.ws.send(JSON.stringify(msg));
}

function signal(to, data) {
  sendToServer({ type: 'signal', to, data });
}

function bytes(s) {
  return new TextEncoder().encode(s).length;
}

async function join(withMic) {
  const name = $('name').value.trim();
  const room = $('room').value.trim();
  const problem = !name ? 'Enter a name.'
    : !room ? 'Enter a room.'
    : bytes(name) > MAX_NAME_BYTES ? 'That name is too long.'
    : bytes(room) > MAX_NAME_BYTES ? 'That room name is too long.'
    : null;
  showJoinError(problem);
  if (problem) return;

  settings.name = name;
  settings.room = room;
  saveSettings();
  audio();

  $('join-button').disabled = true;
  $('join-listening').hidden = true;
  if (withMic && !mic.live && !(await openMic())) {
    $('join-button').disabled = false;
    showJoinError(mic.problem);
    $('join-listening').hidden = false;
    return;
  }
  if (!withMic && !mic.stream) useMicStream(null, mic.problem);
  // Without a microphone, the others are told we are muted, which is true.
  call.muted = !mic.live;
  mic.track.enabled = !call.muted;

  call.myName = name;
  call.room = room;
  call.ending = null;
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${scheme}://${location.host}/ws`);
  call.ws = ws;
  ws.onopen = () => ws.send(JSON.stringify({ type: 'join', room, name }));
  ws.onmessage = (event) => {
    let msg;
    try {
      msg = JSON.parse(event.data);
    } catch {
      return;
    }
    onServerMessage(msg);
  };
  ws.onclose = () => {
    if (call.ws !== ws) return;
    const reason = call.ending
      || (call.joined ? 'The connection to the server was lost.' : 'The server could not be reached.');
    leave(reason);
  };
}

function showJoinError(text) {
  $('join-error').hidden = !text;
  $('join-error').textContent = text || '';
}

function onServerMessage(msg) {
  switch (msg.type) {
    case 'welcome':
      call.myId = msg.id;
      call.iceServers = msg.iceServers || [];
      enterRoom();
      for (const peer of msg.peers) addPeer(peer.id, peer.name);
      render();
      break;
    case 'peer-joined':
      log(`${msg.name} joined, connecting`);
      addPeer(msg.id, msg.name);
      render();
      break;
    case 'peer-left': {
      const peer = call.peers.get(msg.id);
      if (peer) log(`${peer.name} left`);
      removePeer(msg.id);
      render();
      break;
    }
    case 'signal':
      onSignal(msg.from, msg.data || {});
      break;
    case 'error':
      call.ending = `The server said: ${msg.message}.`;
      break;
  }
}

function enterRoom() {
  call.joined = true;
  call.joinedAt = Date.now();
  call.deafened = false;
  $('join-button').disabled = false;
  $('lobby').hidden = true;
  $('room-view').hidden = false;
  $('room-stage').append($('audio-panel'));
  $('room-name').textContent = call.room;
  $('me-name').textContent = call.myName;
  $('fact-server').textContent = location.host;
  $('fact-server').title = location.host;
  $('timeline').replaceChildren();
  unread = 0;
  setTab('people');
  // The address now leads back to this room.
  history.replaceState(null, '', `?room=${encodeURIComponent(call.room)}`);
  log(`You joined ${call.room}`);
  renderMic();
}

/** Leaves the room, if in one, and shows the lobby with `reason`, if any. */
function leave(reason) {
  for (const id of [...call.peers.keys()]) removePeer(id);
  if (call.ws) {
    const ws = call.ws;
    call.ws = null;
    ws.onclose = null;
    ws.close();
  }
  call.joined = false;
  call.muted = false;
  closeMic();
  $('room-view').hidden = true;
  $('lobby').hidden = false;
  $('lobby-audio').append($('audio-panel'));
  $('join-button').disabled = false;
  showJoinError(reason || null);
  document.title = 'flowerchat';
  renderMic();
}

// One connection per peer. Whoever has the lower id makes the offer, as the
// app does; the rest is "perfect negotiation", with the higher id giving
// way, so that a renegotiation from either side settles too.
function addPeer(id, name) {
  if (call.peers.has(id)) return;
  const pc = new RTCPeerConnection({ iceServers: call.iceServers });
  const peer = {
    id,
    name,
    pc,
    weOffer: call.myId < id,
    polite: call.myId > id,
    makingOffer: false,
    ignoreOffer: false,
    state: 'connecting',
    muted: false,
    heardStatus: false,
    sharing: null,
    level: null,
    lastLoud: 0,
    stats: {},
    previous: null,
    voiceMid: null,
  };
  call.peers.set(id, peer);

  peer.audio = new Audio();
  peer.audio.autoplay = true;
  peer.audio.muted = call.deafened;
  peer.audio.volume = volumeFor(name);
  routeSound(peer.audio);

  peer.sender = pc.addTrack(mic.track, mic.stream);

  peer.control = pc.createDataChannel('control', { negotiated: true, id: 0 });
  peer.control.onopen = () => sendTo(peer, { muted: call.muted });
  peer.control.onmessage = (event) => onControl(peer, event.data);

  pc.onnegotiationneeded = async () => {
    // The answering side waits for the first offer.
    if (!peer.weOffer && !pc.remoteDescription) return;
    try {
      peer.makingOffer = true;
      await pc.setLocalDescription();
      signal(id, { description: forGstreamer(pc.localDescription) });
    } catch (err) {
      console.warn(`offer to ${name} failed`, err);
    } finally {
      peer.makingOffer = false;
    }
  };
  pc.onicecandidate = ({ candidate }) => {
    if (candidate) signal(id, { candidate });
  };
  pc.ontrack = (event) => onTrack(peer, event);
  pc.onconnectionstatechange = () => onConnectionState(peer);

  buildRow(peer);
}

async function onSignal(from, { description, candidate }) {
  const peer = call.peers.get(from);
  if (!peer) return;
  const pc = peer.pc;
  try {
    if (description) {
      const collision = description.type === 'offer'
        && (peer.makingOffer || pc.signalingState !== 'stable');
      peer.ignoreOffer = !peer.polite && collision;
      if (peer.ignoreOffer) return;
      await pc.setRemoteDescription(description);
      if (description.type === 'offer') {
        await pc.setLocalDescription();
        signal(from, { description: forGstreamer(pc.localDescription) });
      }
    } else if (candidate) {
      try {
        await pc.addIceCandidate(candidate);
      } catch (err) {
        // Candidates of an offer we ignored are expected to fail.
        if (!peer.ignoreOffer) throw err;
      }
    }
  } catch (err) {
    console.warn(`signaling with ${peer.name} failed`, err);
  }
}

/**
 * GStreamer, which the app is built on, turns down a whole description if
 * one of its media sections lacks ICE credentials or a DTLS role. Browsers
 * leave them out of the sections they refuse, as the standard allows:
 * Firefox refuses the app's HEVC video, for one. Such sections are given
 * those of the first section, which they would share anyway. Only what is
 * sent changes; the browser keeps its own description.
 */
function forGstreamer(description) {
  const sdp = description.sdp;
  const shared = ['ice-ufrag', 'ice-pwd', 'setup']
    .map((key) => sdp.match(new RegExp(`^a=${key}:[^\\r\\n]*`, 'm')))
    .filter(Boolean)
    .map((match) => match[0]);
  const [session, ...media] = sdp.replace(/\r?\n$/, '').split(/\r?\n(?=m=)/);
  const fixed = media.map((section) => {
    const missing = shared.filter((line) => !section.includes(`\na=${line.slice(2, line.indexOf(':'))}:`));
    return [section, ...missing].join('\r\n');
  });
  return { type: description.type, sdp: `${[session, ...fixed].join('\r\n')}\r\n` };
}

/**
 * The app offers more than a voice: video, and a second audio stream for
 * the sound of a shared screen. Only the voice is played. It is the first
 * audio in the offer, and so in the answer too.
 */
function firstAudioMid(sdp) {
  for (const section of sdp.split(/\r?\n(?=m=)/)) {
    if (section.startsWith('m=audio')) {
      const mid = section.match(/^a=mid:(\S+)/m);
      return mid ? mid[1] : null;
    }
  }
  return null;
}

function onTrack(peer, event) {
  if (event.track.kind !== 'audio') return;
  const remote = peer.pc.remoteDescription;
  const voiceMid = remote ? firstAudioMid(remote.sdp) : null;
  if (voiceMid !== null && event.transceiver.mid !== voiceMid) return;
  peer.voiceMid = event.transceiver.mid;
  const stream = new MediaStream([event.track]);
  peer.audio.srcObject = stream;
  peer.audio.play().catch(() => {});
  if (peer.level) peer.level.stop();
  peer.level = levelOf(stream);
}

function onConnectionState(peer) {
  const state = peer.pc.connectionState;
  if (state === peer.state) return;
  peer.state = state;
  const what = {
    connected: 'connected',
    disconnected: 'lost connection',
    failed: 'could not be reached',
  }[state];
  if (what) log(`${peer.name} ${what}`);
  // Whoever offered tries again on other paths.
  if (state === 'failed' && peer.weOffer) peer.pc.restartIce();
  render();
}

function onControl(peer, data) {
  let msg;
  try {
    msg = JSON.parse(data);
  } catch {
    return;
  }
  if (typeof msg.muted === 'boolean') {
    // The first word on a connection is only how things are, not news.
    if (peer.heardStatus && peer.muted !== msg.muted) {
      log(`${peer.name} ${msg.muted ? 'muted' : 'unmuted'}`);
    }
    peer.heardStatus = true;
    peer.muted = msg.muted;
  }
  if (typeof msg.chat === 'string') {
    say(peer.name, msg.chat.slice(0, MAX_CHAT_CHARS));
  }
  if ('sharing' in msg) {
    const sharing = msg.sharing && typeof msg.sharing === 'object' ? msg.sharing : null;
    if (sharing && !peer.sharing) log(`${peer.name} started sharing a screen`);
    if (!sharing && peer.sharing) log(`${peer.name} stopped sharing`);
    peer.sharing = sharing;
  }
  // "watch" asks for our screen, which a browser does not share.
  render();
}

function sendTo(peer, msg) {
  if (peer.control.readyState === 'open') {
    peer.control.send(JSON.stringify(msg));
    return true;
  }
  return false;
}

function broadcast(msg) {
  let sent = 0;
  for (const peer of call.peers.values()) if (sendTo(peer, msg)) sent++;
  return sent;
}

function removePeer(id) {
  const peer = call.peers.get(id);
  if (!peer) return;
  if (peer.level) peer.level.stop();
  peer.pc.close();
  peer.audio.srcObject = null;
  peer.row.remove();
  call.peers.delete(id);
}

function volumeFor(name) {
  const volume = settings.volumes[name];
  return typeof volume === 'number' ? Math.min(1, Math.max(0, volume)) : 1;
}

// ---------------------------------------------------------------------------
// Your own controls.

function setMuted(muted) {
  if (!mic.live) muted = true;
  call.muted = muted;
  mic.track.enabled = !muted;
  broadcast({ muted });
  renderMe();
}

function setDeafened(deafened) {
  call.deafened = deafened;
  for (const peer of call.peers.values()) peer.audio.muted = deafened;
  renderMe();
}

$('mute').addEventListener('click', async () => {
  // Without a microphone, unmuting first tries to open one.
  if (!mic.live && call.muted) {
    if (!(await openMic())) {
      log(mic.problem);
      return;
    }
  }
  setMuted(!call.muted);
});
$('deafen').addEventListener('click', () => setDeafened(!call.deafened));
$('leave').addEventListener('click', () => leave());

$('invite').addEventListener('click', async () => {
  const link = `${location.origin}/?room=${encodeURIComponent(call.room)}`;
  try {
    await navigator.clipboard.writeText(link);
    $('invite').textContent = 'Link copied';
  } catch {
    window.prompt('A link to this room:', link);
  }
  setTimeout(() => { $('invite').textContent = 'Invite'; }, 2000);
});

function icon(name) {
  return `<svg viewBox="0 0 16 16" aria-hidden="true"><use href="#i-${name}"/></svg>`;
}

// ---------------------------------------------------------------------------
// The timeline: chat, and what happens in the room.

let unread = 0;

function clock() {
  return new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false });
}

function append(entry) {
  const timeline = $('timeline');
  const atBottom = timeline.scrollHeight - timeline.scrollTop - timeline.clientHeight < 40;
  timeline.append(entry);
  while (timeline.childElementCount > MAX_TIMELINE) timeline.firstElementChild.remove();
  if (atBottom) timeline.scrollTop = timeline.scrollHeight;
}

function log(text) {
  const entry = document.createElement('div');
  entry.className = 'event';
  entry.textContent = `${clock()} · ${text}`;
  append(entry);
}

function say(from, text, mine = false) {
  const entry = document.createElement('div');
  entry.className = mine ? 'message mine' : 'message';
  const head = document.createElement('div');
  const who = document.createElement('span');
  who.className = 'from';
  who.textContent = from;
  const time = document.createElement('span');
  time.className = 'time';
  time.textContent = clock();
  head.append(who, time);
  const body = document.createElement('div');
  body.className = 'text';
  body.append(...linked(text));
  entry.append(head, body);
  append(entry);
  if (!mine && (!chatShown() || document.hidden)) {
    unread++;
    renderUnread();
  }
}

/** Whether the chat can be seen: always in a wide window, on its tab otherwise. */
function chatShown() {
  return $('room-view').dataset.tab === 'chat' || window.matchMedia('(min-width: 900px)').matches;
}

/** Text with its web addresses made into links. */
function linked(text) {
  const parts = [];
  let last = 0;
  for (const match of text.matchAll(/\bhttps?:\/\/[^\s<>"]+[^\s<>".,;:!?)\]}'"]/g)) {
    if (match.index > last) parts.push(document.createTextNode(text.slice(last, match.index)));
    const link = document.createElement('a');
    link.href = match[0];
    link.textContent = match[0];
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    parts.push(link);
    last = match.index + match[0].length;
  }
  if (last < text.length) parts.push(document.createTextNode(text.slice(last)));
  return parts;
}

function renderUnread() {
  $('unread').hidden = unread === 0;
  $('unread').textContent = unread;
  document.title = unread ? `(${unread}) ${call.room} · flowerchat` : `${call.room} · flowerchat`;
}

function clearUnread() {
  if (!unread) return;
  unread = 0;
  renderUnread();
}

$('composer').addEventListener('submit', (event) => {
  event.preventDefault();
  const text = $('message').value.trim().slice(0, MAX_CHAT_CHARS);
  if (!text) return;
  const sent = broadcast({ chat: text });
  say(call.myName, text, true);
  if (sent === 0) log(call.peers.size ? 'Nobody is connected yet, so nobody got that' : 'Nobody else is here to read that');
  $('message').value = '';
});

document.addEventListener('visibilitychange', () => {
  if (!document.hidden && chatShown()) clearUnread();
});

// Tabs, in a narrow window.
function setTab(tab) {
  $('room-view').dataset.tab = tab;
  for (const button of document.querySelectorAll('.tabs button')) {
    button.setAttribute('aria-selected', String(button.dataset.tab === tab));
  }
  if (tab === 'chat') {
    clearUnread();
    $('timeline').scrollTop = $('timeline').scrollHeight;
  }
}

for (const button of document.querySelectorAll('.tabs button')) {
  button.addEventListener('click', () => setTab(button.dataset.tab));
}

// ---------------------------------------------------------------------------
// Drawing the room.

function buildRow(peer) {
  const row = document.createElement('div');
  row.className = 'peer';
  const cell = (className) => {
    const span = document.createElement('span');
    span.className = className;
    row.append(span);
    return span;
  };
  peer.cells = {
    dot: cell('dot'),
    name: cell('name'),
    status: cell('status'),
  };
  cell('break');
  Object.assign(peer.cells, {
    route: cell('detail col-route'),
    delay: cell('detail num col-delay'),
    loss: cell('detail num col-loss'),
    jitter: cell('detail num col-jitter'),
    recv: cell('detail num col-recv'),
    screen: cell('detail screen col-screen'),
  });
  const vol = cell('vol');
  const slider = document.createElement('input');
  slider.type = 'range';
  slider.min = 0;
  slider.max = 100;
  slider.value = Math.round(peer.audio.volume * 100);
  slider.setAttribute('aria-label', `Volume of ${peer.name}`);
  const output = document.createElement('output');
  output.textContent = `${slider.value}%`;
  slider.addEventListener('input', () => {
    peer.audio.volume = slider.value / 100;
    output.textContent = `${slider.value}%`;
    settings.volumes[peer.name] = peer.audio.volume;
    saveSettings();
  });
  vol.append(slider, output);
  peer.cells.name.textContent = peer.name;
  peer.cells.name.title = peer.name;
  peer.row = row;
  $('peers').append(row);
}

function speaking(peer, now) {
  return now - peer.lastLoud < SPEAKING_HOLD_MS;
}

function statusOf(peer, now) {
  switch (peer.state) {
    case 'new':
    case 'connecting':
      return ['Connecting…', 'quiet'];
    case 'disconnected':
      return ['Connection lost', 'warn'];
    case 'failed':
      return ['Unreachable', 'warn'];
    case 'closed':
      return ['Closed', 'quiet'];
  }
  if (peer.muted) return ['Muted', 'warn'];
  if (speaking(peer, now)) return ['Speaking', ''];
  return ['Idle', 'quiet'];
}

function setText(element, text) {
  if (element.textContent !== text) element.textContent = text;
}

function renderPeer(peer, now) {
  const connected = peer.state === 'connected';
  const [status, tone] = statusOf(peer, now);
  setText(peer.cells.status, status);
  peer.cells.status.className = `status ${tone}`;
  peer.row.classList.toggle('away', !connected);
  peer.cells.dot.className = `dot${connected && !peer.muted && speaking(peer, now) ? ' speaking' : ''}${peer.muted ? ' muted' : ''}${connected ? '' : ' away'}`;

  const stats = connected ? peer.stats : {};
  const number = (value, digits, unit) => (typeof value === 'number' ? `${value.toFixed(digits)}${unit}` : '–');
  setText(peer.cells.route, stats.route || '–');
  setText(peer.cells.delay, number(stats.delayMs, 0, ' ms'));
  setText(peer.cells.loss, number(stats.lossPercent, 1, '%'));
  setText(peer.cells.jitter, number(stats.jitterMs, 0, ' ms'));
  setText(peer.cells.recv, number(stats.receivingKbps, 0, ' kbps'));
  for (const key of ['route', 'delay', 'loss', 'jitter', 'recv']) {
    peer.cells[key].classList.toggle('none', peer.cells[key].textContent === '–');
  }
  setText(peer.cells.screen, peer.sharing ? 'Sharing' : '–');
  peer.cells.screen.classList.toggle('on', !!peer.sharing);
  peer.cells.screen.classList.toggle('none', !peer.sharing);
  peer.cells.screen.title = peer.sharing ? 'Screens are watched in the app' : '';
}

function renderMe() {
  $('mute').innerHTML = call.muted ? `${icon('mic-off')} Unmute` : `${icon('mic')} Mute`;
  $('mute').setAttribute('aria-pressed', String(call.muted));
  $('deafen').innerHTML = call.deafened ? `${icon('speaker-off')} Undeafen` : `${icon('speaker')} Deafen`;
  $('deafen').setAttribute('aria-pressed', String(call.deafened));
  setText($('me-mode'), mic.live ? 'Open mic' : 'No microphone');
}

function render() {
  if (!call.joined) return;
  const now = performance.now();
  const peers = [...call.peers.values()];
  const connected = peers.filter((peer) => peer.state === 'connected').length;
  for (const peer of peers) renderPeer(peer, now);
  $('alone').hidden = peers.length > 0;
  $('peer-grid').hidden = peers.length === 0;
  const people = peers.length + 1;
  const summary = peers.length
    ? `${people} in the room, ${connected} of ${peers.length} connected`
    : 'Only you';
  setText($('people-summary'), summary);
  setText($('room-summary'), summary);
  setText($('fact-people'), String(people));
  setText($('fact-status'), call.ws && call.ws.readyState === WebSocket.OPEN ? 'Connected' : 'Connecting…');
  const seconds = Math.floor((Date.now() - call.joinedAt) / 1000);
  const pad = (n) => String(n).padStart(2, '0');
  setText($('fact-time'), `${pad(Math.floor(seconds / 3600))}:${pad(Math.floor(seconds / 60) % 60)}:${pad(seconds % 60)}`);
  const sending = peers.map((peer) => peer.stats.sendingKbps).filter((kbps) => typeof kbps === 'number');
  setText($('me-sending'), sending.length && !call.muted ? `${Math.round(Math.max(...sending))} kbps` : '–');
  renderMe();
  if (!unread) document.title = `${call.room} · flowerchat`;
}

// Levels: the meters and who is speaking, a few times a second.
setInterval(() => {
  const db = mic.level ? mic.level.db() : -Infinity;
  const width = meterWidth(db);
  $('mic-meter').style.width = width;
  $('me-meter').style.width = call.muted ? '0%' : width;
  if (!call.joined) return;
  const now = performance.now();
  for (const peer of call.peers.values()) {
    if (peer.level && peer.level.db() > SPEAKING_ABOVE_DB) peer.lastLoud = now;
    renderPeer(peer, now);
  }
}, 80);

// The rest, once a second.
setInterval(() => {
  if (!call.joined) return;
  for (const peer of call.peers.values()) measure(peer);
  render();
}, 1000);

/** Route, delay, loss, jitter and bitrates of a connection, as the app shows them. */
async function measure(peer) {
  let report;
  try {
    report = await peer.pc.getStats();
  } catch {
    return;
  }
  const now = performance.now();
  let inbound = null;
  let outbound = null;
  let pair = null;
  let selectedPairId = null;
  let remoteInbound = null;
  for (const stat of report.values()) {
    if (stat.type === 'inbound-rtp' && stat.kind === 'audio'
        && (!peer.voiceMid || !stat.mid || stat.mid === peer.voiceMid) && !inbound) inbound = stat;
    if (stat.type === 'outbound-rtp' && stat.kind === 'audio' && !outbound) outbound = stat;
    if (stat.type === 'remote-inbound-rtp' && stat.kind === 'audio') remoteInbound = stat;
    if (stat.type === 'transport' && stat.selectedCandidatePairId) selectedPairId = stat.selectedCandidatePairId;
    if (stat.type === 'candidate-pair' && stat.selected) pair = stat;
  }
  if (selectedPairId) pair = report.get(selectedPairId) || pair;
  if (!pair) {
    for (const stat of report.values()) {
      if (stat.type === 'candidate-pair' && stat.nominated && stat.state === 'succeeded') pair = stat;
    }
  }

  const stats = {};
  if (pair) {
    const local = report.get(pair.localCandidateId);
    const remote = report.get(pair.remoteCandidateId);
    const relayed = [local, remote].some((candidate) => candidate && candidate.candidateType === 'relay');
    stats.route = relayed ? 'Relayed' : 'Direct';
    if (typeof pair.currentRoundTripTime === 'number') stats.delayMs = pair.currentRoundTripTime * 1000;
  }
  if (stats.delayMs === undefined && remoteInbound && typeof remoteInbound.roundTripTime === 'number') {
    stats.delayMs = remoteInbound.roundTripTime * 1000;
  }
  const previous = peer.previous;
  const seconds = previous ? (now - previous.at) / 1000 : 0;
  if (inbound) {
    if (typeof inbound.jitter === 'number') stats.jitterMs = inbound.jitter * 1000;
    if (previous && previous.inbound && seconds > 0) {
      const received = inbound.packetsReceived - previous.inbound.packetsReceived;
      const lost = Math.max(0, inbound.packetsLost - previous.inbound.packetsLost);
      if (received + lost > 0) stats.lossPercent = (100 * lost) / (received + lost);
      stats.receivingKbps = ((inbound.bytesReceived - previous.inbound.bytesReceived) * 8) / 1000 / seconds;
    }
  }
  if (outbound && previous && previous.outbound && seconds > 0) {
    stats.sendingKbps = ((outbound.bytesSent - previous.outbound.bytesSent) * 8) / 1000 / seconds;
  }
  peer.previous = { at: now, inbound, outbound };
  peer.stats = stats;
}

// ---------------------------------------------------------------------------
// Downloads.

const PLATFORMS = { windows: 'Windows', linux: 'Linux', macos: 'macOS' };

function myPlatform() {
  const agent = navigator.userAgent;
  if (/Windows/.test(agent)) return 'windows';
  if (/Mac OS X|Macintosh/.test(agent) && !/Mobile/.test(agent)) return 'macos';
  if (/Linux|X11/.test(agent) && !/Android/.test(agent)) return 'linux';
  return null;
}

function megabytes(size) {
  return `${Math.round(size / 1e6)} MB`;
}

async function showDownloads() {
  const note = $('release-note');
  let body;
  try {
    const response = await fetch('/api/release');
    body = await response.json();
    if (!response.ok) throw new Error(body.error || response.statusText);
  } catch {
    note.textContent = 'The downloads could not be looked up just now. Try again in a while.';
    return;
  }
  const release = body.release;
  if (!release || release.assets.length === 0) {
    note.textContent = 'No installer has been published yet. Until then, the browser works.';
    return;
  }

  const mine = myPlatform();
  const assets = [...release.assets].sort((a, b) => (b.platform === mine) - (a.platform === mine));
  const labelOf = (asset) => {
    const kind = asset.name.match(/\.(AppImage|deb|rpm|flatpak|msi|dmg)$/i);
    return `${PLATFORMS[asset.platform] || asset.platform}${kind ? ` (.${kind[1]})` : ''}`;
  };
  const labels = assets.map(labelOf);
  // Two files that would look the same, such as one for each kind of Mac,
  // go by their names instead.
  const shown = labels.map((label, i) => (labels.indexOf(label) !== labels.lastIndexOf(label) ? assets[i].name : label));
  const primary = assets.find((asset) => asset.platform === mine) || (mine ? null : assets[0]);
  $('downloads').replaceChildren(...assets.map((asset, i) => {
    const link = document.createElement('a');
    link.className = `button${asset === primary ? ' primary' : ''}`;
    link.href = asset.download;
    link.innerHTML = icon('download');
    link.append(` ${shown[i]}`);
    link.title = `${asset.name}, ${megabytes(asset.size)}`;
    return link;
  }));

  const published = release.published ? new Date(release.published) : null;
  const when = published && !Number.isNaN(published.getTime())
    ? `, published ${published.toLocaleDateString([], { day: 'numeric', month: 'long', year: 'numeric' })}`
    : '';
  const missing = mine && !release.assets.some((asset) => asset.platform === mine)
    ? ` There is nothing for ${PLATFORMS[mine]} yet; the browser works there.`
    : '';
  note.textContent = `${release.name}${when}.${missing}`;

  // In a room, a pointer to the app for what the browser cannot do.
  const ours = release.assets.find((asset) => asset.platform === mine);
  if (ours) {
    const hint = $('room-get-app');
    hint.textContent = 'Screens are shared and watched in the app. ';
    const link = document.createElement('a');
    link.href = ours.download;
    link.textContent = `Get it for ${PLATFORMS[ours.platform]}`;
    hint.append(link);
    hint.hidden = false;
  }
}

// ---------------------------------------------------------------------------
// Starting up.

$('join').addEventListener('submit', (event) => {
  event.preventDefault();
  join(true);
});
$('join-listening').addEventListener('click', () => join(false));

// ?name=…&room=… fills in the form, and &autojoin joins straight away,
// which scripted tests use. An invite link carries only the room.
const params = new URLSearchParams(location.search);
$('name').value = params.get('name') || settings.name;
$('room').value = params.get('room') || settings.room || 'lounge';
if (!$('name').value) $('name').focus();

if (!window.RTCPeerConnection || !navigator.mediaDevices) {
  showJoinError(window.isSecureContext
    ? 'This browser cannot make calls.'
    : 'Calls need this page opened over HTTPS.');
  $('join-button').disabled = true;
}

renderMic();
listDevices();
showDownloads();
if (params.has('autojoin') && !$('join-button').disabled) join(true);
