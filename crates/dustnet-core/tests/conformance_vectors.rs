//! The published conformance vectors, executed against this implementation.
//!
//! `tests/conformance/` exists so that an independent implementation can check
//! itself: accept everything under `valid/`, reject everything under
//! `invalid/`, and sanitize the `sanitize/` inputs to their recorded output.
//! Until this test existed the vectors were inert files that nothing read, so
//! the reference implementation was not held to the contract it published.
//!
//! Three kinds of vector, and only the first needs a manifest:
//!
//! - **ATP bodies** (`.atp`). A wire body is just bytes; nothing in the file
//!   says which message it is or which flags accompanied it, so
//!   `tests/conformance/vectors.json` records that pairing. The expectation is
//!   *not* recorded there — the directory decides it, so the two cannot drift.
//! - **AML documents** (`.aml`). Discovered from the filesystem and run
//!   through scan-and-parse.
//! - **Sanitization inputs** (`sanitize/*.aml` with a sibling `.expected`).
//!   The expected output is hand-written, so the vector asserts the contract
//!   rather than asserting the implementation equals itself.
//!
//! `docs/spec/05-conformance.md` is the prose. Nothing here parses it.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use dustnet_core::protocol::PROTOCOL_VERSION;
use dustnet_core::protocol::frame::MessageType;
use dustnet_core::protocol::message::{HelloMessage, WelcomeMessage, validate_frame_body};

/// Repository root, resolved from this crate's manifest at compile time.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("core crate is nested under <workspace>/crates")
        .to_path_buf()
}

fn conformance_root() -> PathBuf {
    repository_root().join("tests/conformance")
}

/// What a directory name promises about the files inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expectation {
    Accept,
    Reject,
}

/// Derived from the path rather than restated in the manifest: a vector that
/// moved between directories changes what it asserts, and cannot disagree with
/// a second copy of the same claim.
fn expectation_for(relative: &str) -> Expectation {
    if let Some(directory) = relative.split('/').next() {
        match directory {
            "valid" => return Expectation::Accept,
            "invalid" => return Expectation::Reject,
            _ => {}
        }
    }
    panic!("{relative} is in neither valid/ nor invalid/");
}

/// The manifest names messages the way the specification's grammar does.
/// Exhaustive on purpose: a typo becomes a failure rather than a skip.
fn message_type(name: &str) -> MessageType {
    match name {
        "HELLO" => MessageType::Hello,
        "GET" => MessageType::Get,
        "INPUT" => MessageType::Input,
        "SUBSCRIBE" => MessageType::Subscribe,
        "UNSUBSCRIBE" => MessageType::Unsubscribe,
        "PING" => MessageType::Ping,
        "BYE" => MessageType::Bye,
        "WELCOME" => MessageType::Welcome,
        "PAGE" => MessageType::Page,
        "UPDATE" => MessageType::Update,
        "REDIRECT" => MessageType::Redirect,
        "ERROR" => MessageType::Error,
        "RESOURCE" => MessageType::Resource,
        "PONG" => MessageType::Pong,
        "SERVER-BYE" => MessageType::ServerBye,
        other => panic!("vectors.json names unknown message type `{other}`"),
    }
}

struct AtpVector {
    file: String,
    message: MessageType,
    flags: u8,
    /// The capabilities a HELLO offered, for vectors whose validity depends on
    /// the connection rather than on the bytes.
    offered: Option<Vec<String>>,
    note: String,
}

/// Validate one vector the way a client does: the body first, then the
/// connection-level rules that body validation cannot see.
///
/// Splitting these matters. `invalid/unoffered-welcome.atp` is a *well-formed*
/// WELCOME — "may select only names HELLO offered" is a claim about a
/// connection, not about bytes — so a suite that stopped at
/// `validate_frame_body` would publish it as invalid while accepting it. The
/// same is true of the version line: a peer must parse a version it does not
/// speak in order to say so, which is why body validation checks only the
/// `HELLO/` prefix and the announced version is compared here, against the
/// same `PROTOCOL_VERSION` both peers compare it to.
fn validate_vector(vector: &AtpVector, body: &[u8]) -> Result<(), String> {
    validate_frame_body(vector.message, body, vector.flags).map_err(|error| error.to_string())?;

    let announced_version = match vector.message {
        MessageType::Hello => {
            let text = std::str::from_utf8(body).map_err(|_| "HELLO is not UTF-8".to_owned())?;
            Some(
                HelloMessage::parse(text)
                    .map_err(|error| error.to_string())?
                    .protocol_version,
            )
        }
        MessageType::Welcome => {
            let text = std::str::from_utf8(body).map_err(|_| "WELCOME is not UTF-8".to_owned())?;
            Some(
                WelcomeMessage::parse(text)
                    .map_err(|error| error.to_string())?
                    .protocol_version,
            )
        }
        _ => None,
    };
    if let Some(version) = announced_version
        && version != PROTOCOL_VERSION
    {
        return Err(format!(
            "announced protocol version `{version}` is not `{PROTOCOL_VERSION}`"
        ));
    }

    if vector.message == MessageType::Welcome
        && let Some(offered) = &vector.offered
    {
        let text = std::str::from_utf8(body).map_err(|_| "WELCOME is not UTF-8".to_owned())?;
        let welcome = WelcomeMessage::parse(text).map_err(|error| error.to_string())?;
        if let Some(unoffered) = welcome
            .capabilities
            .iter()
            .find(|capability| !offered.contains(capability))
        {
            return Err(format!(
                "WELCOME selects `{unoffered}`, which HELLO did not offer"
            ));
        }
    }

    Ok(())
}

