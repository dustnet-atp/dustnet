use super::frame::MessageType;
use super::{
    MAX_CONTROL_MESSAGE_SIZE, MAX_INPUT_MESSAGE_SIZE, MAX_LIVE_UPDATE_SIZE, MAX_PAGE_MESSAGE_SIZE,
    MAX_PAGE_PATH_LEN, ProtocolError,
};
use crate::session::{MAX_SCOPE_LEN, MAX_TOKEN_LEN, SessionDirective};

fn checked_add_size(size: usize, extra: usize) -> Result<usize, ProtocolError> {
    size.checked_add(extra)
        .ok_or(ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        })
}

fn enforce_message_limit(
    msg_type: MessageType,
    requested: usize,
    max: usize,
) -> Result<(), ProtocolError> {
    if requested > max {
        return Err(ProtocolError::MessageTooLarge {
            msg_type,
            size: u32::try_from(requested).unwrap_or(u32::MAX),
            max,
        });
    }
    Ok(())
}

fn try_string_with_capacity(requested: usize) -> Result<String, ProtocolError> {
    let mut value = String::new();
    value
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    Ok(value)
}

fn try_owned_string(value: &str) -> Result<String, ProtocolError> {
    let mut owned = try_string_with_capacity(value.len())?;
    owned.push_str(value);
    Ok(owned)
}

fn decimal_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 10 {
        value /= 10;
        len += 1;
    }
    len
}

fn checked_string_lengths<'a>(
    size: usize,
    values: impl IntoIterator<Item = &'a String>,
) -> Result<usize, ProtocolError> {
    values
        .into_iter()
        .try_fold(size, |size, value| checked_add_size(size, value.len()))
}

fn push_decimal(bytes: &mut Vec<u8>, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        // `u64::MAX` is twenty digits, so the loop cannot outrun the buffer.
        if let Some(slot) = digits.get_mut(start) {
            *slot = b'0' + (value % 10) as u8;
        }
        value /= 10;
        if value == 0 {
            break;
        }
    }
    bytes.extend_from_slice(digits.get(start..).unwrap_or(&[]));
}

/// Record that `index` has been seen, reporting `false` if it already was.
///
/// `index` comes from a fixed `match` over a message type's known field names
/// and `seen` is sized to that set, so the lookup always resolves. Going
/// through `get_mut` keeps that checked at each call site rather than restating
/// the argument five times; an out-of-range index reports a duplicate, which
/// rejects the message rather than admitting an unchecked field.
fn mark_seen(seen: &mut [bool], index: usize) -> bool {
    match seen.get_mut(index) {
        Some(slot) if *slot => false,
        Some(slot) => {
            *slot = true;
            true
        }
        None => false,
    }
}

fn valid_session_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= MAX_SCOPE_LEN
        && scope.starts_with('/')
        && !scope.chars().any(char::is_control)
        && !scope.contains(['?', '#'])
        && !scope
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

fn try_parse_session_directive(
    key: &str,
    value: &str,
) -> Result<Option<SessionDirective>, ProtocolError> {
    match key {
        "Set-Session" => {
            let mut parts = value.splitn(3, ' ');
            let Some(token) = parts.next() else {
                return Ok(None);
            };
            let scope = parts.next().unwrap_or("/");
            let expires = parts.next().and_then(|part| part.parse::<u64>().ok());
            if token.is_empty() || token.len() > MAX_TOKEN_LEN || !valid_session_scope(scope) {
                return Ok(None);
            }
            Ok(Some(SessionDirective::Set {
                token: try_owned_string(token)?,
                scope: try_owned_string(scope)?,
                expires,
            }))
        }
        "Clear-Session" => {
            let scope = value.trim();
            if !valid_session_scope(scope) {
                return Ok(None);
            }
            Ok(Some(SessionDirective::Clear {
                scope: try_owned_string(scope)?,
            }))
        }
        _ => Ok(None),
    }
}

/// Client HELLO message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloMessage {
    pub protocol_version: String,
    pub terminal_size: Option<String>,
    pub color_support: Option<String>,
    pub client: Option<String>,
    pub capabilities: Vec<String>,
}

impl HelloMessage {
    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Hello, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
        let mut lines = body.lines();
        let first = lines
            .next()
            .ok_or_else(|| ProtocolError::InvalidMessage("empty HELLO body".into()))?;
        let Some(stripped) = first.strip_prefix("HELLO/") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected HELLO/, got: {first}"
            )));
        };
        let protocol_version = try_owned_string(stripped)?;

        let mut msg = HelloMessage {
            protocol_version,
            terminal_size: None,
            color_support: None,
            client: None,
            capabilities: Vec::new(),
        };

        let mut seen_terminal_size = false;
        let mut seen_color_support = false;
        let mut seen_client = false;
        let mut seen_capabilities = false;
        for line in lines {
            let (key, value) = parse_control_field(line)?;
            match key {
                "Terminal-Size" if !seen_terminal_size => {
                    seen_terminal_size = true;
                    msg.terminal_size = Some(try_owned_string(value)?);
                }
                "Color-Support" if !seen_color_support => {
                    seen_color_support = true;
                    msg.color_support = Some(try_owned_string(value)?);
                }
                "Client" if !seen_client => {
                    seen_client = true;
                    msg.client = Some(try_owned_string(value)?);
                }
                "Capabilities" if !seen_capabilities => {
                    seen_capabilities = true;
                    msg.capabilities = parse_capabilities(value)?;
                }
                "Terminal-Size" | "Color-Support" | "Client" | "Capabilities" => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "duplicate HELLO field: {key}"
                    )));
                }
                _ => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "unknown HELLO field: {key}"
                    )));
                }
            }
        }

        Ok(msg)
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        validate_capabilities(&self.capabilities)?;
        let mut requested = checked_add_size("HELLO/\n".len(), self.protocol_version.len())?;
        for (prefix, value) in [
            ("Terminal-Size: ", self.terminal_size.as_deref()),
            ("Color-Support: ", self.color_support.as_deref()),
            ("Client: ", self.client.as_deref()),
        ] {
            if let Some(value) = value {
                requested = checked_add_size(requested, prefix.len())?;
                requested = checked_add_size(requested, value.len())?;
                requested = checked_add_size(requested, 1)?;
            }
        }
        if !self.capabilities.is_empty() {
            requested = checked_add_size(requested, "Capabilities: \n".len())?;
            requested = checked_string_lengths(requested, &self.capabilities)?;
            requested = checked_add_size(requested, self.capabilities.len() - 1)?;
        }
        enforce_message_limit(MessageType::Hello, requested, MAX_CONTROL_MESSAGE_SIZE)?;
        let mut s = try_string_with_capacity(requested)?;
        s.push_str("HELLO/");
        s.push_str(&self.protocol_version);
        s.push('\n');
        if let Some(ref ts) = self.terminal_size {
            s.push_str("Terminal-Size: ");
            s.push_str(ts);
            s.push('\n');
        }
        if let Some(ref cs) = self.color_support {
            s.push_str("Color-Support: ");
            s.push_str(cs);
            s.push('\n');
        }
        if let Some(ref c) = self.client {
            s.push_str("Client: ");
            s.push_str(c);
            s.push('\n');
        }
        if !self.capabilities.is_empty() {
            s.push_str("Capabilities: ");
            for (index, capability) in self.capabilities.iter().enumerate() {
                if index != 0 {
                    s.push(',');
                }
                s.push_str(capability);
            }
            s.push('\n');
        }
        Ok(s)
    }
}

/// Server WELCOME message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeMessage {
    pub protocol_version: String,
    pub server: Option<String>,
    pub site_name: Option<String>,
    pub capabilities: Vec<String>,
}

impl WelcomeMessage {
    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Welcome, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
        let mut lines = body.lines();
        let first = lines
            .next()
            .ok_or_else(|| ProtocolError::InvalidMessage("empty WELCOME body".into()))?;
        let Some(stripped) = first.strip_prefix("WELCOME/") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected WELCOME/, got: {first}"
            )));
        };
        let protocol_version = try_owned_string(stripped)?;

        let mut msg = WelcomeMessage {
            protocol_version,
            server: None,
            site_name: None,
            capabilities: Vec::new(),
        };

        let mut seen_server = false;
        let mut seen_site_name = false;
        let mut seen_capabilities = false;
        for line in lines {
            let (key, value) = parse_control_field(line)?;
            match key {
                "Server" if !seen_server => {
                    seen_server = true;
                    msg.server = Some(try_owned_string(value)?);
                }
                "Site-Name" if !seen_site_name => {
                    seen_site_name = true;
                    msg.site_name = Some(try_owned_string(value)?);
                }
                "Capabilities" if !seen_capabilities => {
                    seen_capabilities = true;
                    msg.capabilities = parse_capabilities(value)?;
                }
                "Server" | "Site-Name" | "Capabilities" => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "duplicate WELCOME field: {key}"
                    )));
                }
                _ => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "unknown WELCOME field: {key}"
                    )));
                }
            }
        }

        Ok(msg)
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        validate_capabilities(&self.capabilities)?;
        let mut requested = checked_add_size("WELCOME/\n".len(), self.protocol_version.len())?;
        for (prefix, value) in [
            ("Server: ", self.server.as_deref()),
            ("Site-Name: ", self.site_name.as_deref()),
        ] {
            if let Some(value) = value {
                requested = checked_add_size(requested, prefix.len())?;
                requested = checked_add_size(requested, value.len())?;
                requested = checked_add_size(requested, 1)?;
            }
        }
        if !self.capabilities.is_empty() {
            requested = checked_add_size(requested, "Capabilities: \n".len())?;
            requested = checked_string_lengths(requested, &self.capabilities)?;
            requested = checked_add_size(requested, self.capabilities.len() - 1)?;
        }
        enforce_message_limit(MessageType::Welcome, requested, MAX_CONTROL_MESSAGE_SIZE)?;
        let mut s = try_string_with_capacity(requested)?;
        s.push_str("WELCOME/");
        s.push_str(&self.protocol_version);
        s.push('\n');
        if let Some(ref sv) = self.server {
            s.push_str("Server: ");
            s.push_str(sv);
            s.push('\n');
        }
        if let Some(ref sn) = self.site_name {
            s.push_str("Site-Name: ");
            s.push_str(sn);
            s.push('\n');
        }
        if !self.capabilities.is_empty() {
            s.push_str("Capabilities: ");
            for (index, capability) in self.capabilities.iter().enumerate() {
                if index != 0 {
                    s.push(',');
                }
                s.push_str(capability);
            }
            s.push('\n');
        }
        Ok(s)
    }
}

