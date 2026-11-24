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

/// What there is to share, on a system that leaves the choosing to us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choices {
    pub screens: Vec<Choice>,
    /// The open windows, the one used last first.
    pub windows: Vec<Choice>,
}

/// A screen or a window that could be shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What the system knows it by.
    pub id: u64,
    /// "Screen 1", or the title of the window.
    pub title: String,
    /// The size of a screen, or the program a window belongs to.
    pub detail: String,
}

/// One of the [`Choices`], by its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    Screen(u64),
    Window(u64),
}

/// What there is to share right now. `None` where the desktop does the
/// asking, and [`pick`] is the way.
pub fn choices() -> Option<Choices> {
    portal::choices()
}

/// What [`pick`] would give for one of the [`choices`].
pub fn listed(picked: Picked) -> Option<Pick> {
    portal::listed(picked)
}

#[cfg(target_os = "linux")]
mod portal {
    use std::sync::Arc;

    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::{PersistMode, ResponseError};
    use flowerchat_client::session::ScreenSource;

    use super::{Cast, Choices, Pick, Picked};

    pub type Session = ashpd::desktop::Session<Screencast>;

    pub fn choices() -> Option<Choices> {
        None
    }

    pub fn listed(_: Picked) -> Option<Pick> {
        None
    }

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
/// the choosing is ours to offer.
#[cfg(windows)]
mod portal {
    use std::ffi::c_void;

    use flowerchat_client::session::ScreenSource;
    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow, GetWindowLongW,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    use super::{Cast, Choice, Choices, Pick, Picked};

    /// Set in a monitor's flags when it is the main one.
    const MAIN_MONITOR: u32 = 1;

    pub async fn pick() -> Result<Pick, String> {
        Ok(chosen(ScreenSource::PrimaryMonitor))
    }

    pub fn choices() -> Option<Choices> {
        let mut choices = Choices::default();
        // Both calls run the function they are given once for each monitor
        // or window, before they return.
        unsafe {
            let screens = &raw mut choices.screens;
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                Some(add_monitor),
                screens as LPARAM,
            );
            let windows = &raw mut choices.windows;
            EnumWindows(Some(add_window), windows as LPARAM);
        }
        choices.screens.sort_by(|a, b| a.title.cmp(&b.title));
        Some(choices)
    }

    pub fn listed(picked: Picked) -> Option<Pick> {
        Some(chosen(match picked {
            Picked::Screen(id) => ScreenSource::Monitor(id),
            Picked::Window(id) => ScreenSource::Window(id),
        }))
    }

    fn chosen(source: ScreenSource) -> Pick {
        Pick::Chosen {
            source,
            cast: Cast {},
        }
    }

    unsafe extern "system" fn add_monitor(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        list: LPARAM,
    ) -> i32 {
        let list = unsafe { &mut *(list as *mut Vec<Choice>) };
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        let known = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast::<MONITORINFO>()) };
        if known != 0 {
            let area = info.monitorInfo.rcMonitor;
            // Windows names them `\\.\DISPLAY1` and so on, with the numbers
            // its own settings show.
            let number: String = text(&info.szDevice)
                .chars()
                .filter(char::is_ascii_digit)
                .collect();
            let size = format!("{} × {}", area.right - area.left, area.bottom - area.top);
            list.push(Choice {
                id: monitor as u64,
                title: format!("Screen {number}"),
                detail: if info.monitorInfo.dwFlags & MAIN_MONITOR != 0 {
                    format!("{size} · main")
                } else {
                    size
                },
            });
        }
        1
    }

    unsafe extern "system" fn add_window(window: HWND, list: LPARAM) -> i32 {
        let list = unsafe { &mut *(list as *mut Vec<Choice>) };
        if let Some(choice) = unsafe { window_choice(window) } {
            list.push(choice);
        }
        1
    }

    /// What to call a window, if it is one a person would think of as a
    /// window: on screen, with a title, and not ours.
    unsafe fn window_choice(window: HWND) -> Option<Choice> {
        unsafe {
            // A minimized window has no picture to capture.
            if IsWindowVisible(window) == 0 || IsIconic(window) != 0 {
                return None;
            }
            // Tool windows and windows that belong to another window are
            // parts of a program, not the program.
            let tool = GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0;
            if tool || !GetWindow(window, GW_OWNER).is_null() {
                return None;
            }
            // Windows keeps some windows around without showing them, such
            // as those of apps that are suspended or on another desktop.
            let mut cloaked = 0u32;
            DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED as u32,
                (&raw mut cloaked).cast::<c_void>(),
                size_of::<u32>() as u32,
            );
            if cloaked != 0 {
                return None;
            }
            let mut buffer = [0u16; 256];
            let length = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32);
            let title = text(&buffer[..length.max(0) as usize]);
            GetClassNameW(window, buffer.as_mut_ptr(), buffer.len() as i32);
            // The desktop itself is a window, called this.
            if title.trim().is_empty() || text(&buffer) == "Progman" {
                return None;
            }
            let mut process = 0u32;
            GetWindowThreadProcessId(window, &raw mut process);
            if process == std::process::id() {
                return None;
            }
            Some(Choice {
                id: window as u64,
                title,
                detail: program(process).unwrap_or_default(),
            })
        }
    }

    /// The name of a process's program, without folder and `.exe`.
    fn program(process: u32) -> Option<String> {
        let mut buffer = [0u16; 512];
        let mut length = buffer.len() as u32;
        let path = unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process);
            if handle.is_null() {
                return None;
            }
            let found = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &raw mut length);
            CloseHandle(handle);
            if found == 0 {
                return None;
            }
            text(&buffer[..length as usize])
        };
        let name = std::path::Path::new(&path).file_stem()?;
        Some(name.to_string_lossy().into_owned())
    }

    /// Text as Windows hands it out, up to the zero that ends it.
    fn text(units: &[u16]) -> String {
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod portal {
    use super::{Choices, Pick, Picked};

    pub async fn pick() -> Result<Pick, String> {
        Err("this system is not supported yet".into())
    }

    pub fn choices() -> Option<Choices> {
        None
    }

    pub fn listed(_: Picked) -> Option<Pick> {
        None
    }
}