fn atp_vectors() -> Vec<AtpVector> {
    let path = conformance_root().join("vectors.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).expect("vectors.json is not valid JSON");
    let entries = parsed
        .get("atp")
        .and_then(serde_json::Value::as_array)
        .expect("vectors.json has no `atp` array");

    entries
        .iter()
        .map(|entry| {
            let file = entry
                .get("file")
                .and_then(serde_json::Value::as_str)
                .expect("vector entry has no `file`")
                .to_owned();
            let message = entry
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("{file} has no `message`"));
            let flags = entry
                .get("flags")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(|| panic!("{file} has no `flags`"));
            let offered = entry.get("offered").map(|value| {
                value
                    .as_array()
                    .unwrap_or_else(|| panic!("{file} has a non-array `offered`"))
                    .iter()
                    .map(|name| {
                        name.as_str()
                            .unwrap_or_else(|| panic!("{file} has a non-string capability"))
                            .to_owned()
                    })
                    .collect()
            });
            let note = entry
                .get("note")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("{file} has no `note`"))
                .to_owned();
            AtpVector {
                message: message_type(message),
                offered,
                flags: u8::try_from(flags).unwrap_or_else(|_| panic!("{file} flags exceed a byte")),
                file,
                note,
            }
        })
        .collect()
}

/// Every file under `tests/conformance/` with the given extension, as a path
/// relative to that directory and using `/` separators.
fn vector_files(extension: &str) -> Vec<String> {
    let root = conformance_root();
    let mut found = Vec::new();
    collect(&root, &root, extension, &mut found);
    found.sort();
    found
}

fn collect(root: &Path, directory: &Path, extension: &str, output: &mut Vec<String>) {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", directory.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect(root, &path, extension, output);
        } else if path.extension().is_some_and(|found| found == extension) {
            let relative = path
                .strip_prefix(root)
                .expect("vector is under the conformance root");
            output.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// The gate that keeps the suite honest: a fixture nobody runs proves nothing,
/// and a manifest row naming a deleted file is a claim about code that is gone.
#[test]
fn every_atp_vector_on_disk_is_listed_and_every_listed_vector_exists() {
    let listed: Vec<String> = {
        let mut names: Vec<String> = atp_vectors().into_iter().map(|v| v.file).collect();
        names.sort();
        names
    };
    let on_disk = vector_files("atp");

    let unlisted: Vec<&String> = on_disk.iter().filter(|f| !listed.contains(f)).collect();
    assert!(
        unlisted.is_empty(),
        "these ATP vectors exist but vectors.json does not list them, so nothing runs them: {unlisted:?}"
    );

    let missing: Vec<&String> = listed.iter().filter(|f| !on_disk.contains(f)).collect();
    assert!(
        missing.is_empty(),
        "vectors.json lists these ATP vectors, but the files are gone: {missing:?}"
    );
}

#[test]
fn atp_vectors_are_accepted_or_rejected_as_their_directory_promises() {
    let vectors = atp_vectors();
    assert!(!vectors.is_empty(), "vectors.json lists no ATP vectors");

    for vector in &vectors {
        let path = conformance_root().join(&vector.file);
        let body = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));
        let outcome = validate_vector(vector, &body);

        match expectation_for(&vector.file) {
            Expectation::Accept => assert!(
                outcome.is_ok(),
                "{} is published as a valid {:?} body ({}) but was rejected: {:?}",
                vector.file,
                vector.message,
                vector.note,
                outcome.err()
            ),
            Expectation::Reject => assert!(
                outcome.is_err(),
                "{} is published as an invalid {:?} body ({}) but was accepted",
                vector.file,
                vector.message,
                vector.note
            ),
        }
    }
}