fn parse_control_field(line: &str) -> Result<(&str, &str), ProtocolError> {
    if line.is_empty() || line.chars().any(char::is_control) {
        return Err(ProtocolError::InvalidMessage(
            "empty or control-containing metadata field".into(),
        ));
    }
    let (key, value) = line
        .split_once(": ")
        .ok_or_else(|| ProtocolError::invalid_message(format_args!("malformed field: {line}")))?;
    if key.is_empty() || value.is_empty() {
        return Err(ProtocolError::invalid_message(format_args!(
            "malformed field: {line}"
        )));
    }
    Ok((key, value))
}

/// Whether `value` may be a PAGE's `Path`.
///
/// An absolute path on the sending site, with an optional query. The three
/// refusals each close a way of turning a relabelling into a navigation:
///
/// - a leading `//` is a protocol-relative reference, and resolving it would
///   move the client to whatever host followed — a cross-origin redirect that
///   skips the redirect limit and the fresh HELLO a real one performs;
/// - a scheme is the same escape spelled differently, and is excluded by
///   requiring the first byte to be `/`;
/// - a fragment has no meaning in ATP, so accepting one would invent syntax the
///   grammar does not have.
fn valid_page_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('#')
        && !value.chars().any(char::is_control)
}

fn validate_primary_value(label: &str, value: &str) -> Result<(), ProtocolError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ProtocolError::invalid_message(format_args!(
            "{label} is empty or contains controls"
        )));
    }
    Ok(())
}

fn parse_capabilities(value: &str) -> Result<Vec<String>, ProtocolError> {
    let count = value.split(',').count();
    if count > 32 {
        return Err(ProtocolError::InvalidMessage(
            "invalid capability list".into(),
        ));
    }
    let mut capabilities = Vec::new();
    capabilities
        .try_reserve_exact(count)
        .map_err(|_| ProtocolError::ResourceExhausted {
            requested: count.saturating_mul(std::mem::size_of::<String>()),
        })?;
    for capability in value.split(',') {
        if capability.is_empty()
            || !capability
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        {
            return Err(ProtocolError::InvalidMessage(
                "invalid capability list".into(),
            ));
        }
        capabilities.push(try_owned_string(capability)?);
    }
    Ok(capabilities)
}

fn validate_capabilities(capabilities: &[String]) -> Result<(), ProtocolError> {
    if capabilities.len() > 32
        || capabilities.iter().any(|capability| {
            capability.is_empty()
                || !capability
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        })
    {
        return Err(ProtocolError::InvalidMessage(
            "invalid capability list".into(),
        ));
    }
    Ok(())
}

/// Client GET message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetMessage {
    pub path: String,
    pub query: Option<String>,
    pub referrer: Option<String>,
    /// Session token for the matching scope (if any).
    pub session: Option<String>,
}

impl GetMessage {
    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Get, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
        let mut lines = body.lines();
        let first = lines
            .next()
            .ok_or_else(|| ProtocolError::InvalidMessage("empty GET body".into()))?;
        let Some(stripped) = first.strip_prefix("GET ") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected GET path, got: {first}"
            )));
        };
        let path = try_owned_string(stripped)?;
        validate_primary_value("GET path", &path)?;

        let mut msg = GetMessage {
            path,
            query: None,
            referrer: None,
            session: None,
        };

        let mut seen_query = false;
        let mut seen_referrer = false;
        let mut seen_session = false;
        for line in lines {
            let (key, value) = parse_control_field(line)?;
            match key {
                "Query" if !seen_query => {
                    seen_query = true;
                    msg.query = Some(try_owned_string(value)?);
                }
                "Referrer" if !seen_referrer => {
                    seen_referrer = true;
                    msg.referrer = Some(try_owned_string(value)?);
                }
                "Session" if !seen_session => {
                    seen_session = true;
                    msg.session = Some(try_owned_string(value)?);
                }
                "Query" | "Referrer" | "Session" => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "duplicate GET field: {key}"
                    )));
                }
                _ => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "unknown GET field: {key}"
                    )));
                }
            }
        }

        Ok(msg)
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        let mut requested = checked_add_size("GET \n".len(), self.path.len())?;
        for (prefix, value) in [
            ("Query: ", self.query.as_deref()),
            ("Referrer: ", self.referrer.as_deref()),
            ("Session: ", self.session.as_deref()),
        ] {
            if let Some(value) = value {
                requested = checked_add_size(requested, prefix.len())?;
                requested = checked_add_size(requested, value.len() + 1)?;
            }
        }
        enforce_message_limit(MessageType::Get, requested, MAX_CONTROL_MESSAGE_SIZE)?;
        let mut s = try_string_with_capacity(requested)?;
        s.push_str("GET ");
        s.push_str(&self.path);
        s.push('\n');
        if let Some(ref q) = self.query {
            s.push_str("Query: ");
            s.push_str(q);
            s.push('\n');
        }
        if let Some(ref r) = self.referrer {
            s.push_str("Referrer: ");
            s.push_str(r);
            s.push('\n');
        }
        if let Some(ref t) = self.session {
            s.push_str("Session: ");
            s.push_str(t);
            s.push('\n');
        }
        Ok(s)
    }
}

/// PAGE response flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageFlags {
    pub cacheable: bool,
    pub has_live_regions: bool,
    /// The metadata block carries a `Path` naming the page this body is.
    pub has_path: bool,
    pub has_session: bool,
}

impl PageFlags {
    pub fn from_bits(flags: u8) -> Self {
        PageFlags {
            cacheable: flags & 0x01 != 0,
            has_live_regions: flags & 0x02 != 0,
            has_path: flags & 0x04 != 0,
            has_session: flags & 0x08 != 0,
        }
    }

    pub fn to_bits(self) -> u8 {
        let mut bits = 0u8;
        if self.cacheable {
            bits |= 0x01;
        }
        if self.has_live_regions {
            bits |= 0x02;
        }
        if self.has_path {
            bits |= 0x04;
        }
        if self.has_session {
            bits |= 0x08;
        }
        bits
    }
}

/// Server PAGE message — the body is AML content, optionally preceded by a
/// metadata block.
///
/// When `flags.has_path` or `flags.has_session` is set, the frame body starts
/// with metadata lines before a `\n\n` separator, then AML content. When
/// neither is set, the entire body is AML content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMessage {
    pub content: String,
    pub flags: PageFlags,
    /// The path this body *is*, when the server chose to say.
    ///
    /// A response is not always to the path that was asked for: an accepted
    /// `INPUT` is answered with the page the handler named, which is how a
    /// login lands on the front page. Without this the client has no way to
    /// learn that, so it keeps the submitted path as its location and a reload
    /// puts the person back on the login form.
    ///
    /// Advisory and same-origin only. It names a path on the site that sent it
    /// and never a URI, so it can relabel where you are within a site but
    /// cannot move you to another one — that is what `REDIRECT` is for, and it
    /// is subject to the redirect limit this deliberately is not.
    pub path: Option<String>,
    /// Session directives from the server (Set-Session, Clear-Session).
    pub session_directives: Vec<SessionDirective>,
}

impl PageMessage {
    /// Encode the PAGE body for the wire.
    /// If a path or session directives are present, prepends metadata before
    /// `\n\n`.
    pub fn encode_body(&self) -> Result<(Vec<u8>, u8), ProtocolError> {
        let mut flags = self.flags;
        if self.session_directives.is_empty() && self.path.is_none() {
            // Cleared, not trusted: a caller that set the flag by hand without
            // supplying the field would otherwise frame a body its own decoder
            // refuses. The fields are what decide the flags, everywhere.
            flags.has_session = false;
            flags.has_path = false;
            enforce_message_limit(MessageType::Page, self.content.len(), MAX_PAGE_MESSAGE_SIZE)?;
            let mut body = Vec::new();
            body.try_reserve_exact(self.content.len()).map_err(|_| {
                ProtocolError::ResourceExhausted {
                    requested: self.content.len(),
                }
            })?;
            body.extend_from_slice(self.content.as_bytes());
            return Ok((body, flags.to_bits()));
        }

        flags.has_session = !self.session_directives.is_empty();
        flags.has_path = self.path.is_some();
        let mut requested = checked_add_size(1, self.content.len())?;
        if let Some(path) = &self.path {
            // Refused at encode time rather than truncated: a path is what the
            // client will call this page, and half a path is a different page.
            if path.len() > MAX_PAGE_PATH_LEN || !valid_page_path(path) {
                return Err(ProtocolError::InvalidMessage(
                    "PAGE Path is oversized or not an absolute same-origin path".into(),
                ));
            }
            requested = checked_add_size(requested, "Path: \n".len())?;
            requested = checked_add_size(requested, path.len())?;
        }
        for directive in &self.session_directives {
            requested = checked_add_size(
                requested,
                match directive {
                    SessionDirective::Set {
                        token,
                        scope,
                        expires,
                    } => {
                        let mut len = checked_add_size("Set-Session:  \n".len(), token.len())?;
                        len = checked_add_size(len, scope.len())?;
                        if let Some(expires) = expires {
                            len = checked_add_size(len, 1 + decimal_len(*expires))?;
                        }
                        len
                    }
                    SessionDirective::Clear { scope } => {
                        checked_add_size("Clear-Session: \n".len(), scope.len())?
                    }
                },
            )?;
        }
        enforce_message_limit(MessageType::Page, requested, MAX_PAGE_MESSAGE_SIZE)?;
        let mut body = Vec::new();
        body.try_reserve_exact(requested)
            .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
        // Path first, so the block has one canonical order and a test can
        // compare bytes rather than parse them back.
        if let Some(path) = &self.path {
            body.extend_from_slice(b"Path: ");
            body.extend_from_slice(path.as_bytes());
            body.push(b'\n');
        }
        for directive in &self.session_directives {
            match directive {
                SessionDirective::Set {
                    token,
                    scope,
                    expires,
                } => {
                    body.extend_from_slice(b"Set-Session: ");
                    body.extend_from_slice(token.as_bytes());
                    body.push(b' ');
                    body.extend_from_slice(scope.as_bytes());
                    if let Some(expires) = expires {
                        body.push(b' ');
                        push_decimal(&mut body, *expires);
                    }
                    body.push(b'\n');
                }
                SessionDirective::Clear { scope } => {
                    body.extend_from_slice(b"Clear-Session: ");
                    body.extend_from_slice(scope.as_bytes());
                    body.push(b'\n');
                }
            }
        }
        body.push(b'\n');
        body.extend_from_slice(self.content.as_bytes());
        Ok((body, flags.to_bits()))
    }

