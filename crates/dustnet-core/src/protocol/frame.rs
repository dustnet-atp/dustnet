use super::{HEADER_SIZE, MAX_FRAME_SIZE, ProtocolError};

/// Allocate a zeroed inbound frame body without invoking an infallible,
/// remotely sized allocation.
///
/// Callers must validate the message-specific body limit before calling this
/// helper. `try_reserve_exact` makes allocator failure a recoverable protocol
/// error; the following resize cannot grow beyond the admitted capacity.
#[doc(hidden)]
pub fn allocate_frame_body(body_len: usize) -> Result<Vec<u8>, ProtocolError> {
    let mut body = Vec::new();
    body.try_reserve_exact(body_len)
        .map_err(|_| ProtocolError::ResourceExhausted {
            requested: body_len,
        })?;
    body.resize(body_len, 0);
    Ok(body)
}

/// ATP message types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    // Client → Server
    Hello = 0x01,
    Get = 0x02,
    Input = 0x03,
    Subscribe = 0x04,
    Unsubscribe = 0x05,
    Ping = 0x06,
    Bye = 0x0F,
    // Server → Client
    Welcome = 0x81,
    Page = 0x82,
    Update = 0x83,
    Redirect = 0x84,
    Error = 0x85,
    Resource = 0x86,
    Pong = 0x87,
    ServerBye = 0x8F,
}

impl MessageType {
    pub fn from_u8(code: u8) -> Result<Self, ProtocolError> {
        match code {
            0x01 => Ok(MessageType::Hello),
            0x02 => Ok(MessageType::Get),
            0x03 => Ok(MessageType::Input),
            0x04 => Ok(MessageType::Subscribe),
            0x05 => Ok(MessageType::Unsubscribe),
            0x06 => Ok(MessageType::Ping),
            0x0F => Ok(MessageType::Bye),
            0x81 => Ok(MessageType::Welcome),
            0x82 => Ok(MessageType::Page),
            0x83 => Ok(MessageType::Update),
            0x84 => Ok(MessageType::Redirect),
            0x85 => Ok(MessageType::Error),
            0x86 => Ok(MessageType::Resource),
            0x87 => Ok(MessageType::Pong),
            0x8F => Ok(MessageType::ServerBye),
            _ => Err(ProtocolError::UnknownMessageType(code)),
        }
    }
}

/// A raw ATP frame — the unit of communication on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub msg_type: MessageType,
    pub flags: u8,
    pub body: Vec<u8>,
}

/// Encode a frame into bytes ready for transmission.
///
/// Wire format: [length: u32 BE] [type: u8] [flags: u8] [body: ...]
/// Length includes the full frame (header + body).
pub fn encode_frame(frame: &RawFrame) -> Result<Vec<u8>, ProtocolError> {
    let body_len =
        u32::try_from(frame.body.len()).map_err(|_| ProtocolError::FrameTooLarge(u32::MAX))?;
    let total_len = (HEADER_SIZE as u32)
        .checked_add(body_len)
        .ok_or(ProtocolError::FrameTooLarge(u32::MAX))?;
    if total_len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(total_len));
    }

    let requested = total_len as usize;
    let mut buf = Vec::new();
    buf.try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.push(frame.msg_type as u8);
    buf.push(frame.flags);
    buf.extend_from_slice(&frame.body);
    Ok(buf)
}

