//! End-to-end test: two real clients connect through a real server on this
//! machine, exchange measurement clicks over WebRTC and time them.
//!
//! Needs the GStreamer plugins the client uses and a network interface other
//! than loopback, but no audio device.

use std::process::{Command, Stdio};
use std::time::Duration;

use flowerchat_server::{Config, app};

const CLIENT: &str = env!("CARGO_BIN_EXE_flowerchat-cli");

fn client(url: &str, name: &str, seconds: u32) -> Command {
    let mut command = Command::new(CLIENT);
    command
        .args(["--server", url, "--room", "selftest", "--name", name])
        .args([
            "--test-tone",
            "--measure",
            "--duration",
            &seconds.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Extracts the median from a line like
/// `latency: median 85.1 ms, min 85.1 ms, max 206.8 ms over 11 clicks`.
fn median_ms(stdout: &str) -> Option<f64> {
    let line = stdout.lines().find(|line| line.starts_with("latency:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_hear_each_other() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    // No STUN server: the two clients find each other on local addresses.
    let config = Config {
        ice_servers: vec![],
        releases: None,
    };
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });

    let first = client(&url, "first", 9).spawn().unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let second = client(&url, "second", 6).spawn().unwrap();

    for (name, child) in [("second", second), ("first", first)] {
        let output = tokio::task::spawn_blocking(move || child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{name} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        let median = median_ms(&stdout)
            .unwrap_or_else(|| panic!("{name} reported no latency\nstdout:\n{stdout}"));
        // Far below this means the clock mapping is broken; far above means
        // something in the pipeline is buffering that should not be.
        assert!(
            (30.0..150.0).contains(&median),
            "{name} measured an implausible {median} ms\nstdout:\n{stdout}"
        );
    }
}