    /// Decode a PAGE frame body, extracting the metadata block if present.
    pub fn decode_body(body: &[u8], flags_byte: u8) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Page, body.len(), MAX_PAGE_MESSAGE_SIZE)?;
        let flags = PageFlags::from_bits(flags_byte);
        let body_str = std::str::from_utf8(body)
            .map_err(|e| ProtocolError::invalid_message(format_args!("invalid UTF-8: {e}")))?;

        if !flags.has_session && !flags.has_path {
            return Ok(PageMessage {
                content: try_owned_string(body_str)?,
                flags,
                path: None,
                session_directives: Vec::new(),
            });
        }

        // Split metadata from content at the first blank line
        let (metadata, content) = match body_str.find("\n\n") {
            Some(pos) => (&body_str[..pos], &body_str[pos + 2..]),
            None => {
                return Err(ProtocolError::InvalidMessage(
                    "PAGE metadata flag requires metadata separator".into(),
                ));
            }
        };

        let directive_count = metadata.lines().count();
        let mut directives = Vec::new();
        directives.try_reserve_exact(directive_count).map_err(|_| {
            ProtocolError::ResourceExhausted {
                requested: directive_count.saturating_mul(std::mem::size_of::<SessionDirective>()),
            }
        })?;
        let mut path = None;
        for line in metadata.lines() {
            let (key, value) = parse_control_field(line)?;
            if key == "Path" {
                // A singleton, rejected on repeat rather than last-one-wins:
                // which of two paths survives is a difference an attacker picks
                // and a reviewer does not see.
                if path.is_some() {
                    return Err(ProtocolError::InvalidMessage(
                        "duplicate PAGE Path field".into(),
                    ));
                }
                if !flags.has_path {
                    return Err(ProtocolError::InvalidMessage(
                        "PAGE Path without the path flag".into(),
                    ));
                }
                if value.len() > MAX_PAGE_PATH_LEN {
                    return Err(ProtocolError::InvalidMessage("PAGE Path too long".into()));
                }
                if !valid_page_path(value) {
                    return Err(ProtocolError::InvalidMessage(
                        "PAGE Path must be an absolute same-origin path".into(),
                    ));
                }
                path = Some(try_owned_string(value)?);
                continue;
            }
            if !matches!(key, "Set-Session" | "Clear-Session") {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown PAGE metadata field: {key}"
                )));
            }
            let directive = try_parse_session_directive(key, value)?;
            if let Some(directive) = directive {
                directives.push(directive);
            } else {
                return Err(ProtocolError::invalid_message(format_args!(
                    "invalid PAGE metadata field: {key}"
                )));
            }
        }
        // The flag promises a field. Accepting the flag with no field would make
        // two encodings of the same message, and a differential fuzzer finds
        // that before a reviewer does.
        if flags.has_path && path.is_none() {
            return Err(ProtocolError::InvalidMessage(
                "PAGE path flag without a Path field".into(),
            ));
        }
        if flags.has_session && directives.is_empty() {
            return Err(ProtocolError::InvalidMessage(
                "PAGE session flag without a session directive".into(),
            ));
        }

        Ok(PageMessage {
            content: try_owned_string(content)?,
            flags,
            path,
            session_directives: directives,
        })
    }
}

/// Server REDIRECT message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectMessage {
    pub code: u16,
    pub target: String,
}

impl RedirectMessage {
    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Redirect, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
        let first = body
            .lines()
            .next()
            .ok_or_else(|| ProtocolError::InvalidMessage("empty REDIRECT body".into()))?;
        let Some(stripped) = first.strip_prefix("REDIRECT ") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected REDIRECT, got: {first}"
            )));
        };
        let rest = stripped;
        let (code_str, target) = rest
            .split_once(' ')
            .ok_or_else(|| ProtocolError::InvalidMessage("REDIRECT missing target".into()))?;
        let code = parse_redirect_code(code_str)?;
        validate_primary_value("REDIRECT target", target)?;
        if body.lines().count() != 1 {
            return Err(ProtocolError::InvalidMessage(
                "REDIRECT must not contain metadata".into(),
            ));
        }
        Ok(RedirectMessage {
            code,
            target: try_owned_string(target)?,
        })
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        let requested = checked_add_size(
            "REDIRECT  \n".len() + decimal_len(self.code.into()),
            self.target.len(),
        )?;
        enforce_message_limit(MessageType::Redirect, requested, MAX_CONTROL_MESSAGE_SIZE)?;
        let mut body = try_string_with_capacity(requested)?;
        use std::fmt::Write as _;
        let _ = write!(&mut body, "REDIRECT {} ", self.code);
        body.push_str(&self.target);
        body.push('\n');
        Ok(body)
    }
}

/// Server ERROR message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorMessage {
    pub code: u16,
    pub message: Option<String>,
}

impl ErrorMessage {
    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Error, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
        let mut lines = body.lines();
        let first = lines
            .next()
            .ok_or_else(|| ProtocolError::InvalidMessage("empty ERROR body".into()))?;
        let Some(stripped) = first.strip_prefix("ERROR ") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected ERROR code, got: {first}"
            )));
        };
        let code = parse_error_code(stripped)?;

        let mut message = None;
        let mut seen_message = false;
        for line in lines {
            let (key, value) = parse_control_field(line)?;
            if key != "Message" {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown ERROR field: {key}"
                )));
            }
            if seen_message {
                return Err(ProtocolError::InvalidMessage(
                    "duplicate ERROR field: Message".into(),
                ));
            }
            seen_message = true;
            message = Some(try_owned_string(value)?);
        }

        Ok(ErrorMessage { code, message })
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        let mut requested = "ERROR \n".len() + decimal_len(self.code.into());
        if let Some(message) = &self.message {
            requested = checked_add_size(requested, "Message: \n".len())?;
            requested = checked_add_size(requested, message.len())?;
        }
        enforce_message_limit(MessageType::Error, requested, MAX_CONTROL_MESSAGE_SIZE)?;
        let mut s = try_string_with_capacity(requested)?;
        use std::fmt::Write as _;
        let _ = writeln!(&mut s, "ERROR {}", self.code);
        if let Some(ref m) = self.message {
            s.push_str("Message: ");
            s.push_str(m);
            s.push('\n');
        }
        Ok(s)
    }
}

/// Client INPUT message — form submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub path: String,
    pub form_data: String,
    /// Session token for the matching scope (if any).
    pub session: Option<String>,
}

impl InputMessage {
    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Input, body.len(), MAX_INPUT_MESSAGE_SIZE)?;
        let mut lines = body.lines();
        let first = lines
            .next()
            .ok_or_else(|| ProtocolError::InvalidMessage("empty INPUT body".into()))?;
        let Some(stripped) = first.strip_prefix("INPUT ") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected INPUT path, got: {first}"
            )));
        };
        let path = try_owned_string(stripped)?;
        validate_primary_value("INPUT path", &path)?;

        let mut form_data = String::new();
        let mut session = None;
        let mut seen_form = false;
        let mut seen_session = false;
        for line in lines {
            let (key, value) = parse_control_field(line)?;
            match key {
                "Form" if !seen_form => {
                    seen_form = true;
                    form_data = try_owned_string(value)?;
                }
                "Session" if !seen_session => {
                    seen_session = true;
                    session = Some(try_owned_string(value)?);
                }
                "Form" | "Session" => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "duplicate INPUT field: {key}"
                    )));
                }
                _ => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "unknown INPUT field: {key}"
                    )));
                }
            }
        }

        Ok(InputMessage {
            path,
            form_data,
            session,
        })
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        let mut requested = checked_add_size("INPUT \n".len(), self.path.len())?;
        if !self.form_data.is_empty() {
            requested = checked_add_size(requested, "Form: \n".len())?;
            requested = checked_add_size(requested, self.form_data.len())?;
        }
        if let Some(session) = &self.session {
            requested = checked_add_size(requested, "Session: \n".len())?;
            requested = checked_add_size(requested, session.len())?;
        }
        enforce_message_limit(MessageType::Input, requested, MAX_INPUT_MESSAGE_SIZE)?;
        let mut s = try_string_with_capacity(requested)?;
        s.push_str("INPUT ");
        s.push_str(&self.path);
        s.push('\n');
        if !self.form_data.is_empty() {
            s.push_str("Form: ");
            s.push_str(&self.form_data);
            s.push('\n');
        }
        if let Some(ref t) = self.session {
            s.push_str("Session: ");
            s.push_str(t);
            s.push('\n');
        }
        Ok(s)
    }
}

/// Subscribe mode — whether the client wants delta or full-replace updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubscribeMode {
    /// Full content on every update (default, backward compatible).
    #[default]
    Replace,
    /// Client supports incremental/delta updates.
    Delta,
}

/// Server SUBSCRIBE message — request live updates for a region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeMessage {
    pub path: String,
    pub region: String,
    pub mode: SubscribeMode,
    /// Session token matching the subscription endpoint, if any.
    pub session: Option<String>,
}

