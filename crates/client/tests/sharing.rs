//! Shares a test picture from one client and watches it from another, with
//! both timing each other's clicks as in `selftest.rs`.
//!
//! Needs an NVIDIA GPU for encoding and decoding, but no screen, window or
//! audio device. `software.rs` does without the GPU.

mod common;

use std::time::Duration;

use common::{join, serve};
use flowerchat_client::session::{Codec, Command, ShareSettings, ShareSource};

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_screen_reaches_whoever_watches() {
    gstreamer::init().unwrap();
    let url = serve().await;

    let sharer = join(&url, "sharer", 0.0, false);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let viewer = join(&url, "viewer", 0.0, false);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let voice_before = viewer.median_delay().await;

    // Sharing alone sends nothing: the stream is for those who ask.
    let full = ShareSettings {
        max_height: None,
        fps: 60,
        sound: true,
        codec: Codec::H264,
        kbps: None,
    };
    sharer.send(Command::StartShare {
        source: ShareSource::TestPicture {
            width: 1280,
            height: 720,
        },
        settings: full,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let announced = viewer.seen.lock().unwrap().peer_share.clone();
    let announced = announced.expect("the viewer hears of the share");
    assert_eq!((announced.width, announced.height), (1280, 720));
    assert_eq!((announced.fps, announced.kbps), (60, 3900));
    assert_eq!(announced.codec, Codec::H264);
    assert!(announced.watchers.is_empty() && announced.sound);
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "nobody asked to watch");
    assert_eq!(viewer.sound_over(1).await, None, "nor to listen");

    viewer.send(Command::Watch {
        id: sharer.id(),
        watch: true,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (rate, size) = viewer.frames_over(3).await;
    assert_eq!(size, Some((1280, 720)));
    assert!((55.0..65.0).contains(&rate), "60 fps arrive as {rate:.1}");
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert_eq!(own.watchers, vec![viewer.id()]);
    assert!((55.0..65.0).contains(&own.sent_fps));
    // The graphics card does the work at both ends.
    assert_eq!((own.encoder.as_str(), own.software), ("NVENC", false));
    let watching = viewer.seen.lock().unwrap().watching.clone().unwrap();
    assert_eq!((watching.decoder, watching.software), ("NVDEC", false));
    // The share's sound, a test tone, comes with the picture.
    let sound = viewer.sound_over(2).await.expect("the share has sound");
    assert!(
        (-30.0..-15.0).contains(&sound),
        "the tone arrives at {sound:.0} dB"
    );

    // A viewer with less room for the picture gets its frames smaller, and
    // at full size again when it asks.
    viewer.send(Command::SetViewHeight {
        id: sharer.id(),
        max_height: 360,
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (rate, size) = viewer.frames_over(2).await;
    assert_eq!(size, Some((640, 360)));
    assert!(rate > 50.0, "scaled down, {rate:.1} fps");
    viewer.send(Command::SetViewHeight {
        id: sharer.id(),
        max_height: 0,
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(viewer.frames_over(1).await.1, Some((1280, 720)));

    // The voice is as quick as it was, in both directions.
    let voice_watching = viewer.median_delay().await;
    let voice_sharing = sharer.median_delay().await;
    for (what, delay) in [("watching", voice_watching), ("sharing", voice_sharing)] {
        assert!(
            (delay - voice_before).abs() < 15.0,
            "voice took {voice_before:.1} ms before and {delay:.1} ms while {what}"
        );
    }

    // The sharer changes codec and sets a bitrate. The viewer does nothing
    // and keeps getting every frame.
    let rate_before = viewer.video_kbps_over(3).await;
    assert!(
        (3000.0..5000.0).contains(&rate_before),
        "3.9 Mbit/s arrive as {rate_before:.0} kbit/s"
    );
    let hevc = ShareSettings {
        codec: Codec::Hevc,
        ..full
    };
    sharer.send(Command::SetShareSettings(hevc));
    let (rate, size) = viewer.frames_over(3).await;
    assert_eq!(size, Some((1280, 720)));
    assert!(rate > 50.0, "across the change, {rate:.1} fps");
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.codec, announced.kbps), (Codec::Hevc, 2900));
    sharer.send(Command::SetShareSettings(ShareSettings {
        kbps: Some(1000),
        ..hevc
    }));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (frames, rate_set) = tokio::join!(viewer.frames_over(3), viewer.video_kbps_over(3));
    assert!(
        (55.0..65.0).contains(&frames.0),
        "in HEVC, {:.1} fps",
        frames.0
    );
    assert!(
        (700.0..1300.0).contains(&rate_set),
        "1 Mbit/s arrives as {rate_set:.0} kbit/s"
    );
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.codec, announced.kbps), (Codec::Hevc, 1000));
    // Looking away and back starts a new decoder, which needs the stream
    // to pick up with everything a decoder starts from.
    for watch in [false, true] {
        viewer.send(Command::Watch {
            id: sharer.id(),
            watch,
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let (rate, _) = viewer.frames_over(2).await;
    assert!(
        (55.0..65.0).contains(&rate),
        "watching HEVC again, {rate:.1} fps"
    );
    sharer.send(Command::SetShareSettings(full));
    let (rate, _) = viewer.frames_over(3).await;
    assert!(rate > 50.0, "across the change back, {rate:.1} fps");
    let (frames, rate_back) = tokio::join!(viewer.frames_over(3), viewer.video_kbps_over(3));
    assert!(
        (55.0..65.0).contains(&frames.0),
        "back in H.264, {:.1} fps",
        frames.0
    );
    assert!(
        (3000.0..5000.0).contains(&rate_back),
        "3.9 Mbit/s arrive again as {rate_back:.0} kbit/s"
    );
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.codec, announced.kbps), (Codec::H264, 3900));

    // Someone on a poor connection joins and watches too. The packets that
    // go missing are sent again in time, so no frames are lost, and the
    // first viewer notices nothing.
    let lossy = join(&url, "lossy", 0.02, false);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(lossy.seen.lock().unwrap().peer_share.is_some());
    lossy.send(Command::Watch {
        id: sharer.id(),
        watch: true,
    });
    let (lossy_frames, steady_frames) = tokio::join!(lossy.frames_over(4), viewer.frames_over(4));
    assert!(
        lossy_frames.0 > 50.0,
        "with loss, {:.1} fps",
        lossy_frames.0
    );
    assert!(
        steady_frames.0 > 55.0,
        "beside it, {:.1} fps",
        steady_frames.0
    );
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert_eq!(own.watchers, vec![viewer.id(), lossy.id()]);
    lossy.send(Command::Leave);
    lossy.session.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert_eq!(own.watchers, vec![viewer.id()]);

    // A smaller, slower picture takes over without the viewer doing a thing.
    // The sound is turned off with it, and back on afterwards.
    let quiet = ShareSettings {
        max_height: Some(360),
        fps: 30,
        sound: false,
        ..full
    };
    sharer.send(Command::SetShareSettings(quiet));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (frames, sound) = tokio::join!(viewer.frames_over(3), viewer.sound_over(3));
    let (rate, size) = frames;
    assert_eq!(size, Some((640, 360)));
    assert!((27.0..33.0).contains(&rate), "30 fps arrive as {rate:.1}");
    assert_eq!(sound, None, "the picture goes on without sound");
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.width, announced.height), (640, 360));
    assert_eq!((announced.fps, announced.sound), (30, false));
    sharer.send(Command::SetShareSettings(ShareSettings {
        sound: true,
        ..quiet
    }));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(viewer.sound_over(2).await.is_some(), "the sound is back");

    // Looking away stops the stream; looking again restarts it.
    viewer.send(Command::Watch {
        id: sharer.id(),
        watch: false,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "no longer watching");
    assert_eq!(viewer.sound_over(1).await, None, "nor listening");
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert!(own.watchers.is_empty());
    viewer.send(Command::Watch {
        id: sharer.id(),
        watch: true,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (rate, size) = viewer.frames_over(2).await;
    assert_eq!(size, Some((640, 360)));
    assert!((27.0..33.0).contains(&rate), "30 fps arrive as {rate:.1}");

    sharer.send(Command::StopShare);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(viewer.seen.lock().unwrap().peer_share, None);
    assert_eq!(sharer.seen.lock().unwrap().own_share, None);
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "the share is over");

    for client in [sharer, viewer] {
        assert_eq!(client.seen.lock().unwrap().problems, Vec::<String>::new());
        client.send(Command::Leave);
        client.session.await.unwrap().unwrap();
    }
}