#[test]
fn aml_vectors_parse_or_fail_as_their_directory_promises() {
    let files = vector_files("aml");
    assert!(!files.is_empty(), "no AML vectors are published");

    for relative in &files {
        // Sanitization vectors are inputs to a transform, not accept/reject
        // cases, and have their own test below.
        if relative.starts_with("sanitize/") {
            continue;
        }
        let path = conformance_root().join(relative);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));

        let parsed = dustnet_core::scanner::Scanner::new(&bytes)
            .and_then(|mut scanner| scanner.scan_all())
            .map(dustnet_core::parser::parse);

        let accepted = match &parsed {
            Ok(result) => !result.has_errors(),
            Err(_) => false,
        };

        match expectation_for(relative) {
            Expectation::Accept => assert!(
                accepted,
                "{relative} is published as a valid AML document but was rejected: {:?}",
                parsed.map(|result| result.diagnostics)
            ),
            Expectation::Reject => assert!(
                !accepted,
                "{relative} is published as an invalid AML document but parsed cleanly"
            ),
        }
    }
}

/// The sanitization contract in `docs/spec/05-conformance.md`: the listed
/// characters are removed and the document still renders, rather than the
/// document being rejected.
#[test]
fn sanitization_vectors_produce_their_recorded_output() {
    let root = conformance_root().join("sanitize");
    let mut checked = 0usize;

    let entries = std::fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", root.display()));
    for entry in entries {
        let input = entry.expect("directory entry").path();
        if input.extension().is_none_or(|found| found != "aml") {
            continue;
        }
        let expected_path = input.with_extension("expected");
        let raw = std::fs::read_to_string(&input)
            .unwrap_or_else(|error| panic!("{} is not readable: {error}", input.display()));
        let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|error| {
            panic!("{} has no sibling .expected file: {error}", input.display())
        });

        assert_eq!(
            dustnet_core::scanner::escape::sanitize(&raw),
            expected,
            "{} did not sanitize to its recorded output",
            input.display()
        );

        // A sanitized document must still be a document: removal, not
        // rejection, is the whole point of the rule.
        let mut scanner = dustnet_core::scanner::Scanner::new(raw.as_bytes())
            .unwrap_or_else(|error| panic!("{} failed scanning: {error}", input.display()));
        let tokens = scanner
            .scan_all()
            .unwrap_or_else(|error| panic!("{} failed scanning: {error}", input.display()));
        let result = dustnet_core::parser::parse(tokens);
        assert!(
            !result.has_errors(),
            "{} should still parse after sanitization: {:?}",
            input.display(),
            result.diagnostics
        );

        checked += 1;
    }

    assert!(
        checked > 0,
        "no sanitization vectors were found; the fixture layout has moved"
    );
}

/// Guards the guards. Each of the tests above iterates whatever it finds, so a
/// collapsed fixture directory would leave them passing while asserting almost
/// nothing. Floors sit below the current counts so ordinary additions do not
/// trip them; never lower one to make a failing run pass.
#[test]
fn the_published_vector_set_retains_its_expected_breadth() {
    let atp = atp_vectors();
    let accepted = atp
        .iter()
        .filter(|v| expectation_for(&v.file) == Expectation::Accept)
        .count();
    let rejected = atp.len() - accepted;

    assert!(
        accepted >= 20,
        "expected at least 20 accept vectors, found {accepted}"
    );
    assert!(
        rejected >= 18,
        "expected at least 18 reject vectors, found {rejected}"
    );

    let covered: std::collections::BTreeSet<&str> = atp
        .iter()
        .filter(|v| expectation_for(&v.file) == Expectation::Accept)
        .map(|v| match v.message {
            MessageType::Hello => "HELLO",
            MessageType::Get => "GET",
            MessageType::Input => "INPUT",
            MessageType::Subscribe => "SUBSCRIBE",
            MessageType::Unsubscribe => "UNSUBSCRIBE",
            MessageType::Ping => "PING",
            MessageType::Bye => "BYE",
            MessageType::Welcome => "WELCOME",
            MessageType::Page => "PAGE",
            MessageType::Update => "UPDATE",
            MessageType::Redirect => "REDIRECT",
            MessageType::Error => "ERROR",
            MessageType::Resource => "RESOURCE",
            MessageType::Pong => "PONG",
            MessageType::ServerBye => "SERVER-BYE",
        })
        .collect();

    // RESOURCE carries opaque bytes and has no body grammar to publish.
    let required = [
        "HELLO",
        "WELCOME",
        "GET",
        "INPUT",
        "SUBSCRIBE",
        "UNSUBSCRIBE",
        "PING",
        "PONG",
        "BYE",
        "SERVER-BYE",
        "PAGE",
        "UPDATE",
        "REDIRECT",
        "ERROR",
    ];
    let uncovered: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| !covered.contains(name))
        .collect();
    assert!(
        uncovered.is_empty(),
        "no accept vector is published for these message types: {uncovered:?}"
    );
}