impl SubscribeMessage {
    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Subscribe, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
        let mut lines = body.lines();
        let first = lines
            .next()
            .ok_or_else(|| ProtocolError::InvalidMessage("empty SUBSCRIBE body".into()))?;
        let Some(stripped) = first.strip_prefix("SUBSCRIBE ") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected SUBSCRIBE path, got: {first}"
            )));
        };
        let path = try_owned_string(stripped)?;
        validate_primary_value("SUBSCRIBE path", &path)?;

        let mut region = String::new();
        let mut mode = SubscribeMode::Replace;
        let mut session = None;
        let mut seen_region = false;
        let mut seen_mode = false;
        let mut seen_session = false;
        for line in lines {
            let (key, value) = parse_control_field(line)?;
            match key {
                "Region" if !seen_region => {
                    seen_region = true;
                    region = try_owned_string(value)?;
                }
                "Mode" if !seen_mode && value == "delta" => {
                    seen_mode = true;
                    mode = SubscribeMode::Delta;
                }
                "Mode" if !seen_mode && value == "replace" => {
                    seen_mode = true;
                    mode = SubscribeMode::Replace;
                }
                "Session" if !seen_session => {
                    seen_session = true;
                    session = Some(try_owned_string(value)?);
                }
                "Region" | "Session" => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "duplicate SUBSCRIBE field: {key}"
                    )));
                }
                "Mode" => {
                    if seen_mode {
                        return Err(ProtocolError::InvalidMessage(
                            "duplicate SUBSCRIBE field: Mode".into(),
                        ));
                    }
                    return Err(ProtocolError::invalid_message(format_args!(
                        "unknown subscription mode: {value}"
                    )));
                }
                _ => {
                    return Err(ProtocolError::invalid_message(format_args!(
                        "unknown SUBSCRIBE field: {key}"
                    )));
                }
            }
        }

        Ok(SubscribeMessage {
            path,
            region,
            mode,
            session,
        })
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        Self::try_serialize_parts(&self.path, &self.region, self.mode, self.session.as_deref())
    }

    pub fn try_serialize_parts(
        path: &str,
        region: &str,
        mode: SubscribeMode,
        session: Option<&str>,
    ) -> Result<String, ProtocolError> {
        let requested = "SUBSCRIBE \n"
            .len()
            .checked_add(path.len())
            .and_then(|size| {
                (!region.is_empty())
                    .then_some("Region: \n".len().checked_add(region.len()))
                    .flatten()
                    .map_or(Some(size), |extra| size.checked_add(extra))
            })
            .and_then(|size| {
                (mode == SubscribeMode::Delta)
                    .then_some("Mode: delta\n".len())
                    .map_or(Some(size), |extra| size.checked_add(extra))
            })
            .and_then(|size| {
                session.map_or(Some(size), |value| {
                    size.checked_add("Session: \n".len())
                        .and_then(|size| size.checked_add(value.len()))
                })
            })
            .ok_or(ProtocolError::ResourceExhausted {
                requested: usize::MAX,
            })?;
        if requested > MAX_CONTROL_MESSAGE_SIZE {
            return Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Subscribe,
                size: requested as u32,
                max: MAX_CONTROL_MESSAGE_SIZE,
            });
        }
        let mut body = String::new();
        body.try_reserve_exact(requested)
            .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
        body.push_str("SUBSCRIBE ");
        body.push_str(path);
        body.push('\n');
        if !region.is_empty() {
            body.push_str("Region: ");
            body.push_str(region);
            body.push('\n');
        }
        if mode == SubscribeMode::Delta {
            body.push_str("Mode: delta\n");
        }
        if let Some(session) = session {
            body.push_str("Session: ");
            body.push_str(session);
            body.push('\n');
        }
        Ok(body)
    }
}

/// UPDATE response flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateFlags {
    /// When true, content is incremental — append/prepend to existing region
    /// content rather than replacing it. The client's scroll mode determines
    /// insert position.
    pub delta: bool,
}

impl UpdateFlags {
    pub fn from_bits(flags: u8) -> Self {
        UpdateFlags {
            delta: flags & 0x01 != 0,
        }
    }

    pub fn to_bits(self) -> u8 {
        let mut bits = 0u8;
        if self.delta {
            bits |= 0x01;
        }
        bits
    }
}

/// Server UPDATE message — pushed content for a live region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMessage {
    pub region: String,
    pub content: String,
    /// Flags from the frame header (not serialized in the body).
    pub flags: UpdateFlags,
}

impl UpdateMessage {
    fn validate_serialized(body: &str) -> Result<(), ProtocolError> {
        let first_line_end = body.find('\n').unwrap_or(body.len());
        let first = &body[..first_line_end];
        let Some(region) = first.strip_prefix("UPDATE ") else {
            return Err(ProtocolError::InvalidMessage(
                "UPDATE body has an invalid first line".into(),
            ));
        };
        validate_primary_value("UPDATE region-id", region)?;
        if !body[first_line_end..].starts_with("\n\n") {
            return Err(ProtocolError::InvalidMessage(
                "UPDATE body is missing its content separator".into(),
            ));
        }
        Ok(())
    }

    pub fn parse(body: &str) -> Result<Self, ProtocolError> {
        enforce_message_limit(MessageType::Update, body.len(), MAX_LIVE_UPDATE_SIZE)?;
        Self::validate_serialized(body)?;
        let first_line_end = body.find('\n').unwrap_or(body.len());
        let first = body.get(..first_line_end).unwrap_or(body);
        let Some(stripped) = first.strip_prefix("UPDATE ") else {
            return Err(ProtocolError::invalid_message(format_args!(
                "expected UPDATE, got: {first}"
            )));
        };
        let region = try_owned_string(stripped)?;

        // Content follows after a blank line
        let content = if let Some(pos) = body.find("\n\n") {
            try_owned_string(&body[pos + 2..])?
        } else {
            String::new()
        };

        Ok(UpdateMessage {
            region,
            content,
            flags: UpdateFlags::default(),
        })
    }

    /// Set flags from the frame header byte (flags live in the frame, not the body).
    pub fn with_flags(mut self, flags_byte: u8) -> Self {
        self.flags = UpdateFlags::from_bits(flags_byte);
        self
    }

    pub fn serialize(&self) -> Result<String, ProtocolError> {
        Self::try_serialize_parts(&self.region, &self.content)
    }

    /// Length of the UPDATE body that `try_serialize_parts` would produce.
    pub fn parts_body_len(region: &str, content_len: usize) -> Result<usize, ProtocolError> {
        "UPDATE \n\n"
            .len()
            .checked_add(region.len())
            .and_then(|size| size.checked_add(content_len))
            .ok_or(ProtocolError::ResourceExhausted {
                requested: usize::MAX,
            })
    }

    /// Validate an UPDATE that will be written to the wire from its parts,
    /// without assembling it.
    ///
    /// This is exactly what `try_serialize_parts` checks, minus the allocation.
    /// Writing the header, `"UPDATE "`, the region, the separator and the
    /// content directly to the stream lets a live update be fanned out to many
    /// subscribers without allocating a copy of the content per subscriber —
    /// which is the difference between live regions costing O(subscribers) of
    /// memory and costing O(1). The structural checks `validate_serialized`
    /// performs on a received body (the `UPDATE ` prefix and the `\n\n`
    /// separator) hold by construction here, so only the region and the size
    /// limit remain to be established.
    ///
    /// `parts_framing_matches_assembled_body` asserts the equivalence rather
    /// than leaving it to this comment.
    pub fn validate_update_parts(region: &str, content_len: usize) -> Result<(), ProtocolError> {
        validate_primary_value("UPDATE region-id", region)?;
        let requested = Self::parts_body_len(region, content_len)?;
        enforce_message_limit(MessageType::Update, requested, MAX_LIVE_UPDATE_SIZE)
    }

    /// Fallibly serialize a complete UPDATE body while enforcing its semantic
    /// wire limit before allocating the destination.
    pub fn try_serialize_parts(region: &str, content: &str) -> Result<String, ProtocolError> {
        validate_primary_value("UPDATE region-id", region)?;
        let requested = "UPDATE \n\n"
            .len()
            .checked_add(region.len())
            .and_then(|size| size.checked_add(content.len()))
            .ok_or(ProtocolError::ResourceExhausted {
                requested: usize::MAX,
            })?;
        enforce_message_limit(MessageType::Update, requested, MAX_LIVE_UPDATE_SIZE)?;
        let mut body = String::new();
        body.try_reserve_exact(requested)
            .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
        body.push_str("UPDATE ");
        body.push_str(region);
        body.push_str("\n\n");
        body.push_str(content);
        Ok(body)
    }
}

/// Allocation-free metadata produced while validating a complete frame body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameBodyMetadata {
    carries_session: bool,
}

impl FrameBodyMetadata {
    pub const fn carries_session(self) -> bool {
        self.carries_session
    }
}

fn validate_capability_list(value: &str) -> Result<(), ProtocolError> {
    if value.split(',').count() > 32
        || value.split(',').any(|capability| {
            capability.is_empty()
                || !capability
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        })
    {
        return Err(ProtocolError::InvalidMessage(
            "invalid capability list".into(),
        ));
    }
    Ok(())
}

fn validate_hello_body(body: &str) -> Result<(), ProtocolError> {
    enforce_message_limit(MessageType::Hello, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
    let mut lines = body.lines();
    let first = lines
        .next()
        .ok_or_else(|| ProtocolError::InvalidMessage("empty HELLO body".into()))?;
    if !first.starts_with("HELLO/") {
        return Err(ProtocolError::invalid_message(format_args!(
            "expected HELLO/, got: {first}"
        )));
    }
    let mut seen = [false; 4];
    for line in lines {
        let (key, value) = parse_control_field(line)?;
        let index = match key {
            "Terminal-Size" => 0,
            "Color-Support" => 1,
            "Client" => 2,
            "Capabilities" => 3,
            _ => {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown HELLO field: {key}"
                )));
            }
        };
        if !mark_seen(&mut seen, index) {
            return Err(ProtocolError::invalid_message(format_args!(
                "duplicate HELLO field: {key}"
            )));
        }
        if key == "Capabilities" {
            validate_capability_list(value)?;
        }
    }
    Ok(())
}

fn validate_welcome_body(body: &str) -> Result<(), ProtocolError> {
    enforce_message_limit(MessageType::Welcome, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
    let mut lines = body.lines();
    let first = lines
        .next()
        .ok_or_else(|| ProtocolError::InvalidMessage("empty WELCOME body".into()))?;
    if !first.starts_with("WELCOME/") {
        return Err(ProtocolError::invalid_message(format_args!(
            "expected WELCOME/, got: {first}"
        )));
    }
    let mut seen = [false; 3];
    for line in lines {
        let (key, value) = parse_control_field(line)?;
        let index = match key {
            "Server" => 0,
            "Site-Name" => 1,
            "Capabilities" => 2,
            _ => {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown WELCOME field: {key}"
                )));
            }
        };
        if !mark_seen(&mut seen, index) {
            return Err(ProtocolError::invalid_message(format_args!(
                "duplicate WELCOME field: {key}"
            )));
        }
        if key == "Capabilities" {
            validate_capability_list(value)?;
        }
    }
    Ok(())
}

