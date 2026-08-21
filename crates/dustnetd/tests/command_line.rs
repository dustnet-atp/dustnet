//! The command-line surface, asserted rather than described.
//!
//! Both binaries hand-rolled argv until this landed, so `--help` and
//! `--version` did not exist and an unknown flag exited through the same
//! `usage()` path as a missing argument. These tests fix the observable
//! contract so a future parser change cannot quietly alter it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Exit code for a usage error. Matches clap's default and the daemon's own
/// `USAGE_EXIT`, so a caller cannot distinguish "bad flag" from "bad value".
const USAGE_EXIT: i32 = 2;

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dustnetd"))
        .args(arguments)
        .output()
        .expect("run dustnetd")
}

#[test]
fn version_prints_the_package_version_and_succeeds() {
    let output = run(&["--version"]);
    assert!(output.status.success(), "--version must exit 0");
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(
        printed.contains(env!("CARGO_PKG_VERSION")),
        "--version printed {printed:?}, which omits {}",
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn help_prints_usage_and_succeeds() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "--help must exit 0");
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(printed.contains("Usage: dustnetd"), "no usage: {printed:?}");
    assert!(printed.contains("--bind"), "no --bind: {printed:?}");
}

#[test]
fn an_unknown_flag_is_a_usage_error_on_stderr() {
    let output = run(&["--no-such-flag"]);
    assert_eq!(output.status.code(), Some(USAGE_EXIT));
    let printed = String::from_utf8_lossy(&output.stderr);
    assert!(printed.contains("--no-such-flag"), "silent: {printed:?}");
    assert!(
        output.stdout.is_empty(),
        "a usage error must not write to stdout"
    );
}

/// The loopback restriction is a security invariant, not a default: plaintext
/// ATP carries session state, so `--bind` must not be able to widen it. The
/// listener refuses this too; refusing at the argument boundary makes it a
/// stated error rather than a bind failure an operator has to interpret.
#[test]
fn plaintext_loopback_refuses_a_non_loopback_bind() {
    let site = tempfile::tempdir().expect("temporary static site");
    std::fs::write(site.path().join("index.aml"), "[page][/page]").expect("write page");
    let output = Command::new(env!("CARGO_BIN_EXE_dustnetd"))
        .arg(site.path())
        .args(["--plaintext-loopback", "--bind", "0.0.0.0", "--port", "0"])
        .output()
        .expect("run dustnetd");
    assert_eq!(
        output.status.code(),
        Some(USAGE_EXIT),
        "a non-loopback plaintext bind must be refused"
    );
    let printed = String::from_utf8_lossy(&output.stderr);
    assert!(printed.contains("loopback"), "unexplained: {printed:?}");
}

/// `--bind` is only meaningful if it selects the address actually listened on.
///
/// Asserted against `::1` rather than `127.0.0.2`: the IPv6 loopback is
/// distinct from the `127.0.0.1` default on both claimed platforms, whereas
/// `127.0.0.2` is aliased on Linux and not on macOS — a test that passes for
/// a platform-specific reason is not evidence.
#[test]
fn bind_selects_the_listening_address() {
    let port = {
        let reserved = TcpListener::bind("[::1]:0").expect("reserve IPv6 loopback port");
        reserved.local_addr().expect("reserved address").port()
    };
    let site = tempfile::tempdir().expect("temporary static site");
    std::fs::write(site.path().join("index.aml"), "[page][/page]").expect("write page");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dustnetd"))
        .arg(site.path())
        .args([
            "--plaintext-loopback",
            "--bind",
            "::1",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dustnetd");

    let address = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(15);
    let connected = loop {
        if let Some(status) = child.try_wait().expect("poll dustnetd") {
            panic!("dustnetd exited before binding ::1 ({status})");
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(50)) {
            break stream;
        }
        assert!(Instant::now() < deadline, "dustnetd never bound ::1");
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(connected);

    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("signal dustnetd");
    assert!(killed.success());
    let _ = child.wait();
    let mut discard = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_end(&mut discard);
    }
    let _ = std::io::stdout().flush();
}
