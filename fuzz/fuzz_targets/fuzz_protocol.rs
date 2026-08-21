#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz all ATP protocol message parsers with arbitrary strings.
//
// In production, message bodies arrive over TLS from remote peers. A
// malicious server (or MITM on plaintext dev connections) could send
// crafted message bodies. Every parser must handle arbitrary input
// without panicking.
//
// Also fuzzes frame header decoding with arbitrary 6-byte headers.

fuzz_target!(|data: &[u8]| {
    // Fuzz frame header decoding (needs exactly 6 bytes)
    if data.len() >= 6 {
        let mut header = [0u8; 6];
        header.copy_from_slice(&data[..6]);
        let _ = dustnet_core::protocol::frame::decode_header(&header);
    }

    // Fuzz all message parsers with the input interpreted as UTF-8
    if let Ok(body) = std::str::from_utf8(data) {
        let _ = dustnet_core::protocol::message::HelloMessage::parse(body);
        let _ = dustnet_core::protocol::message::WelcomeMessage::parse(body);
        let _ = dustnet_core::protocol::message::GetMessage::parse(body);
        let _ = dustnet_core::protocol::message::RedirectMessage::parse(body);
        let _ = dustnet_core::protocol::message::ErrorMessage::parse(body);
        let _ = dustnet_core::protocol::message::InputMessage::parse(body);
        let _ = dustnet_core::protocol::message::SubscribeMessage::parse(body);
        let _ = dustnet_core::protocol::message::UpdateMessage::parse(body);
    }
});