/// Decode a 6-byte header into (body_length, message_type, flags).
///
/// Returns the *body* length (total - header), message type, and flags.
pub fn decode_header(header: &[u8; HEADER_SIZE]) -> Result<(u32, MessageType, u8), ProtocolError> {
    let total_len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if total_len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(total_len));
    }
    if total_len < HEADER_SIZE as u32 {
        return Err(ProtocolError::invalid_message(format_args!(
            "frame length {total_len} is smaller than header size"
        )));
    }
    let msg_type = MessageType::from_u8(header[4])?;
    let flags = header[5];
    let body_len = total_len - HEADER_SIZE as u32;
    Ok((body_len, msg_type, flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let frame = RawFrame {
            msg_type: MessageType::Hello,
            flags: 0,
            body: b"HELLO/1.0\nClient: test\n".to_vec(),
        };
        let encoded = encode_frame(&frame).unwrap();

        // Check header
        let mut header = [0u8; HEADER_SIZE];
        header.copy_from_slice(&encoded[..HEADER_SIZE]);
        let (body_len, msg_type, flags) = decode_header(&header).unwrap();

        assert_eq!(msg_type, MessageType::Hello);
        assert_eq!(flags, 0);
        assert_eq!(body_len as usize, frame.body.len());
        assert_eq!(&encoded[HEADER_SIZE..], &frame.body);
    }

    #[test]
    fn encode_decode_all_message_types() {
        let types = [
            MessageType::Hello,
            MessageType::Get,
            MessageType::Input,
            MessageType::Subscribe,
            MessageType::Unsubscribe,
            MessageType::Ping,
            MessageType::Bye,
            MessageType::Welcome,
            MessageType::Page,
            MessageType::Update,
            MessageType::Redirect,
            MessageType::Error,
            MessageType::Resource,
            MessageType::Pong,
            MessageType::ServerBye,
        ];

        for &mt in &types {
            let frame = RawFrame {
                msg_type: mt,
                flags: 0x42,
                body: b"test body".to_vec(),
            };
            let encoded = encode_frame(&frame).unwrap();
            let mut header = [0u8; HEADER_SIZE];
            header.copy_from_slice(&encoded[..HEADER_SIZE]);
            let (body_len, decoded_type, flags) = decode_header(&header).unwrap();

            assert_eq!(decoded_type, mt);
            assert_eq!(flags, 0x42);
            assert_eq!(body_len, 9);
        }
    }

    #[test]
    fn empty_body_frame() {
        let frame = RawFrame {
            msg_type: MessageType::Bye,
            flags: 0,
            body: Vec::new(),
        };
        let encoded = encode_frame(&frame).unwrap();
        assert_eq!(encoded.len(), HEADER_SIZE);

        let mut header = [0u8; HEADER_SIZE];
        header.copy_from_slice(&encoded);
        let (body_len, msg_type, _) = decode_header(&header).unwrap();
        assert_eq!(body_len, 0);
        assert_eq!(msg_type, MessageType::Bye);
    }

    #[test]
    fn inbound_body_allocation_is_zeroed_after_fallible_reservation() {
        let body = allocate_frame_body(4096).unwrap();
        assert_eq!(body.len(), 4096);
        assert!(body.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn inbound_body_capacity_overflow_is_recoverable() {
        assert!(matches!(
            allocate_frame_body(usize::MAX),
            Err(ProtocolError::ResourceExhausted {
                requested: usize::MAX
            })
        ));
    }

    #[test]
    fn frame_too_large() {
        let frame = RawFrame {
            msg_type: MessageType::Page,
            flags: 0,
            body: vec![0u8; MAX_FRAME_SIZE as usize], // body alone exceeds max
        };
        assert!(matches!(
            encode_frame(&frame),
            Err(ProtocolError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn header_too_large() {
        // Forge a header claiming a frame > MAX_FRAME_SIZE
        let big_len = MAX_FRAME_SIZE + 1;
        let mut header = [0u8; HEADER_SIZE];
        header[..4].copy_from_slice(&big_len.to_be_bytes());
        header[4] = 0x01;
        assert!(matches!(
            decode_header(&header),
            Err(ProtocolError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn header_too_small_length() {
        let mut header = [0u8; HEADER_SIZE];
        header[..4].copy_from_slice(&3u32.to_be_bytes()); // less than 6
        header[4] = 0x01;
        assert!(matches!(
            decode_header(&header),
            Err(ProtocolError::InvalidMessage(_))
        ));
    }

    #[test]
    fn unknown_message_type() {
        let mut header = [0u8; HEADER_SIZE];
        header[..4].copy_from_slice(&6u32.to_be_bytes());
        header[4] = 0xFF; // unknown
        assert!(matches!(
            decode_header(&header),
            Err(ProtocolError::UnknownMessageType(0xFF))
        ));
    }

    #[test]
    fn flags_preserved() {
        let frame = RawFrame {
            msg_type: MessageType::Page,
            flags: 0x03, // CACHEABLE | HAS_LIVE_REGIONS
            body: b"content".to_vec(),
        };
        let encoded = encode_frame(&frame).unwrap();
        let mut header = [0u8; HEADER_SIZE];
        header.copy_from_slice(&encoded[..HEADER_SIZE]);
        let (_, _, flags) = decode_header(&header).unwrap();
        assert_eq!(flags, 0x03);
    }

    #[test]
    fn length_field_is_big_endian() {
        let frame = RawFrame {
            msg_type: MessageType::Hello,
            flags: 0,
            body: vec![0u8; 300],
        };
        let encoded = encode_frame(&frame).unwrap();
        let expected_len: u32 = 306; // 6 + 300
        assert_eq!(encoded[0], (expected_len >> 24) as u8);
        assert_eq!(encoded[1], (expected_len >> 16) as u8);
        assert_eq!(encoded[2], (expected_len >> 8) as u8);
        assert_eq!(encoded[3], expected_len as u8);
    }
}