fn validate_get_body(body: &str) -> Result<bool, ProtocolError> {
    enforce_message_limit(MessageType::Get, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
    let mut lines = body.lines();
    let first = lines
        .next()
        .ok_or_else(|| ProtocolError::InvalidMessage("empty GET body".into()))?;
    let Some(path) = first.strip_prefix("GET ") else {
        return Err(ProtocolError::invalid_message(format_args!(
            "expected GET path, got: {first}"
        )));
    };
    validate_primary_value("GET path", path)?;
    let mut seen = [false; 3];
    for line in lines {
        let (key, _) = parse_control_field(line)?;
        let index = match key {
            "Query" => 0,
            "Referrer" => 1,
            "Session" => 2,
            _ => {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown GET field: {key}"
                )));
            }
        };
        if !mark_seen(&mut seen, index) {
            return Err(ProtocolError::invalid_message(format_args!(
                "duplicate GET field: {key}"
            )));
        }
    }
    Ok(seen[2])
}

fn validate_input_body(body: &str) -> Result<bool, ProtocolError> {
    enforce_message_limit(MessageType::Input, body.len(), MAX_INPUT_MESSAGE_SIZE)?;
    let mut lines = body.lines();
    let first = lines
        .next()
        .ok_or_else(|| ProtocolError::InvalidMessage("empty INPUT body".into()))?;
    let Some(path) = first.strip_prefix("INPUT ") else {
        return Err(ProtocolError::invalid_message(format_args!(
            "expected INPUT path, got: {first}"
        )));
    };
    validate_primary_value("INPUT path", path)?;
    let mut seen = [false; 2];
    for line in lines {
        let (key, _) = parse_control_field(line)?;
        let index = match key {
            "Form" => 0,
            "Session" => 1,
            _ => {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown INPUT field: {key}"
                )));
            }
        };
        if !mark_seen(&mut seen, index) {
            return Err(ProtocolError::invalid_message(format_args!(
                "duplicate INPUT field: {key}"
            )));
        }
    }
    Ok(seen[1])
}

fn validate_subscribe_body(body: &str) -> Result<bool, ProtocolError> {
    enforce_message_limit(MessageType::Subscribe, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
    let mut lines = body.lines();
    let first = lines
        .next()
        .ok_or_else(|| ProtocolError::InvalidMessage("empty SUBSCRIBE body".into()))?;
    let Some(path) = first.strip_prefix("SUBSCRIBE ") else {
        return Err(ProtocolError::invalid_message(format_args!(
            "expected SUBSCRIBE path, got: {first}"
        )));
    };
    validate_primary_value("SUBSCRIBE path", path)?;
    let mut seen = [false; 3];
    for line in lines {
        let (key, value) = parse_control_field(line)?;
        let index = match key {
            "Region" => 0,
            "Mode" => 1,
            "Session" => 2,
            _ => {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown SUBSCRIBE field: {key}"
                )));
            }
        };
        if !mark_seen(&mut seen, index) {
            return Err(ProtocolError::invalid_message(format_args!(
                "duplicate SUBSCRIBE field: {key}"
            )));
        }
        if key == "Mode" && !matches!(value, "delta" | "replace") {
            return Err(ProtocolError::invalid_message(format_args!(
                "unknown subscription mode: {value}"
            )));
        }
    }
    Ok(seen[2])
}

/// Validate a PAGE body without assembling it.
///
/// This must accept and reject exactly what [`PageMessage::decode_body`] does —
/// it is the same grammar checked on the framing path, where no `PageMessage` is
/// built. Two readers of one wire format drift silently, so
/// `page_validation_matches_decoding` compares them over a corpus rather than
/// trusting this comment.
fn validate_page_body(body: &str, flags: PageFlags) -> Result<(), ProtocolError> {
    enforce_message_limit(MessageType::Page, body.len(), MAX_PAGE_MESSAGE_SIZE)?;
    if !flags.has_session && !flags.has_path {
        return Ok(());
    }
    let Some((metadata, _)) = body.split_once("\n\n") else {
        return Err(ProtocolError::InvalidMessage(
            "PAGE metadata flag requires metadata separator".into(),
        ));
    };
    let mut saw_path = false;
    let mut saw_directive = false;
    for line in metadata.lines() {
        let (key, value) = parse_control_field(line)?;
        let valid = match key {
            "Path" => {
                if saw_path {
                    return Err(ProtocolError::InvalidMessage(
                        "duplicate PAGE Path field".into(),
                    ));
                }
                if !flags.has_path {
                    return Err(ProtocolError::InvalidMessage(
                        "PAGE Path without the path flag".into(),
                    ));
                }
                saw_path = true;
                if value.len() > MAX_PAGE_PATH_LEN {
                    return Err(ProtocolError::InvalidMessage("PAGE Path too long".into()));
                }
                if !valid_page_path(value) {
                    return Err(ProtocolError::InvalidMessage(
                        "PAGE Path must be an absolute same-origin path".into(),
                    ));
                }
                true
            }
            "Set-Session" => {
                saw_directive = true;
                let mut parts = value.splitn(3, ' ');
                let token = parts.next().unwrap_or_default();
                let scope = parts.next().unwrap_or("/");
                !token.is_empty() && token.len() <= MAX_TOKEN_LEN && valid_session_scope(scope)
            }
            "Clear-Session" => {
                saw_directive = true;
                valid_session_scope(value.trim())
            }
            _ => {
                return Err(ProtocolError::invalid_message(format_args!(
                    "unknown PAGE metadata field: {key}"
                )));
            }
        };
        if !valid {
            return Err(ProtocolError::invalid_message(format_args!(
                "invalid PAGE metadata field: {key}"
            )));
        }
    }
    if flags.has_path && !saw_path {
        return Err(ProtocolError::InvalidMessage(
            "PAGE path flag without a Path field".into(),
        ));
    }
    if flags.has_session && !saw_directive {
        return Err(ProtocolError::InvalidMessage(
            "PAGE session flag without a session directive".into(),
        ));
    }
    Ok(())
}

/// `REDIRECT " ("301" / "302")` per `docs/spec/05-conformance.md`. Parsing as a
/// `u16` admits every other code, including ones a client has no defined
/// behaviour for; an undefined redirect is a fatal protocol error, not a
/// forward-compatibility hook.
fn parse_redirect_code(code: &str) -> Result<u16, ProtocolError> {
    match code {
        "301" => Ok(301),
        "302" => Ok(302),
        other => Err(ProtocolError::invalid_message(format_args!(
            "invalid redirect code: {other}"
        ))),
    }
}

fn validate_redirect_body(body: &str) -> Result<(), ProtocolError> {
    enforce_message_limit(MessageType::Redirect, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
    let first = body
        .lines()
        .next()
        .ok_or_else(|| ProtocolError::InvalidMessage("empty REDIRECT body".into()))?;
    let Some(rest) = first.strip_prefix("REDIRECT ") else {
        return Err(ProtocolError::invalid_message(format_args!(
            "expected REDIRECT, got: {first}"
        )));
    };
    let (code, target) = rest
        .split_once(' ')
        .ok_or_else(|| ProtocolError::InvalidMessage("REDIRECT missing target".into()))?;
    parse_redirect_code(code)?;
    validate_primary_value("REDIRECT target", target)?;
    // The grammar says `atp-uri`, and the client resolves the target with
    // `AtpUri::parse` rather than `resolve`, so a relative or non-atp target is
    // a malformed body rather than a navigation that fails later. Checking it
    // here rejects the frame at the peer that received it, instead of carrying
    // a target that cannot be used to the point of use.
    crate::protocol::uri::AtpUri::parse(target)?;
    if body.lines().count() != 1 {
        return Err(ProtocolError::InvalidMessage(
            "REDIRECT must not contain metadata".into(),
        ));
    }
    Ok(())
}

/// `ERROR " 3DIGIT` per `docs/spec/05-conformance.md`. Parsing as a `u16` alone
/// admits `ERROR 7` and `ERROR 99`, which the contract does not, so the width
/// is checked before the value: a conformance suite cannot enforce a limit the
/// parser is more permissive than.
fn parse_error_code(code: &str) -> Result<u16, ProtocolError> {
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProtocolError::invalid_message(format_args!(
            "invalid error code: {code}"
        )));
    }
    code.parse::<u16>()
        .map_err(|_| ProtocolError::invalid_message(format_args!("invalid error code: {code}")))
}

fn validate_error_body(body: &str) -> Result<(), ProtocolError> {
    enforce_message_limit(MessageType::Error, body.len(), MAX_CONTROL_MESSAGE_SIZE)?;
    let mut lines = body.lines();
    let first = lines
        .next()
        .ok_or_else(|| ProtocolError::InvalidMessage("empty ERROR body".into()))?;
    let Some(code) = first.strip_prefix("ERROR ") else {
        return Err(ProtocolError::invalid_message(format_args!(
            "expected ERROR code, got: {first}"
        )));
    };
    parse_error_code(code)?;
    let mut seen_message = false;
    for line in lines {
        let (key, _) = parse_control_field(line)?;
        if key != "Message" {
            return Err(ProtocolError::invalid_message(format_args!(
                "unknown ERROR field: {key}"
            )));
        }
        if seen_message {
            return Err(ProtocolError::InvalidMessage(
                "duplicate ERROR field: Message".into(),
            ));
        }
        seen_message = true;
    }
    Ok(())
}

/// Check the framing of a line-structured control body.
///
/// `docs/spec/05-conformance.md`: "Control lines end in LF; CR, NUL, other
/// controls, duplicate singleton fields, malformed fields, and unknown fields
/// are rejected."
///
/// This is not cosmetic. `str::lines` silently strips a trailing CR, so
/// `GET /\r\n` would otherwise validate as a request for `/` while a peer
/// splitting on LF alone read a path of `/\r`. Two implementations disagreeing
/// about the same bytes is the shape every request-smuggling bug has, and the
/// disagreement is invisible to both.
///
/// The same reasoning covers the final LF, which the grammar requires on every
/// control line: `str::lines` yields the same thing for `GET /` and `GET /\n`,
/// so without this an unterminated body is indistinguishable from a terminated
/// one, and a peer that buffered until LF would still be waiting.
///
/// Applies to control bodies only. PAGE and UPDATE carry AML, whose whitespace
/// production includes CR, and RESOURCE is opaque bytes.
fn validate_control_framing(
    message: crate::protocol::frame::MessageType,
    text: &str,
) -> Result<(), ProtocolError> {
    if let Some(found) = text.chars().find(|ch| *ch != '\n' && ch.is_ascii_control()) {
        return Err(ProtocolError::invalid_message(format_args!(
            "{message:?} body contains control character U+{:04X}",
            found as u32
        )));
    }
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(ProtocolError::invalid_message(format_args!(
            "{message:?} body does not end in LF"
        )));
    }
    Ok(())
}

