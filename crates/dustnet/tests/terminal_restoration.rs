#![cfg(unix)]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn pty_command_for(binary: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new("script");
    command.env("DUSTNET_AMBIGUOUS_WIDTH", "1");
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // Selected by a `cfg!` expression rather than a `#[cfg]` attribute, so
    // both branches type-check on both platforms. Under an attribute the Linux
    // branch was invisible to every macOS clippy run and could rot silently
    // between Linux runs, which is indistinguishable from a real Linux
    // failure when one finally happens.
    if cfg!(target_os = "macos") {
        command.args(["-q", "/dev/null", binary]);
        command.args(arguments);
    } else {
        let invocation = std::iter::once(binary)
            .chain(arguments.iter().copied())
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        command.args(["-q", "-e", "-c", &invocation, "/dev/null"]);
    }
    command
}

fn pty_command(arguments: &[&str]) -> Command {
    pty_command_for(env!("CARGO_BIN_EXE_dustnet"), arguments)
}

// Compiled everywhere, run only where the driver shape exists: macOS `script`
// has no `-c`, so the invocation under test is Linux-only, but the body must
// keep type-checking on macOS or it rots.
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS `script` takes no -c; this exercises the Linux driver shape"
)]
#[test]
fn linux_pty_driver_propagates_child_status() {
    let output = pty_command_for("/bin/sh", &["-c", "exit 23"])
        .output()
        .expect("run PTY status probe");
    assert_eq!(output.status.code(), Some(23));
}

fn run_in_pty(arguments: &[&str], input: &[u8]) -> Output {
    let mut command = pty_command(arguments);
    let mut child = command.spawn().expect("spawn PTY driver");
    if !input.is_empty() {
        // Let the child enter raw mode before delivering control input; data
        // written while `script` is still establishing the PTY can be lost.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    child
        .stdin
        .take()
        .expect("PTY stdin")
        .write_all(input)
        .expect("write PTY input");
    child.wait_with_output().expect("wait for PTY command")
}

fn assert_restored(output: &Output) {
    let bytes = [&output.stdout[..], &output.stderr[..]].concat();
    assert_restored_bytes(&bytes);
}

fn assert_restored_bytes(bytes: &[u8]) {
    assert!(
        bytes
            .windows(b"\x1b[?1049h".len())
            .any(|w| w == b"\x1b[?1049h"),
        "viewer never entered the alternate screen: {}",
        String::from_utf8_lossy(bytes)
    );
    assert!(
        bytes
            .windows(b"\x1b[?1000l".len())
            .any(|w| w == b"\x1b[?1000l"),
        "mouse capture was not disabled"
    );
    assert!(
        bytes.windows(b"\x1b[?25h".len()).any(|w| w == b"\x1b[?25h"),
        "cursor was not restored"
    );
    assert!(
        bytes
            .windows(b"\x1b[?1049l".len())
            .any(|w| w == b"\x1b[?1049l"),
        "viewer never left the alternate screen"
    );
}

#[test]
fn terminal_is_restored_after_normal_exit() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/aml/hello.aml");
    let output = run_in_pty(&["render", fixture.to_str().unwrap()], b"q");
    assert!(output.status.success(), "{output:?}");
    assert_restored(&output);
}

#[test]
fn terminal_is_restored_after_connected_viewer_error() {
    let output = run_in_pty(&["connect", "atp://127.0.0.1:1/", "--no-tls"], b"");
    assert!(
        !output.status.success(),
        "connection unexpectedly succeeded"
    );
    assert_restored(&output);
}

