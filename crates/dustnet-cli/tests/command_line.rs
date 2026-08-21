//! The command-line surface, asserted rather than described.
//!
//! `dustnet` hand-rolled argv until this landed: no `--help`, no `--version`,
//! and every mistake exited 1 through the same path. These tests fix the
//! observable contract so a future parser change cannot quietly alter it.

use std::process::{Command, Output};

/// Exit code for a usage error, matching clap's default.
const USAGE_EXIT: i32 = 2;

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dustnet"))
        .args(arguments)
        .output()
        .expect("run dustnet")
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
fn help_lists_every_subcommand_and_succeeds() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "--help must exit 0");
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(printed.contains("Usage: dustnet"), "no usage: {printed:?}");
    // Named individually: a subcommand dropped in a refactor is otherwise
    // invisible, since `--help` still succeeds with the rest.
    for command in [
        "render",
        "check",
        "dump-tokens",
        "dump-ast",
        "dump-cells",
        "dump-scene",
        "connect",
    ] {
        assert!(printed.contains(command), "no `{command}`: {printed:?}");
    }
}

#[test]
fn an_unknown_subcommand_is_a_usage_error_on_stderr() {
    let output = run(&["frobnicate"]);
    assert_eq!(output.status.code(), Some(USAGE_EXIT));
    let printed = String::from_utf8_lossy(&output.stderr);
    assert!(printed.contains("frobnicate"), "silent: {printed:?}");
    assert!(
        output.stdout.is_empty(),
        "a usage error must not write to stdout"
    );
}

#[test]
fn an_unknown_flag_is_a_usage_error() {
    let output = run(&["check", "--no-such-flag", "page.aml"]);
    assert_eq!(output.status.code(), Some(USAGE_EXIT));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--no-such-flag"),
        "the rejected flag must be named"
    );
}

#[test]
fn a_subcommand_missing_its_file_is_a_usage_error() {
    let output = run(&["check"]);
    assert_eq!(output.status.code(), Some(USAGE_EXIT));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage:"));
}

/// `check` is the one subcommand with a documented exit contract: zero when
/// the document parses, non-zero when it does not.
#[test]
fn check_reports_a_valid_document_and_rejects_a_broken_one() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let valid = directory.path().join("valid.aml");
    std::fs::write(&valid, "[page]\n[text]hello[/text]\n[/page]\n").expect("write page");
    let output = run(&["check", &valid.to_string_lossy()]);
    assert!(
        output.status.success(),
        "a valid page must check clean: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let missing = directory.path().join("absent.aml");
    let output = run(&["check", &missing.to_string_lossy()]);
    assert!(!output.status.success(), "a missing file must not succeed");
}
