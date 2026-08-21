#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz the ATP URI parser and path resolver.
//
// URIs come from link hrefs in remote pages and from user input on the
// command line. The parser must handle arbitrary strings (including very
// long inputs, unusual Unicode, embedded nulls-as-UTF8, etc.) without
// panicking.
//
// Also exercises path resolution (relative → absolute) which involves
// string manipulation that could panic on edge cases.

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Test direct parsing
        if let Ok(uri) = dustnet_core::protocol::uri::AtpUri::parse(s) {
            // If parsing succeeds, test resolution with the same string
            // and with various relative paths
            let _ = uri.resolve(s);
            let _ = uri.resolve("/");
            let _ = uri.resolve(".");
            let _ = uri.resolve("..");
            let _ = uri.resolve("../../../etc/passwd");

            // Round-trip: display → parse should not panic
            let displayed = uri.to_string();
            let _ = dustnet_core::protocol::uri::AtpUri::parse(&displayed);
        }
    }
});
