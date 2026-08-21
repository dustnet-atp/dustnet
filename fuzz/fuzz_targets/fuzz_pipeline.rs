#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz the full pipeline: bytes → scan → parse → build scene → layout.
//
// This is the exact code path that runs when a client receives a page from
// a remote server. If any stage panics on adversarial input, a malicious
// server can crash the client.
//
// The layout engine is exercised at a fixed 80x24 terminal size with
// truecolor support — the most common real-world configuration.

fuzz_target!(|data: &[u8]| {
    let mut scanner = match dustnet_core::scanner::Scanner::new(data) {
        Ok(s) => s,
        Err(_) => return,
    };

    let tokens = match scanner.scan_all() {
        Ok(t) => t,
        Err(_) => return,
    };

    let result = dustnet_core::parser::parse(tokens);

    if let Some(ref doc) = result.document {
        // Build the scene, then lay it out at a standard terminal size.
        // Must not panic even on degenerate documents (deeply nested,
        // zero-width, huge element counts near the limit, etc.)
        let mut scene = dustnet::compositor::scene::build::from_document(doc);
        let _ = dustnet::compositor::layout::engine::layout_scene(
            &mut scene,
            80,
            24,
            dustnet_core::color::ColorSupport::Truecolor,
            dustnet::compositor::layout::text::WidthConfig::default(),
        );
    }
});
