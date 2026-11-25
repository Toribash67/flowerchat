//! Shares a test picture with the graphics card left out of it: the
//! processor encodes it, in every codec it has an encoder for, and decodes
//! it again for one of two viewers. The other viewer uses its graphics
//! card, if the machine has one that can.
//!
//! Needs no GPU, screen, window or audio device.

mod common;

use std::time::Duration;

use common::{join, serve};
use flowerchat_client::session::{Codec, Command, ShareCodec, ShareSettings, ShareSource};

/// Enough for any of the encoders to fit the test picture into, which
/// comes with a patch of noise that cannot be compressed.
const KBPS: u32 = 12_000;

#[tokio::test(flavor = "multi_thread")]
async fn the_processor_shares_and_shows_a_screen() {
    gstreamer::init().unwrap();
    let url = serve().await;

    let sharer = join(&url, "sharer", 0.0, true);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let plain = join(&url, "plain", 0.0, true);
    let card = join(&url, "card", 0.0, false);
    tokio::time::sleep(Duration::from_secs(4)).await;

    let codecs = Codec::for_sharing(true);
    if codecs.is_empty() {
        eprintln!("nothing here encodes on the processor");
    }
    let mut sharing = false;
    for ShareCodec {
        codec,
        encoder,
        software,
    } in codecs
    {
        assert!(software);
        let what = codec.label();
        let settings = ShareSettings {
            max_height: None,
            fps: 60,
            sound: false,
            codec,
            kbps: Some(KBPS),
        };
        if sharing {
            // The viewers do nothing, and follow the change of codec.
            sharer.send(Command::SetShareSettings(settings));
        } else {
            sharer.send(Command::StartShare {
                source: ShareSource::TestPicture {
                    width: 1280,
                    height: 720,
                },
                settings,
            });
            tokio::time::sleep(Duration::from_secs(2)).await;
            for viewer in [&plain, &card] {
                viewer.send(Command::Watch {
                    id: sharer.id(),
                    watch: true,
                });
            }
            sharing = true;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;

        let (plain_frames, card_frames, kbps) = tokio::join!(
            plain.frames_over(3),
            card.frames_over(3),
            plain.video_kbps_over(3)
        );
        for (who, (rate, size)) in [("processor", plain_frames), ("card", card_frames)] {
            assert_eq!(size, Some((1280, 720)), "{what}, shown by the {who}");
            assert!(
                (55.0..65.0).contains(&rate),
                "{what}: the {who} shows {rate:.1} fps"
            );
        }
        assert!(
            (0.7..1.3).contains(&(kbps / f64::from(KBPS))),
            "{what}: {KBPS} kbit/s arrive as {kbps:.0}"
        );

        // Both ends say that their processors do the work.
        let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
        assert_eq!((own.codec, own.software, own.kbps), (codec, true, KBPS));
        assert_eq!(own.encoder, encoder);
        assert!((55.0..65.0).contains(&own.sent_fps), "{what}");
        let mut viewers = [plain.id(), card.id()];
        viewers.sort_unstable();
        assert_eq!(own.watchers, viewers);
        let announced = plain.seen.lock().unwrap().peer_share.clone().unwrap();
        assert_eq!((announced.codec, announced.software), (codec, true));
        assert_eq!(announced.encoder, encoder);
        let watching = plain.seen.lock().unwrap().watching.clone().unwrap();
        assert!(watching.software, "{what}");
        let beside = card.seen.lock().unwrap().watching.clone().unwrap();
        eprintln!(
            "{what}: encoded in {:.1} ms by {encoder}; ready in {:.1} ms by {} and in {:.1} ms \
             by {}",
            own.encode_ms, watching.decode_ms, watching.decoder, beside.decode_ms, beside.decoder
        );
    }

    sharer.send(Command::StopShare);
    tokio::time::sleep(Duration::from_secs(2)).await;
    for client in [sharer, plain, card] {
        assert_eq!(client.seen.lock().unwrap().problems, Vec::<String>::new());
        client.send(Command::Leave);
        client.session.await.unwrap().unwrap();
    }
}
