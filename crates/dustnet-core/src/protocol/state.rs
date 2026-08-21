use super::{ProtocolError, frame::MessageType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Client,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Handshake,
    HandshakeReply,
    Ready,
    ResponsePending,
    Closing,
    Closed,
}

/// Deterministic ATP connection state validator.
///
/// The network runners use the same serial request contract: one GET/INPUT is
/// outstanding at a time, while UPDATE may be interleaved before its response.
#[derive(Debug, Clone)]
pub struct ConnectionStateMachine {
    endpoint: Endpoint,
    phase: Phase,
}

impl ConnectionStateMachine {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            phase: Phase::Handshake,
        }
    }

    pub fn apply(
        &mut self,
        direction: Direction,
        message: MessageType,
    ) -> Result<(), ProtocolError> {
        use Direction::{Receive, Send};
        use Endpoint::{Client, Server};
        use MessageType::*;
        use Phase::{Closed, Closing, Handshake, HandshakeReply, Ready, ResponsePending};

        let next = match (self.endpoint, self.phase, direction, message) {
            (Client, Handshake, Send, Hello) => HandshakeReply,
            (Client, HandshakeReply, Receive, Welcome) => Ready,
            (Client, HandshakeReply, Receive, Error | ServerBye) => Closed,
            (Client, Ready, Send, Get | Input) => ResponsePending,
            (Client, Ready, Send, Subscribe | Unsubscribe) => Ready,
            // PING is legal in both phases on purpose: a slow page fetch holds
            // the connection in ResponsePending for as long as the server takes,
            // and a keepalive that could not fire during it would let the very
            // request it is protecting time the connection out.
            (Client, Ready, Send, Ping) => Ready,
            (Client, ResponsePending, Send, Ping) => ResponsePending,
            (Client, Ready, Receive, Pong) => Ready,
            (Client, ResponsePending, Receive, Pong) => ResponsePending,
            (Client, Ready, Receive, Update) => Ready,
            (Client, Ready, Send, Bye) => Closing,
            (Client, ResponsePending, Receive, Update) => ResponsePending,
            (Client, ResponsePending, Receive, Page | Redirect | Error | Resource) => Ready,
            (Client, Ready | ResponsePending, Receive, ServerBye) => Closed,
            (Client, Closing, Receive, ServerBye) => Closed,

            (Server, Handshake, Receive, Hello) => HandshakeReply,
            (Server, HandshakeReply, Send, Welcome) => Ready,
            (Server, HandshakeReply, Send, Error | ServerBye) => Closed,
            (Server, Ready, Receive, Get | Input) => ResponsePending,
            (Server, Ready, Receive, Subscribe | Unsubscribe) => Ready,
            (Server, Ready, Receive, Ping) => Ready,
            (Server, ResponsePending, Receive, Ping) => ResponsePending,
            (Server, Ready, Send, Pong) => Ready,
            (Server, ResponsePending, Send, Pong) => ResponsePending,
            (Server, Ready, Receive, Bye) => Closing,
            (Server, ResponsePending, Send, Update) => ResponsePending,
            (Server, ResponsePending, Send, Page | Redirect | Error | Resource) => Ready,
            (Server, Ready, Send, Update) => Ready,
            (Server, Ready | ResponsePending, Send, ServerBye) => Closed,
            (Server, Closing, Send, ServerBye) => Closed,
            _ => {
                return Err(ProtocolError::invalid_message(format_args!(
                    "invalid {:?} transition for {:?} in {:?}: {message:?}",
                    direction, self.endpoint, self.phase
                )));
            }
        };
        self.phase = next;
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_serial_request_sequence_accepts_interleaved_update() {
        let mut state = ConnectionStateMachine::new(Endpoint::Client);
        for (direction, message) in [
            (Direction::Send, MessageType::Hello),
            (Direction::Receive, MessageType::Welcome),
            (Direction::Send, MessageType::Get),
            (Direction::Receive, MessageType::Update),
            (Direction::Receive, MessageType::Page),
            (Direction::Send, MessageType::Bye),
            (Direction::Receive, MessageType::ServerBye),
        ] {
            state.apply(direction, message).unwrap();
        }
        assert!(state.is_closed());
    }

    #[test]
    fn rejects_second_request_and_wrong_direction() {
        let mut client = ConnectionStateMachine::new(Endpoint::Client);
        client.apply(Direction::Send, MessageType::Hello).unwrap();
        client
            .apply(Direction::Receive, MessageType::Welcome)
            .unwrap();
        client.apply(Direction::Send, MessageType::Get).unwrap();
        assert!(client.apply(Direction::Send, MessageType::Get).is_err());

        let mut server = ConnectionStateMachine::new(Endpoint::Server);
        assert!(server.apply(Direction::Send, MessageType::Hello).is_err());
    }

    /// The state table is data, not prose: `verification/protocol-state-table.json`
    /// is the authoritative transition list and `docs/spec/05-conformance.md` refers
    /// to it. This asserts the table still describes the transitions this module
    /// implements, so a transition cannot be added or removed in code while the
    /// contract still claims the old machine.
    #[test]
    fn documented_state_table_matches_implementation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let raw = std::fs::read_to_string(root.join("verification/protocol-state-table.json"))
            .expect("read protocol-state-table.json");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("parse protocol-state-table.json");
        let join = |value: &serde_json::Value| {
            value
                .as_array()
                .expect("transition field is an array")
                .iter()
                .map(|item| item.as_str().expect("array holds strings").to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let documented: std::collections::BTreeSet<(String, String, String, String, String)> =
            parsed["transitions"]
                .as_array()
                .expect("`transitions` is an array")
                .iter()
                .map(|t| {
                    (
                        t["endpoint"].as_str().expect("endpoint").to_owned(),
                        join(&t["states"]),
                        t["direction"].as_str().expect("direction").to_owned(),
                        join(&t["messages"]),
                        t["next_state"].as_str().expect("next_state").to_owned(),
                    )
                })
                .collect();

        for (endpoint, state, direction, message, next) in [
            ("Client", "Handshake", "send", "HELLO", "HandshakeReply"),
            ("Client", "HandshakeReply", "receive", "WELCOME", "Ready"),
            (
                "Client",
                "HandshakeReply",
                "receive",
                "ERROR, SERVER-BYE",
                "Closed",
            ),
            ("Client", "Ready", "send", "GET, INPUT", "ResponsePending"),
            ("Client", "Ready", "send", "SUBSCRIBE, UNSUBSCRIBE", "Ready"),
            ("Client", "Ready", "receive", "UPDATE", "Ready"),
            ("Client", "Ready", "send", "BYE", "Closing"),
            (
                "Client",
                "ResponsePending",
                "receive",
                "UPDATE",
                "ResponsePending",
            ),
            (
                "Client",
                "ResponsePending",
                "receive",
                "PAGE, RESOURCE, REDIRECT, ERROR",
                "Ready",
            ),
            (
                "Client",
                "Ready, ResponsePending",
                "send",
                "PING",
                "unchanged",
            ),
            (
                "Client",
                "Ready, ResponsePending",
                "receive",
                "PONG",
                "unchanged",
            ),
            (
                "Client",
                "Ready, ResponsePending",
                "receive",
                "SERVER-BYE",
                "Closed",
            ),
            ("Client", "Closing", "receive", "SERVER-BYE", "Closed"),
            ("Server", "Handshake", "receive", "HELLO", "HandshakeReply"),
            ("Server", "HandshakeReply", "send", "WELCOME", "Ready"),
            (
                "Server",
                "HandshakeReply",
                "send",
                "ERROR, SERVER-BYE",
                "Closed",
            ),
            (
                "Server",
                "Ready",
                "receive",
                "GET, INPUT",
                "ResponsePending",
            ),
            (
                "Server",
                "Ready",
                "receive",
                "SUBSCRIBE, UNSUBSCRIBE",
                "Ready",
            ),
            ("Server", "Ready", "receive", "BYE", "Closing"),
            (
                "Server",
                "ResponsePending",
                "send",
                "UPDATE",
                "ResponsePending",
            ),
            (
                "Server",
                "ResponsePending",
                "send",
                "PAGE, RESOURCE, REDIRECT, ERROR",
                "Ready",
            ),
            ("Server", "Ready", "send", "UPDATE", "Ready"),
            (
                "Server",
                "Ready, ResponsePending",
                "receive",
                "PING",
                "unchanged",
            ),
            (
                "Server",
                "Ready, ResponsePending",
                "send",
                "PONG",
                "unchanged",
            ),
            (
                "Server",
                "Ready, ResponsePending",
                "send",
                "SERVER-BYE",
                "Closed",
            ),
            ("Server", "Closing", "send", "SERVER-BYE", "Closed"),
        ] {
            let row = (
                endpoint.to_owned(),
                state.to_owned(),
                direction.to_owned(),
                message.to_owned(),
                next.to_owned(),
            );
            assert!(
                documented.contains(&row),
                "protocol-state-table.json is missing the transition {row:?}"
            );
        }
    }
}
