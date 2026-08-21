#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Debug binaries can take several seconds to reach the listener while the
// all-workspace checkpoint is also running CPU-heavy TLS/server tests. Startup
// is not the bounded behavior under test; signal drain remains capped below.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(9);
static SIGNAL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn unused_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    listener.local_addr().expect("reserved address").port()
}

fn collect_output(child: &mut Child) -> String {
    let mut bytes = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout
            .read_to_end(&mut bytes)
            .expect("read dustnetd stdout");
    }
    if let Some(mut stderr) = child.stderr.take() {
        stderr
            .read_to_end(&mut bytes)
            .expect("read dustnetd stderr");
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll dustnetd status") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn connect_when_ready(child: &mut Child, address: SocketAddr) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll dustnetd during startup") {
            let output = collect_output(child);
            panic!("dustnetd exited before accepting connections ({status}): {output}");
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(50)) {
            return stream;
        }
        assert!(Instant::now() < deadline, "dustnetd did not start in time");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_bounded_signal_drain(signal: &str) {
    // Both cases spawn the same process binary and briefly reserve a loopback
    // port. Serialize them so parallel test runners cannot create startup
    // contention or race the release-and-rebind window.
    let _guard = SIGNAL_TEST_LOCK.lock().expect("signal test lock");
    let site = tempfile::tempdir().expect("temporary static site");
    std::fs::write(site.path().join("index.aml"), "[page][/page]").expect("write static page");
    let port = unused_loopback_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dustnetd"))
        .arg(site.path())
        .args(["--plaintext-loopback", "--port", &port.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dustnetd");

    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stalled = connect_when_ready(&mut child, address);
    // Leave an incomplete ATP header in flight. This proves the process signal
    // path bounds draining even when a connection task cannot finish itself.
    stalled.write_all(&[0]).expect("start stalled ATP frame");
    std::thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    let signal_status = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .expect("send process signal");
    assert!(signal_status.success(), "failed to send {signal}");

    let Some(status) = wait_for_exit(&mut child, SHUTDOWN_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        let output = collect_output(&mut child);
        panic!("dustnetd did not drain after {signal}: {output}");
    };
    let elapsed = started.elapsed();
    let output = collect_output(&mut child);
    assert!(
        status.success(),
        "dustnetd exited unsuccessfully after {signal} ({status}): {output}"
    );
    assert!(
        elapsed <= SHUTDOWN_TIMEOUT,
        "dustnetd exceeded its bounded drain after {signal}: {elapsed:?}"
    );
}

#[test]
fn sigint_triggers_bounded_drain() {
    assert_bounded_signal_drain("-INT");
}

#[test]
fn sigterm_triggers_bounded_drain() {
    assert_bounded_signal_drain("-TERM");
}
