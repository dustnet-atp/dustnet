#![no_main]

use dustnet_core::protocol::frame::MessageType;
use dustnet_core::protocol::state::{ConnectionStateMachine, Direction, Endpoint};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let endpoint = if data.first().is_some_and(|byte| byte & 1 == 0) {
        Endpoint::Client
    } else {
        Endpoint::Server
    };
    let mut state = ConnectionStateMachine::new(endpoint);
    for pair in data.get(1..).unwrap_or_default().chunks_exact(2) {
        let direction = if pair[0] & 1 == 0 {
            Direction::Send
        } else {
            Direction::Receive
        };
        let Ok(message) = MessageType::from_u8(pair[1]) else {
            continue;
        };
        let _ = state.apply(direction, message);
    }
});
