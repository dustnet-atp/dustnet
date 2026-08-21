//! Every AML document tracked in the repository must scan and parse within
//! current client limits.
//!
//! This is the mechanical form of the "all repository AML pages pass
//! unchanged" gate. It deliberately reads only tracked, hermetic corpora:
//!
//! - `tests/fixtures/` — the pages and live fragments the client test suite
//!   renders.
//! - `tests/conformance/valid/` — the published conformance vectors.
//! - `examples/demo-site/` — a complete multi-page example site, covering
//!   markup a single hand-written fixture does not reach: cross-page links and
//!   navigation, panel state machines, page transitions, live regions, WASM
//!   effect references, and forms.
//!
//! `fuzz/seeds/` is excluded: those inputs are adversarial by construction and
//! include deliberate non-`[page]` fragments that must fail to parse.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// Repository root, resolved from this crate's manifest at compile time.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("core crate is nested under <workspace>/crates")
        .to_path_buf()
}

fn aml_files(directory: &Path, output: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", directory.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            aml_files(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "aml") {
            output.push(path);
        }
    }
}

fn tracked_corpus() -> Vec<PathBuf> {
    let root = repository_root();
    let mut files = Vec::new();
    aml_files(&root.join("tests/fixtures"), &mut files);
    aml_files(&root.join("tests/conformance/valid"), &mut files);
    aml_files(&root.join("examples/demo-site"), &mut files);
    files.sort();
    files
}

/// Floor for the tracked corpus, deliberately set below the current count (30)
/// so that ordinary churn does not fail the gate while a collapsed or moved
/// fixture layout still does. Never lower it to make a failing run pass.
const MINIMUM_CORPUS: usize = 24;

#[test]
fn every_tracked_aml_document_conforms_to_current_client_limits() {
    let files = tracked_corpus();
    assert!(
        !files.is_empty(),
        "the tracked AML corpus is empty; the fixture layout has moved"
    );

    for path in &files {
        let bytes = std::fs::read(path)
            .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));

        let mut scanner = dustnet_core::scanner::Scanner::new(&bytes)
            .unwrap_or_else(|error| panic!("{} failed scanning: {error}", path.display()));
        let tokens = scanner
            .scan_all()
            .unwrap_or_else(|error| panic!("{} failed scanning: {error}", path.display()));

        let result = dustnet_core::parser::parse(tokens);
        assert!(
            !result.has_errors(),
            "{} violates client limits: {:?}",
            path.display(),
            result.diagnostics
        );
    }
}

/// Guards the guard: if the corpus silently shrinks, the test above would
/// still pass while covering almost nothing.
#[test]
fn the_tracked_corpus_retains_its_expected_breadth() {
    let files = tracked_corpus();
    assert!(
        files.len() >= MINIMUM_CORPUS,
        "expected at least {MINIMUM_CORPUS} tracked AML documents, found {}: {:?}",
        files.len(),
        files
    );
}