fn run_signal_restoration(signal_name: &str) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/aml/hello.aml");
    let mut command = pty_command(&["render", fixture.to_str().unwrap()]);
    let mut child = command.spawn().expect("spawn PTY driver");
    let mut stdout = child.stdout.take().expect("PTY stdout");
    let mut stderr = child.stderr.take().expect("PTY stderr");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let stdout_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 4096];
        let mut notified = false;
        loop {
            let count = stdout.read(&mut chunk).expect("read PTY stdout");
            if count == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..count]);
            if !notified
                && output
                    .windows(b"\x1b[?1000h".len())
                    .any(|window| window == b"\x1b[?1000h")
            {
                ready_tx.send(()).expect("notify rendered viewer");
                notified = true;
            }
        }
        output
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).expect("read PTY stderr");
        output
    });
    if ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        let bytes = stdout_reader.join().expect("join timed-out stdout reader");
        let _ = stderr_reader.join();
        panic!(
            "viewer did not enter terminal mode before signal: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let mut viewer_pid = child.id().to_string();
    loop {
        let lookup = Command::new("pgrep")
            .args(["-P", &viewer_pid])
            .output()
            .expect("find PTY child");
        if !lookup.status.success() {
            break;
        }
        let Some(descendant) = String::from_utf8(lookup.stdout)
            .expect("numeric child PID")
            .lines()
            .next()
            .map(str::trim)
            .filter(|pid| !pid.is_empty())
            .map(str::to_string)
        else {
            break;
        };
        viewer_pid = descendant;
    }
    let signal = Command::new("kill")
        .args([signal_name, &viewer_pid])
        .status()
        .expect("send signal");
    assert!(signal.success(), "failed to send {signal_name}");

    let status = child.wait().expect("wait for signalled viewer");
    let mut bytes = stdout_reader.join().expect("join stdout reader");
    bytes.extend(stderr_reader.join().expect("join stderr reader"));
    assert!(
        status.success(),
        "viewer status {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    assert_restored_bytes(&bytes);
}

#[test]
fn terminal_is_restored_after_supported_signals() {
    for signal in ["-INT", "-TERM", "-HUP"] {
        run_signal_restoration(signal);
    }
}

#[test]
fn terminal_panic_helper() {
    if std::env::var_os("DUSTNET_TERMINAL_PANIC_HELPER").is_none() {
        return;
    }
    let _terminal = dustnet_client::compositor::terminal::Terminal::enter()
        .expect("enter terminal before deliberate panic");
    panic!("deliberate terminal restoration test panic");
}

#[test]
fn terminal_is_restored_after_panic_unwind() {
    let executable = std::env::current_exe().expect("current integration-test executable");
    let mut command = pty_command_for(
        executable.to_str().unwrap(),
        &["--exact", "terminal_panic_helper", "--nocapture"],
    );
    command.env("DUSTNET_TERMINAL_PANIC_HELPER", "1");
    let output = command.output().expect("run panic helper in PTY");
    assert!(!output.status.success(), "panic helper unexpectedly passed");
    assert_restored(&output);
}

/// Killing the PTY driver must not leave an orphaned viewer spinning.
///
/// When the terminal goes away without a signal reaching the viewer — window
/// closed, ssh dropped, parent gone — the descriptor stays open and reports
/// `Ok((0, 0))` rather than an error. `crossterm`'s poll then spins inside
/// `read(2)` and never returns, so the loop never reaches its termination
/// check: the process ignores SIGTERM and burns a core until SIGKILLed.
/// Orphaned viewers were found doing this for three days at ~90% CPU each.
///
/// This kills `script` rather than the viewer, which is what closes the
/// master and reproduces the real condition; the viewer itself is signalled
/// by nothing.
///
/// The fixture is copied to a uniquely named file so the survivor check
/// matches only this test's viewer. The sibling tests in this file also run
/// `dustnet render`, and at `-j 4` they overlap.
#[test]
fn viewer_exits_when_its_terminal_disappears() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/aml/hello.aml");
    let unique = std::env::temp_dir().join(format!(
        "dustnet-orphan-probe-{}-{}.aml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::copy(&source, &unique).expect("stage a uniquely named fixture");
    let marker = unique.to_str().expect("utf-8 temp path").to_string();

    let mut command = pty_command(&["render", &marker]);
    let mut child = command.spawn().expect("spawn PTY driver");

    // Let the viewer reach its main loop before the terminal is taken away.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    child.kill().expect("kill the PTY driver");
    let _ = child.wait();

    // The viewer is now orphaned with a dead terminal. It must exit on its
    // own; nothing will signal it.
    let survivors = || {
        let output = Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .expect("run pgrep");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|pid| pid.parse::<i32>().ok())
            .collect::<Vec<_>>()
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let leaked = loop {
        let alive = survivors();
        if alive.is_empty() {
            break Vec::new();
        }
        if std::time::Instant::now() >= deadline {
            break alive;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };

    // Never leak a spinning process out of the test, whatever the outcome.
    for pid in &leaked {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    let _ = std::fs::remove_file(&unique);

    assert!(
        leaked.is_empty(),
        "viewer still running 15s after its terminal disappeared \
         (pids {leaked:?}); it is spinning on a dead descriptor"
    );
}
