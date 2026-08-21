#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz the full scan → parse → validate pipeline with arbitrary bytes.
//
// This exercises component expansion, structural validation, and trigger
// validation. The parser must never panic — it returns diagnostics for
// all error conditions. This target catches bugs in:
// - Recursive descent parsing with error recovery
// - Component macro expansion (token substitution, slot mapping)
// - Depth and element count limits
// - Trigger reference validation

fuzz_target!(|data: &[u8]| {
    let mut scanner = match dustnet_core::scanner::Scanner::new(data) {
        Ok(s) => s,
        Err(_) => return,
    };

    let tokens = match scanner.scan_all() {
        Ok(t) => t,
        Err(_) => return,
    };

    // parse() includes component expansion and trigger validation.
    // Must never panic — errors are returned as diagnostics.
    let _result = dustnet_core::parser::parse(tokens);
});
