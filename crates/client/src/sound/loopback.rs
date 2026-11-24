//! Collecting what plays on Windows.
//!
//! Windows records what the machine plays by itself, and can leave one
//! program and the programs it started out of the recording. We leave out
//! ourselves, so the people in the call do not get their own voices back.
//! That takes Windows 10 from build 20348, or Windows 11.
//!
//! Another copy of flowerchat on the same machine is a program like any
//! other here, and its side of the call is recorded.

use anyhow::Result;
use gstreamer as gst;

/// The element that records what plays, named `capture`, and what
/// turns its sound into a format we can ask for.
pub(super) fn description() -> String {
    format!(
        "wasapi2src name=capture loopback=true loopback-mode=exclude-process-tree \
         loopback-target-pid={} low-latency=true provide-clock=false \
         ! audioconvert ! audioresample",
        std::process::id()
    )
}

/// Nothing has to be kept going while recording.
pub(super) struct Collector;

pub(super) fn collect(_capture: &gst::Element) -> Result<Collector> {
    Ok(Collector)
}
