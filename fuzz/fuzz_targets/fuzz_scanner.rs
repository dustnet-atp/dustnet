#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz the scanner with arbitrary bytes.
//
// The scanner is the security boundary — it processes untrusted input from
// remote servers. This target verifies it never panics, hangs, or crashes
// regardless of input. It must gracefully reject or handle:
// - Invalid UTF-8
// - Embedded escape sequences (terminal injection attempts)
// - Pathological nesting and token counts
// - Inputs at or near size limits

fuzz_target!(|data: &[u8]| {
    let mut scanner = match dustnet_core::scanner::Scanner::new(data) {
        Ok(s) => s,
        Err(_) => return, // size/utf8 rejection is fine
    };

    // Must terminate and not panic
    let _ = scanner.scan_all();
});