/// Validate a complete frame body before any transport dispatches it.
///
/// Successful validation borrows the wire body and performs no owning parse.
/// Consumers construct an owned message once, only when they need one.
pub fn validate_frame_body(
    message: crate::protocol::frame::MessageType,
    body: &[u8],
    flags: u8,
) -> Result<FrameBodyMetadata, ProtocolError> {
    use crate::protocol::frame::MessageType;
    if message == MessageType::Resource {
        return Ok(FrameBodyMetadata::default());
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| ProtocolError::invalid_message(format_args!("{message:?} is not UTF-8")))?;
    if matches!(
        message,
        MessageType::Hello
            | MessageType::Welcome
            | MessageType::Get
            | MessageType::Input
            | MessageType::Subscribe
            | MessageType::Redirect
            | MessageType::Error
    ) {
        validate_control_framing(message, text)?;
    }
    let carries_session = match message {
        MessageType::Hello => validate_hello_body(text).map(|()| false)?,
        MessageType::Get => validate_get_body(text)?,
        MessageType::Input => validate_input_body(text)?,
        MessageType::Subscribe => validate_subscribe_body(text)?,
        MessageType::Unsubscribe
        | MessageType::Ping
        | MessageType::Pong
        | MessageType::Bye
        | MessageType::ServerBye => {
            if !body.is_empty() {
                return Err(ProtocolError::invalid_message(format_args!(
                    "{message:?} must have an empty body"
                )));
            }
            false
        }
        MessageType::Welcome => validate_welcome_body(text).map(|()| false)?,
        MessageType::Page => {
            let page_flags = PageFlags::from_bits(flags);
            validate_page_body(text, page_flags)?;
            page_flags.has_session
        }
        MessageType::Update => {
            enforce_message_limit(MessageType::Update, text.len(), MAX_LIVE_UPDATE_SIZE)?;
            UpdateMessage::validate_serialized(text)?;
            false
        }
        MessageType::Redirect => validate_redirect_body(text).map(|()| false)?,
        MessageType::Error => validate_error_body(text).map(|()| false)?,
        MessageType::Resource => unreachable!(),
    };
    Ok(FrameBodyMetadata { carries_session })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allocation-free fan-out path validates and frames an UPDATE without
    /// assembling its body. That is only safe if it accepts and rejects exactly
    /// what the assembling path does, and produces the same bytes when it
    /// accepts. Asserted here rather than asserted in a comment.
    #[test]
    fn parts_framing_matches_assembled_body() {
        let oversize_region = "r".repeat(64);
        let max_content = MAX_LIVE_UPDATE_SIZE - "UPDATE \n\n".len() - "region".len();
        let cases: [(&str, String); 9] = [
            ("region", String::new()),
            ("region", "x".to_string()),
            ("region", "line one\nline two\n".to_string()),
            ("region", "\u{1f600} multi-byte \u{00e9}".to_string()),
            ("region", "embedded\n\nseparator".to_string()),
            ("region", "x".repeat(max_content)),
            ("region", "x".repeat(max_content + 1)),
            ("", "content".to_string()),
            (oversize_region.as_str(), "content".to_string()),
        ];

        for (region, content) in &cases {
            let assembled = UpdateMessage::try_serialize_parts(region, content);
            let parts = UpdateMessage::validate_update_parts(region, content.len());
            assert_eq!(
                assembled.is_ok(),
                parts.is_ok(),
                "verdicts diverged for region {region:?} with {} content bytes",
                content.len()
            );
            if let Ok(body) = assembled {
                // The bytes the fan-out path writes, in the order it writes them.
                let mut written = String::from("UPDATE ");
                written.push_str(region);
                written.push_str("\n\n");
                written.push_str(content);
                assert_eq!(written, body, "framing diverged for region {region:?}");
                assert_eq!(
                    UpdateMessage::parts_body_len(region, content.len()).unwrap(),
                    body.len(),
                    "declared body length must match the frame that is written"
                );
                // And the assembled body must still pass inbound validation.
                validate_frame_body(MessageType::Update, body.as_bytes(), 0).unwrap();
            }
        }
    }

    // ─── HelloMessage ────────────────────────────────────────

    #[test]
    fn hello_roundtrip() {
        let msg = HelloMessage {
            protocol_version: "1.0".into(),
            terminal_size: Some("120x40".into()),
            color_support: Some("truecolor".into()),
            client: Some("Dustnet/0.1".into()),
            capabilities: vec!["sessions".into(), "wasm-effects".into()],
        };
        let serialized = msg.serialize().unwrap();
        let parsed = HelloMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn hello_minimal() {
        let msg = HelloMessage {
            protocol_version: "1.0".into(),
            terminal_size: None,
            color_support: None,
            client: None,
            capabilities: Vec::new(),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = HelloMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn hello_parse_bad_prefix() {
        assert!(HelloMessage::parse("WELCOME/1.0\n").is_err());
    }

    #[test]
    fn hello_rejects_unknown_fields() {
        let body = "HELLO/1.0\nFuture-Field: value\nClient: test\n";
        assert!(HelloMessage::parse(body).is_err());
    }

    #[test]
    fn hello_rejects_duplicate_singletons_and_malformed_fields() {
        assert!(HelloMessage::parse("HELLO/0.2\nClient: one\nClient: two\n").is_err());
        assert!(HelloMessage::parse("HELLO/0.2\nClient=broken\n").is_err());
    }

    // ─── WelcomeMessage ──────────────────────────────────────

    #[test]
    fn welcome_roundtrip() {
        let msg = WelcomeMessage {
            protocol_version: "1.0".into(),
            server: Some("Dustnet-Server/0.1".into()),
            site_name: Some("Test Site".into()),
            capabilities: vec!["live-updates".into()],
        };
        let serialized = msg.serialize().unwrap();
        let parsed = WelcomeMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn welcome_minimal() {
        let msg = WelcomeMessage {
            protocol_version: "1.0".into(),
            server: None,
            site_name: None,
            capabilities: Vec::new(),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = WelcomeMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    // ─── GetMessage ──────────────────────────────────────────

    #[test]
    fn get_roundtrip() {
        let msg = GetMessage {
            path: "/hello".into(),
            query: Some("key=val".into()),
            referrer: Some("atp://other.site/page".into()),
            session: None,
        };
        let serialized = msg.serialize().unwrap();
        let parsed = GetMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn get_with_session() {
        let msg = GetMessage {
            path: "/admin/dashboard".into(),
            query: None,
            referrer: None,
            session: Some("abc123".into()),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = GetMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
        assert_eq!(parsed.session, Some("abc123".into()));
    }

    #[test]
    fn singleton_fields_and_unknown_metadata_are_strictly_rejected() {
        assert!(GetMessage::parse("GET /\nSession: one\nSession: two\n").is_err());
        assert!(InputMessage::parse("INPUT /\nForm: a\nForm: b\n").is_err());
        assert!(SubscribeMessage::parse("SUBSCRIBE /\nMode: invented\n").is_err());
        assert!(ErrorMessage::parse("ERROR 400\nUnexpected: value\n").is_err());
        assert!(RedirectMessage::parse("REDIRECT 302 atp://example.com/\nExtra: x\n").is_err());
    }

    #[test]
    fn get_minimal() {
        let msg = GetMessage {
            path: "/".into(),
            query: None,
            referrer: None,
            session: None,
        };
        let serialized = msg.serialize().unwrap();
        let parsed = GetMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    // ─── PageFlags ───────────────────────────────────────────

    #[test]
    fn page_flags_roundtrip() {
        let flags = PageFlags {
            cacheable: true,
            has_live_regions: true,
            has_path: false,
            has_session: false,
        };
        let bits = flags.to_bits();
        assert_eq!(bits, 0x03);
        let decoded = PageFlags::from_bits(bits);
        assert_eq!(decoded, flags);
    }

    #[test]
    fn page_flags_session() {
        let flags = PageFlags {
            cacheable: false,
            has_live_regions: false,
            has_path: false,
            has_session: true,
        };
        assert_eq!(flags.to_bits(), 0x08);
        let decoded = PageFlags::from_bits(0x08);
        assert_eq!(decoded, flags);
    }

    #[test]
    fn page_flags_none() {
        let flags = PageFlags::default();
        assert_eq!(flags.to_bits(), 0);
        let decoded = PageFlags::from_bits(0);
        assert_eq!(decoded, flags);
    }

    // ─── PageMessage ──────────────────────────────────────────

    #[test]
    fn page_message_no_session() {
        let msg = PageMessage {
            content: "[page][text]hello[/text][/page]".into(),
            flags: PageFlags::default(),
            path: None,
            session_directives: Vec::new(),
        };
        let (body, flags_byte) = msg.encode_body().unwrap();
        let decoded = PageMessage::decode_body(&body, flags_byte).unwrap();
        assert_eq!(decoded.content, msg.content);
        assert!(decoded.session_directives.is_empty());
        assert!(!decoded.flags.has_session);
    }

    #[test]
    fn page_message_with_set_session() {
        let msg = PageMessage {
            content: "[page][text]welcome[/text][/page]".into(),
            flags: PageFlags::default(),
            path: None,
            session_directives: vec![SessionDirective::Set {
                token: "tok123".into(),
                scope: "/admin/".into(),
                expires: None,
            }],
        };
        let (body, flags_byte) = msg.encode_body().unwrap();

        // Flag should be set
        assert!(PageFlags::from_bits(flags_byte).has_session);

        let decoded = PageMessage::decode_body(&body, flags_byte).unwrap();
        assert_eq!(decoded.content, msg.content);
        assert_eq!(decoded.session_directives.len(), 1);
        assert_eq!(
            decoded.session_directives[0],
            SessionDirective::Set {
                token: "tok123".into(),
                scope: "/admin/".into(),
                expires: None,
            }
        );
    }

    #[test]
    fn page_message_with_clear_session() {
        let msg = PageMessage {
            content: "[page][text]logged out[/text][/page]".into(),
            flags: PageFlags::default(),
            path: None,
            session_directives: vec![SessionDirective::Clear {
                scope: "/admin/".into(),
            }],
        };
        let (body, flags_byte) = msg.encode_body().unwrap();
        let decoded = PageMessage::decode_body(&body, flags_byte).unwrap();
        assert_eq!(decoded.content, msg.content);
        assert_eq!(
            decoded.session_directives[0],
            SessionDirective::Clear {
                scope: "/admin/".into(),
            }
        );
    }

    #[test]
    fn page_message_with_a_path_names_itself() {
        let msg = PageMessage {
            content: "[page][text]news[/text][/page]".into(),
            flags: PageFlags::default(),
            path: Some("/index".into()),
            session_directives: Vec::new(),
        };
        let (body, flags_byte) = msg.encode_body().unwrap();
        assert!(PageFlags::from_bits(flags_byte).has_path);
        assert!(!PageFlags::from_bits(flags_byte).has_session);
        assert!(body.starts_with(b"Path: /index\n\n"));
        let decoded = PageMessage::decode_body(&body, flags_byte).unwrap();
        assert_eq!(decoded.path.as_deref(), Some("/index"));
        assert_eq!(decoded.content, msg.content);
    }

    /// The case the field exists for: a login answers with the front page and
    /// says so, in the same frame that issues the session.
    #[test]
    fn a_path_and_a_session_share_one_metadata_block() {
        let msg = PageMessage {
            content: "[page][text]news[/text][/page]".into(),
            flags: PageFlags::default(),
            path: Some("/index".into()),
            session_directives: vec![SessionDirective::Set {
                token: "tok123".into(),
                scope: "/".into(),
                expires: Some(1_900_000_000),
            }],
        };
        let (body, flags_byte) = msg.encode_body().unwrap();
        let flags = PageFlags::from_bits(flags_byte);
        assert!(flags.has_path && flags.has_session);
        let decoded = PageMessage::decode_body(&body, flags_byte).unwrap();
        assert_eq!(decoded.path.as_deref(), Some("/index"));
        assert_eq!(decoded.session_directives.len(), 1);
        assert_eq!(decoded.content, msg.content);
    }

    /// A path names a page on the site that sent it. A URI would let a page
    /// relabel itself as living somewhere else, which is a redirect wearing a
    /// page's clothes and would escape the redirect limit.
    #[test]
    fn a_page_path_must_be_absolute_and_bounded() {
        for rejected in [
            "atp://elsewhere.example/index",
            "//elsewhere.example/index",
            "index",
            "../index",
            "/index#frag",
            "",
        ] {
            let msg = PageMessage {
                content: "[page][/page]".into(),
                flags: PageFlags::default(),
                path: Some(rejected.into()),
                session_directives: Vec::new(),
            };
            match msg.encode_body() {
                Err(_) => {}
                Ok((body, flags_byte)) => {
                    assert!(
                        PageMessage::decode_body(&body, flags_byte).is_err(),
                        "{rejected:?} should not survive a round trip"
                    );
                }
            }
        }
        let oversized = format!("/{}", "x".repeat(MAX_PAGE_PATH_LEN));
        let msg = PageMessage {
            content: "[page][/page]".into(),
            flags: PageFlags::default(),
            path: Some(oversized),
            session_directives: Vec::new(),
        };
        assert!(msg.encode_body().is_err());
    }

    /// A flag promising a field that is not there would be a second encoding of
    /// the same message. Both readers refuse it.
    #[test]
    fn a_metadata_flag_without_its_field_is_refused() {
        for (body, flags) in [
            (
                b"Set-Session: tok /\n\n[page][/page]".as_slice(),
                PageFlags {
                    cacheable: false,
                    has_live_regions: false,
                    has_path: true,
                    has_session: true,
                },
            ),
            (
                b"Path: /index\n\n[page][/page]".as_slice(),
                PageFlags {
                    cacheable: false,
                    has_live_regions: false,
                    has_path: true,
                    has_session: true,
                },
            ),
            (
                b"Path: /index\n\n[page][/page]".as_slice(),
                PageFlags {
                    cacheable: false,
                    has_live_regions: false,
                    has_path: false,
                    has_session: true,
                },
            ),
        ] {
            assert!(
                PageMessage::decode_body(body, flags.to_bits()).is_err(),
                "decoded a flag with no field: {flags:?}"
            );
        }
    }

    #[test]
    fn a_duplicate_page_path_is_refused() {
        let flags = PageFlags {
            cacheable: false,
            has_live_regions: false,
            has_path: true,
            has_session: false,
        };
        assert!(
            PageMessage::decode_body(
                b"Path: /index\nPath: /login\n\n[page][/page]",
                flags.to_bits()
            )
            .is_err()
        );
    }

    /// The framing path validates a PAGE without assembling one, so it is a
    /// second reader of the same grammar. Two readers drift; this is what says
    /// they have not.
    #[test]
    fn page_validation_matches_decoding() {
        let bodies: &[&[u8]] = &[
            b"[page][/page]",
            b"Path: /index\n\n[page][/page]",
            b"Path: /index\nSet-Session: tok /\n\n[page][/page]",
            b"Set-Session: tok / 1900000000\n\n[page][/page]",
            b"Clear-Session: /\n\n[page][/page]",
            b"Path: /index\nPath: /login\n\n[page][/page]",
            b"Path: relative\n\n[page][/page]",
            b"Path: atp://elsewhere.example/x\n\n[page][/page]",
            b"Path: //elsewhere.example/x\n\n[page][/page]",
            b"Path: /index#frag\n\n[page][/page]",
            b"Path: /index?item=12\n\n[page][/page]",
            b"Path: /index\n[page][/page]",
            b"Unknown: /index\n\n[page][/page]",
            b"Set-Session: \n\n[page][/page]",
            b"\n\n[page][/page]",
        ];
        for body in bodies {
            for bits in 0u8..16 {
                let flags = PageFlags::from_bits(bits);
                let validated = validate_page_body(
                    std::str::from_utf8(body).expect("test bodies are UTF-8"),
                    flags,
                )
                .is_ok();
                let decoded = PageMessage::decode_body(body, bits).is_ok();
                assert_eq!(
                    validated,
                    decoded,
                    "validator and decoder disagree on {:?} with flags {bits:#04x}",
                    String::from_utf8_lossy(body)
                );
            }
        }
    }

    #[test]
    fn page_encoding_and_decoding_enforce_page_bound() {
        let oversized = "x".repeat(MAX_PAGE_MESSAGE_SIZE + 1);
        let message = PageMessage {
            content: oversized.clone(),
            flags: PageFlags::default(),
            path: None,
            session_directives: Vec::new(),
        };
        assert!(matches!(
            message.encode_body(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Page,
                ..
            })
        ));
        assert!(matches!(
            PageMessage::decode_body(oversized.as_bytes(), 0),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Page,
                ..
            })
        ));
    }

    #[test]
    fn message_serializers_enforce_semantic_bounds() {
        let oversized_control = "x".repeat(MAX_CONTROL_MESSAGE_SIZE);
        let oversized_input = "x".repeat(MAX_INPUT_MESSAGE_SIZE);

        let hello = HelloMessage {
            protocol_version: oversized_control.clone(),
            terminal_size: None,
            color_support: None,
            client: None,
            capabilities: Vec::new(),
        };
        assert!(matches!(
            hello.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Hello,
                ..
            })
        ));

        let welcome = WelcomeMessage {
            protocol_version: oversized_control.clone(),
            server: None,
            site_name: None,
            capabilities: Vec::new(),
        };
        assert!(matches!(
            welcome.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Welcome,
                ..
            })
        ));

        let get = GetMessage {
            path: oversized_control.clone(),
            query: None,
            referrer: None,
            session: None,
        };
        assert!(matches!(
            get.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Get,
                ..
            })
        ));

        let redirect = RedirectMessage {
            code: 302,
            target: oversized_control.clone(),
        };
        assert!(matches!(
            redirect.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Redirect,
                ..
            })
        ));

        let error = ErrorMessage {
            code: 500,
            message: Some(oversized_control.clone()),
        };
        assert!(matches!(
            error.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Error,
                ..
            })
        ));

        let input = InputMessage {
            path: "/".into(),
            form_data: oversized_input,
            session: None,
        };
        assert!(matches!(
            input.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Input,
                ..
            })
        ));

        let subscribe = SubscribeMessage {
            path: oversized_control,
            region: String::new(),
            mode: SubscribeMode::Replace,
            session: None,
        };
        assert!(matches!(
            subscribe.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Subscribe,
                ..
            })
        ));

        let update = UpdateMessage {
            region: "ticker".into(),
            content: "x".repeat(MAX_LIVE_UPDATE_SIZE),
            flags: UpdateFlags::default(),
        };
        assert!(matches!(
            update.serialize(),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Update,
                ..
            })
        ));
    }

    #[test]
    fn message_parsers_enforce_semantic_bounds_before_ownership() {
        let oversized_control = "x".repeat(MAX_CONTROL_MESSAGE_SIZE + 1);
        let oversized_input = "x".repeat(MAX_INPUT_MESSAGE_SIZE + 1);
        let oversized_update = "x".repeat(MAX_LIVE_UPDATE_SIZE + 1);

        for (result, msg_type) in [
            (
                HelloMessage::parse(&oversized_control).map(|_| ()),
                MessageType::Hello,
            ),
            (
                WelcomeMessage::parse(&oversized_control).map(|_| ()),
                MessageType::Welcome,
            ),
            (
                GetMessage::parse(&oversized_control).map(|_| ()),
                MessageType::Get,
            ),
            (
                RedirectMessage::parse(&oversized_control).map(|_| ()),
                MessageType::Redirect,
            ),
            (
                ErrorMessage::parse(&oversized_control).map(|_| ()),
                MessageType::Error,
            ),
            (
                SubscribeMessage::parse(&oversized_control).map(|_| ()),
                MessageType::Subscribe,
            ),
            (
                InputMessage::parse(&oversized_input).map(|_| ()),
                MessageType::Input,
            ),
            (
                UpdateMessage::parse(&oversized_update).map(|_| ()),
                MessageType::Update,
            ),
        ] {
            assert!(matches!(
                result,
                Err(ProtocolError::MessageTooLarge {
                    msg_type: actual,
                    ..
                }) if actual == msg_type
            ));
        }
    }

    #[test]
    fn handshake_serializers_reject_invalid_capability_lists() {
        let invalid = HelloMessage {
            protocol_version: crate::protocol::PROTOCOL_VERSION.into(),
            terminal_size: None,
            color_support: None,
            client: None,
            capabilities: vec!["INVALID".into()],
        };
        assert!(matches!(
            invalid.serialize(),
            Err(ProtocolError::InvalidMessage(_))
        ));

        let too_many = WelcomeMessage {
            protocol_version: crate::protocol::PROTOCOL_VERSION.into(),
            server: None,
            site_name: None,
            capabilities: (0..33).map(|_| "live-regions".into()).collect(),
        };
        assert!(matches!(
            too_many.serialize(),
            Err(ProtocolError::InvalidMessage(_))
        ));
    }

    // ─── RedirectMessage ─────────────────────────────────────

    #[test]
    fn redirect_roundtrip() {
        let msg = RedirectMessage {
            code: 302,
            target: "atp://other.site/page".into(),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = RedirectMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn redirect_permanent() {
        let body = "REDIRECT 301 atp://new.site/home\n";
        let msg = RedirectMessage::parse(body).unwrap();
        assert_eq!(msg.code, 301);
        assert_eq!(msg.target, "atp://new.site/home");
    }

    // ─── ErrorMessage ────────────────────────────────────────

    #[test]
    fn error_roundtrip() {
        let msg = ErrorMessage {
            code: 404,
            message: Some("Page not found".into()),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = ErrorMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn error_without_message() {
        let msg = ErrorMessage {
            code: 500,
            message: None,
        };
        let serialized = msg.serialize().unwrap();
        let parsed = ErrorMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn error_all_codes() {
        for code in [400, 401, 403, 404, 429, 500, 503] {
            let msg = ErrorMessage {
                code,
                message: Some(format!("code {code}")),
            };
            let serialized = msg.serialize().unwrap();
            let parsed = ErrorMessage::parse(&serialized).unwrap();
            assert_eq!(parsed.code, code);
        }
    }

    // ─── InputMessage ───────────────────────────────────────

    #[test]
    fn input_roundtrip() {
        let msg = InputMessage {
            path: "/submit".into(),
            form_data: "name=Alice&msg=Hello+World".into(),
            session: None,
        };
        let serialized = msg.serialize().unwrap();
        let parsed = InputMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn input_with_session() {
        let msg = InputMessage {
            path: "/admin/post".into(),
            form_data: "title=Hello".into(),
            session: Some("admin-tok".into()),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = InputMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
        assert_eq!(parsed.session, Some("admin-tok".into()));
    }

    #[test]
    fn input_empty_form() {
        let msg = InputMessage {
            path: "/action".into(),
            form_data: String::new(),
            session: None,
        };
        let serialized = msg.serialize().unwrap();
        let parsed = InputMessage::parse(&serialized).unwrap();
        assert_eq!(parsed.path, "/action");
    }

    #[test]
    fn input_parse_bad_prefix() {
        assert!(InputMessage::parse("GET /path\n").is_err());
    }

    // ─── SubscribeMessage ───────────────────────────────────

    #[test]
    fn subscribe_roundtrip() {
        let msg = SubscribeMessage {
            path: "/live".into(),
            region: "clock".into(),
            mode: SubscribeMode::Replace,
            session: Some("session-token".into()),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = SubscribeMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn subscribe_no_region() {
        let msg = SubscribeMessage {
            path: "/data".into(),
            region: String::new(),
            mode: SubscribeMode::Replace,
            session: None,
        };
        let serialized = msg.serialize().unwrap();
        let parsed = SubscribeMessage::parse(&serialized).unwrap();
        assert_eq!(parsed.path, "/data");
    }

    #[test]
    fn subscribe_delta_mode_roundtrip() {
        let msg = SubscribeMessage {
            path: "/chat/stream".into(),
            region: "chat".into(),
            mode: SubscribeMode::Delta,
            session: None,
        };
        let serialized = msg.serialize().unwrap();
        assert!(serialized.contains("Mode: delta"));
        let parsed = SubscribeMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn subscribe_missing_mode_defaults_to_replace() {
        let body = "SUBSCRIBE /live\nRegion: clock\n";
        let parsed = SubscribeMessage::parse(body).unwrap();
        assert_eq!(parsed.mode, SubscribeMode::Replace);
    }

    #[test]
    fn fallible_subscribe_serialization_enforces_control_bound() {
        let body = SubscribeMessage::try_serialize_parts(
            "/chat/stream",
            "chat",
            SubscribeMode::Delta,
            Some("token"),
        )
        .unwrap();
        assert_eq!(SubscribeMessage::parse(&body).unwrap().region, "chat");
        let oversized = "x".repeat(MAX_CONTROL_MESSAGE_SIZE);
        assert!(matches!(
            SubscribeMessage::try_serialize_parts(&oversized, "chat", SubscribeMode::Replace, None,),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Subscribe,
                ..
            })
        ));
    }

    // ─── UpdateFlags ─────────────────────────────────────────

    #[test]
    fn update_flags_roundtrip() {
        let flags = UpdateFlags { delta: true };
        assert_eq!(flags.to_bits(), 0x01);
        assert_eq!(UpdateFlags::from_bits(0x01), flags);
    }

    #[test]
    fn update_flags_default_is_no_delta() {
        let flags = UpdateFlags::default();
        assert!(!flags.delta);
        assert_eq!(flags.to_bits(), 0);
    }

    #[test]
    fn update_flags_ignores_unknown_bits() {
        let flags = UpdateFlags::from_bits(0xFE);
        assert!(!flags.delta);
    }

    // ─── UpdateMessage ──────────────────────────────────────

    #[test]
    fn update_roundtrip() {
        let msg = UpdateMessage {
            region: "clock".into(),
            content: "[text]12:34:56[/text]".into(),
            flags: UpdateFlags::default(),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = UpdateMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn update_empty_content() {
        let msg = UpdateMessage {
            region: "status".into(),
            content: String::new(),
            flags: UpdateFlags::default(),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = UpdateMessage::parse(&serialized).unwrap();
        assert_eq!(parsed.region, "status");
    }

    #[test]
    fn update_multiline_content() {
        let msg = UpdateMessage {
            region: "feed".into(),
            content: "[text]Line 1[/text]\n[text]Line 2[/text]".into(),
            flags: UpdateFlags::default(),
        };
        let serialized = msg.serialize().unwrap();
        let parsed = UpdateMessage::parse(&serialized).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn update_with_flags() {
        let body = "UPDATE chat\n\n[text]hello[/text]";
        let msg = UpdateMessage::parse(body).unwrap().with_flags(0x01);
        assert!(msg.flags.delta);
        assert_eq!(msg.region, "chat");
        assert_eq!(msg.content, "[text]hello[/text]");
    }

    #[test]
    fn fallible_update_serialization_enforces_complete_body_bound() {
        let region = "ticker";
        let prefix = "UPDATE \n\n".len() + region.len();
        let exact = "x".repeat(super::super::MAX_LIVE_UPDATE_SIZE - prefix);
        let body = UpdateMessage::try_serialize_parts(region, &exact).unwrap();
        assert_eq!(body.len(), super::super::MAX_LIVE_UPDATE_SIZE);

        let excessive = "x".repeat(exact.len() + 1);
        assert!(matches!(
            UpdateMessage::try_serialize_parts(region, &excessive),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Update,
                ..
            })
        ));
        assert!(UpdateMessage::parse("UPDATE ticker\nmissing separator").is_err());
    }

    #[test]
    fn borrowed_frame_validation_reports_session_metadata() {
        let get = GetMessage {
            path: "/account".into(),
            query: None,
            referrer: None,
            session: Some("token".into()),
        }
        .serialize()
        .unwrap();
        assert!(
            validate_frame_body(MessageType::Get, get.as_bytes(), 0)
                .unwrap()
                .carries_session()
        );

        let input = InputMessage {
            path: "/submit".into(),
            form_data: "name=value".into(),
            session: None,
        }
        .serialize()
        .unwrap();
        assert!(
            !validate_frame_body(MessageType::Input, input.as_bytes(), 0)
                .unwrap()
                .carries_session()
        );

        let page = PageMessage {
            content: "[text]ok[/text]".into(),
            flags: PageFlags::default(),
            path: None,
            session_directives: vec![SessionDirective::Clear { scope: "/".into() }],
        };
        let (body, flags) = page.encode_body().unwrap();
        assert!(
            validate_frame_body(MessageType::Page, &body, flags)
                .unwrap()
                .carries_session()
        );
    }

    #[test]
    fn borrowed_frame_validation_rejects_malformed_typed_bodies() {
        for (message, body, flags) in [
            (
                MessageType::Hello,
                b"HELLO/0.2\nClient: one\nClient: two\n".as_slice(),
                0,
            ),
            (
                MessageType::Welcome,
                b"WELCOME/0.2\nUnknown: value\n".as_slice(),
                0,
            ),
            (
                MessageType::Get,
                b"GET /\nSession: one\nSession: two\n".as_slice(),
                0,
            ),
            (
                MessageType::Input,
                b"INPUT /\nForm: one\nForm: two\n".as_slice(),
                0,
            ),
            (
                MessageType::Subscribe,
                b"SUBSCRIBE /\nMode: invented\n".as_slice(),
                0,
            ),
            (
                MessageType::Page,
                b"Set-Session: token /\ncontent".as_slice(),
                0x08,
            ),
            (
                MessageType::Redirect,
                b"REDIRECT 302 atp://example.com/\nExtra: x\n".as_slice(),
                0,
            ),
            (
                MessageType::Error,
                b"ERROR 400\nUnexpected: value\n".as_slice(),
                0,
            ),
            (
                MessageType::Update,
                b"UPDATE ticker\nmissing separator".as_slice(),
                0,
            ),
        ] {
            assert!(
                validate_frame_body(message, body, flags).is_err(),
                "{message:?} validation unexpectedly succeeded"
            );
        }
    }
}
