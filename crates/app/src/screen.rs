//! Asks the desktop which screen or window to share.
//!
//! An app cannot look at the screen on its own. It asks the desktop's
//! ScreenCast portal, the desktop shows its picker, and what the user picks
//! comes back as a PipeWire stream that only this app may read.

use flowerchat_client::session::ScreenSource;

/// The outcome of asking.
pub enum Pick {
    Chosen {
        source: ScreenSource,
        /// The stream lasts for as long as this is kept.
        cast: Cast,
    },
    /// The user closed the picker without choosing.
    Cancelled,
}

/// Permission to read a screen. Dropping it gives the permission back,
/// which ends the stream and whatever the desktop shows while a screen is
/// being shared.
pub struct Cast {
    #[cfg(target_os = "linux")]
    session: Option<portal::Session>,
}

/// Shows the desktop's picker and waits for the user to choose. Fails with
/// a reason that completes the sentence "The screen cannot be shared,
/// because ...".
pub async fn pick() -> Result<Pick, String> {
    portal::pick().await
}

#[cfg(target_os = "linux")]
mod portal {
    use std::sync::Arc;

    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::{PersistMode, ResponseError};
    use flowerchat_client::session::ScreenSource;

    use super::{Cast, Pick};

    pub type Session = ashpd::desktop::Session<Screencast>;

    pub async fn pick() -> Result<Pick, String> {
        match ask().await {
            Ok((source, session)) => Ok(Pick::Chosen {
                source,
                cast: Cast {
                    session: Some(session),
                },
            }),
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(Pick::Cancelled),
            Err(ashpd::Error::NoResponse) => Err("the desktop did not answer".into()),
            Err(err) => Err(format!("the desktop refused ({err})")),
        }
    }

    async fn ask() -> ashpd::Result<(ScreenSource, Session)> {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session(Default::default()).await?;
        // The pointer is drawn into the picture, so that viewers can see
        // what is being pointed at.
        let options = SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::EMBEDDED)
            .set_sources(SourceType::MONITOR | SourceType::WINDOW)
            .set_multiple(false)
            .set_persist_mode(PersistMode::DoNot);
        let outcome = async {
            proxy.select_sources(&session, options).await?.response()?;
            let streams = proxy
                .start(&session, None, Default::default())
                .await?
                .response()?;
            let node = streams
                .streams()
                .first()
                .ok_or(ashpd::Error::NoResponse)?
                .pipe_wire_node_id();
            let remote = proxy
                .open_pipe_wire_remote(&session, Default::default())
                .await?;
            Ok(ScreenSource {
                remote: Some(Arc::new(remote)),
                node,
            })
        }
        .await;
        match outcome {
            Ok(source) => Ok((source, session)),
            Err(err) => {
                let _ = session.close().await;
                Err(err)
            }
        }
    }

    impl Drop for Cast {
        fn drop(&mut self) {
            let Some(session) = self.session.take() else {
                return;
            };
            // Closing takes a round trip to the desktop, and this may be
            // running where waiting is not allowed.
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => drop(runtime.spawn(async move { session.close().await })),
                Err(_) => drop(std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        let _ = runtime.block_on(session.close());
                    }
                })),
            }
        }
    }
}

/// Windows lets a program look at any screen or window without asking, so
/// the choosing is ours to offer. Until there is a picker, it is always
/// the main monitor.
#[cfg(windows)]
mod portal {
    use flowerchat_client::session::ScreenSource;

    use super::{Cast, Pick};

    pub async fn pick() -> Result<Pick, String> {
        Ok(Pick::Chosen {
            source: ScreenSource::PrimaryMonitor,
            cast: Cast {},
        })
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod portal {
    use super::Pick;

    pub async fn pick() -> Result<Pick, String> {
        Err("this system is not supported yet".into())
    }
}
