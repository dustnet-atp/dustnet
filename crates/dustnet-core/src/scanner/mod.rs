pub mod escape;
#[cfg(test)]
mod tests;

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScannerAllocationSite {
    Sanitized,
    Chars,
    Tokens,
    Text,
    Attributes,
    Name,
    Value,
}

#[cfg(test)]
thread_local! {
    static REJECT_ALLOCATION: std::cell::Cell<Option<ScannerAllocationSite>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
fn reject_allocation(site: ScannerAllocationSite) -> bool {
    REJECT_ALLOCATION.with(|rejected| rejected.get() == Some(site))
}

#[cfg(not(test))]
fn reject_allocation(_site: ScannerAllocationSite) -> bool {
    false
}

fn allocation_error(requested: usize) -> ScanError {
    ScanError::ResourceExhausted { requested }
}

fn try_string(requested: usize, site: ScannerAllocationSite) -> Result<String, ScanError> {
    if reject_allocation(site) {
        return Err(allocation_error(requested));
    }
    let mut value = String::new();
    value
        .try_reserve_exact(requested)
        .map_err(|_| allocation_error(requested))?;
    Ok(value)
}

fn try_reserve_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    site: ScannerAllocationSite,
) -> Result<(), ScanError> {
    let requested = values
        .len()
        .checked_add(additional)
        .and_then(|capacity| capacity.checked_mul(std::mem::size_of::<T>()))
        .ok_or_else(|| allocation_error(usize::MAX))?;
    if reject_allocation(site) {
        return Err(allocation_error(requested));
    }
    values
        .try_reserve_exact(additional)
        .map_err(|_| allocation_error(requested))
}

/// Maximum input size: 1 MiB
const MAX_INPUT_SIZE: usize = 1024 * 1024;

/// Maximum token count before the scanner aborts
const MAX_TOKEN_COUNT: usize = 50_000;

/// Maximum value carried by a single attribute.
const MAX_ATTRIBUTE_VALUE_CHARS: usize = 4_096;

/// Maximum UTF-8 size of one contiguous text token.
const MAX_TEXT_TOKEN_SIZE: usize = 64 * 1024;

/// A single attribute on a tag: `name=value`, `name="value"`, or `name` (flag).
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub value: AttributeValue,
}

/// The value of an attribute.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    /// Quoted string: `attribute="value with spaces"`
    String(String),
    /// Unquoted identifier: `attribute=value`
    Ident(String),
    /// Bare flag with no value: `bold`, `italic`
    Flag,
}

/// A token produced by the scanner.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Opening tag: `[tagname attr=val]` or self-closing `[tagname /]`
    OpenTag {
        name: String,
        attributes: Vec<Attribute>,
        self_closing: bool,
    },
    /// Closing tag: `[/tagname]`
    CloseTag { name: String },
    /// Text content between tags
    Text(String),
    /// End of input
    Eof,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } => {
                write!(f, "[{name}")?;
                for attr in attributes {
                    write!(f, " {}", attr)?;
                }
                if *self_closing {
                    write!(f, " /]")
                } else {
                    write!(f, "]")
                }
            }
            Token::CloseTag { name } => write!(f, "[/{name}]"),
            Token::Text(s) => write!(f, "{s}"),
            Token::Eof => write!(f, "<EOF>"),
        }
    }
}

impl fmt::Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            AttributeValue::String(s) => write!(f, "{}=\"{}\"", self.name, s),
            AttributeValue::Ident(s) => write!(f, "{}={}", self.name, s),
            AttributeValue::Flag => write!(f, "{}", self.name),
        }
    }
}

/// Errors that can occur during scanning.
#[derive(Debug, Clone, PartialEq)]
pub enum ScanError {
    /// Input exceeds maximum size
    InputTooLarge { size: usize, max: usize },
    /// Invalid UTF-8 in input
    InvalidUtf8,
    /// Token count exceeded
    TooManyTokens { max: usize },
    /// Unexpected end of input inside a tag
    UnterminatedTag { offset: usize },
    /// Unexpected end of input inside a quoted string
    UnterminatedString { offset: usize },
    /// Invalid character in tag name
    InvalidTagName { offset: usize },
    /// A single attribute value exceeded its semantic limit.
    AttributeValueTooLong { max: usize },
    /// A single text token exceeded its semantic limit.
    TextTooLong { max: usize },
    /// A bounded scanner allocation could not be completed.
    ResourceExhausted { requested: usize },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::InputTooLarge { size, max } => {
                write!(f, "input too large: {size} bytes (max {max})")
            }
            ScanError::InvalidUtf8 => write!(f, "input is not valid UTF-8"),
            ScanError::TooManyTokens { max } => {
                write!(f, "too many tokens (max {max})")
            }
            ScanError::UnterminatedTag { offset } => {
                write!(f, "unterminated tag at byte offset {offset}")
            }
            ScanError::UnterminatedString { offset } => {
                write!(f, "unterminated string at byte offset {offset}")
            }
            ScanError::InvalidTagName { offset } => {
                write!(f, "invalid tag name at byte offset {offset}")
            }
            ScanError::AttributeValueTooLong { max } => {
                write!(
                    f,
                    "attribute value exceeds maximum length of {max} characters"
                )
            }
            ScanError::TextTooLong { max } => {
                write!(f, "text content exceeds maximum size of {max} bytes")
            }
            ScanError::ResourceExhausted { requested } => {
                write!(f, "scanner allocation failed for {requested} bytes")
            }
        }
    }
}

impl std::error::Error for ScanError {}

/// Scanner for AML (ANSI Markup Language) documents.
///
/// Converts raw input bytes into a stream of tokens. Strips dangerous
/// control characters (including ANSI escape sequences) from text content
/// to prevent terminal injection attacks.
#[derive(Debug)]
pub struct Scanner {
    /// The sanitized input as a vector of chars for easy indexing
    chars: Vec<char>,
    /// Current position in the char array
    pos: usize,
    /// Number of tokens produced so far
    token_count: usize,
}

impl Scanner {
    /// Create a new scanner from raw bytes.
    ///
    /// Validates UTF-8 encoding and input size, then sanitizes control characters.
    pub fn new(input: &[u8]) -> Result<Self, ScanError> {
        if input.len() > MAX_INPUT_SIZE {
            return Err(ScanError::InputTooLarge {
                size: input.len(),
                max: MAX_INPUT_SIZE,
            });
        }

        let input_str = std::str::from_utf8(input).map_err(|_| ScanError::InvalidUtf8)?;

        let mut sanitized = try_string(input.len(), ScannerAllocationSite::Sanitized)?;
        // The destination is reserved to the input byte length and sanitising
        // never grows it, so this cannot fail; discarded rather than unwrapped
        // so the scanner has no panic on the remote-input path.
        let _ = escape::sanitize_into(input_str, &mut sanitized);
        let char_count = sanitized.chars().count();
        let mut chars = Vec::new();
        try_reserve_vec(&mut chars, char_count, ScannerAllocationSite::Chars)?;
        chars.extend(sanitized.chars());

        Ok(Scanner {
            chars,
            pos: 0,
            token_count: 0,
        })
    }

    /// Scan the entire input and return all tokens.
    pub fn scan_all(&mut self) -> Result<Vec<Token>, ScanError> {
        let mut tokens = Vec::new();
        loop {
            try_reserve_vec(&mut tokens, 1, ScannerAllocationSite::Tokens)?;
            let token = self.next_token()?;
            let is_eof = token == Token::Eof;
            tokens.push(token);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    /// Get the next token from the input.
    pub fn next_token(&mut self) -> Result<Token, ScanError> {
        if self.pos >= self.chars.len() {
            return Ok(Token::Eof);
        }

        self.token_count += 1;
        if self.token_count > MAX_TOKEN_COUNT {
            return Err(ScanError::TooManyTokens {
                max: MAX_TOKEN_COUNT,
            });
        }

        // Check if `[` starts a tag or is an escaped `[[`
        if self.chars.get(self.pos) == Some(&'[') {
            // `[[` is an escaped bracket — handle in scan_text so it merges
            // with surrounding text content
            if self.peek_ahead(1) == Some('[') {
                self.scan_text()
            } else {
                self.scan_tag_or_escape()
            }
        } else {
            self.scan_text()
        }
    }

    /// Scan text content until we hit a `[` or end of input.
    fn scan_text(&mut self) -> Result<Token, ScanError> {
        let mut cursor = self.pos;
        let mut requested = 0usize;
        while let Some(&ch) = self.chars.get(cursor) {
            if ch == '[' && self.chars.get(cursor + 1) != Some(&'[') {
                break;
            }
            let (output, advance) =
                if matches!(ch, '[' | ']') && self.chars.get(cursor + 1) == Some(&ch) {
                    (ch, 2)
                } else {
                    (ch, 1)
                };
            requested = requested
                .checked_add(output.len_utf8())
                .ok_or_else(|| allocation_error(usize::MAX))?;
            if requested > MAX_TEXT_TOKEN_SIZE {
                return Err(ScanError::TextTooLong {
                    max: MAX_TEXT_TOKEN_SIZE,
                });
            }
            cursor += advance;
        }
        let mut text = try_string(requested, ScannerAllocationSite::Text)?;

        while let Some(&ch) = self.chars.get(self.pos) {
            if ch == '[' {
                // Check for `[[` escape
                if self.peek_ahead(1) == Some('[') {
                    text.push('[');
                    self.pos += 2;
                } else {
                    // Start of a tag — stop collecting text
                    break;
                }
            } else if ch == ']' {
                // Check for `]]` escape
                if self.peek_ahead(1) == Some(']') {
                    text.push(']');
                    self.pos += 2;
                } else {
                    // Stray `]` — include it as text
                    text.push(']');
                    self.pos += 1;
                }
            } else {
                text.push(ch);
                self.pos += 1;
            }
        }

        Ok(Token::Text(text))
    }

    /// We're at a `[`. Determine if it's an open tag, close tag, or escaped bracket.
    fn scan_tag_or_escape(&mut self) -> Result<Token, ScanError> {
        let tag_start = self.pos;

        // `[[` is an escaped bracket — handled in scan_text,
        // but if we got here it means scan_text didn't catch it (shouldn't happen).
        if self.peek_ahead(1) == Some('[') {
            self.pos += 2;
            let mut text = try_string(1, ScannerAllocationSite::Text)?;
            text.push('[');
            return Ok(Token::Text(text));
        }

        // Skip the opening `[`
        self.pos += 1;

        // Check for close tag: `[/tagname]`
        if self.current_char() == Some('/') {
            self.pos += 1;
            return self.scan_close_tag(tag_start);
        }

        self.scan_open_tag(tag_start)
    }

    /// Scan a closing tag `[/tagname]`. We've already consumed `[/`.
    fn scan_close_tag(&mut self, tag_start: usize) -> Result<Token, ScanError> {
        self.skip_whitespace();

        let name = self.scan_tag_name()?;
        if name.is_empty() {
            return Err(ScanError::InvalidTagName { offset: tag_start });
        }

        self.skip_whitespace();

        if self.current_char() != Some(']') {
            return Err(ScanError::UnterminatedTag { offset: tag_start });
        }
        self.pos += 1;

        Ok(Token::CloseTag { name })
    }

    /// Scan an opening tag `[tagname attrs...]` or `[tagname attrs... /]`.
    /// We've already consumed `[`.
    fn scan_open_tag(&mut self, tag_start: usize) -> Result<Token, ScanError> {
        self.skip_whitespace();

        let name = self.scan_tag_name()?;
        if name.is_empty() {
            return Err(ScanError::InvalidTagName { offset: tag_start });
        }

        let mut attributes = Vec::new();
        let mut self_closing = false;

        loop {
            self.skip_whitespace();

            match self.current_char() {
                None => return Err(ScanError::UnterminatedTag { offset: tag_start }),
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                Some('/') => {
                    if self.peek_ahead(1) == Some(']') {
                        self_closing = true;
                        self.pos += 2;
                        break;
                    } else {
                        // `/` not followed by `]` — treat as part of attribute
                        try_reserve_vec(&mut attributes, 1, ScannerAllocationSite::Attributes)?;
                        if let Some(attr) = self.scan_attribute(tag_start)? {
                            attributes.push(attr);
                        }
                    }
                }
                Some(_) => {
                    try_reserve_vec(&mut attributes, 1, ScannerAllocationSite::Attributes)?;
                    if let Some(attr) = self.scan_attribute(tag_start)? {
                        attributes.push(attr);
                    }
                }
            }
        }

        Ok(Token::OpenTag {
            name,
            attributes,
            self_closing,
        })
    }

    /// Scan a tag name (lowercase alphanumeric + hyphens).
    fn scan_tag_name(&mut self) -> Result<String, ScanError> {
        let requested = self
            .remaining()
            .iter()
            .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
            .count();
        let mut name = try_string(requested, ScannerAllocationSite::Name)?;

        while let Some(ch) = self.current_char() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                name.push(ch.to_ascii_lowercase());
                self.pos += 1;
            } else {
                break;
            }
        }

        Ok(name)
    }

    /// Scan a single attribute: `name=value`, `name="value"`, or `name` (flag).
    /// Returns None if we couldn't parse anything meaningful.
    fn scan_attribute(&mut self, tag_start: usize) -> Result<Option<Attribute>, ScanError> {
        let attr_name = self.scan_attribute_name()?;
        if attr_name.is_empty() {
            // Skip one character to avoid infinite loop on unexpected input
            self.pos += 1;
            return Ok(None);
        }

        // Check for `=`
        if self.current_char() == Some('=') {
            self.pos += 1;

            let value = self.scan_attribute_value(tag_start)?;
            Ok(Some(Attribute {
                name: attr_name,
                value,
            }))
        } else {
            // Flag attribute (no value)
            Ok(Some(Attribute {
                name: attr_name,
                value: AttributeValue::Flag,
            }))
        }
    }

    /// Scan an attribute name (alphanumeric + hyphens + underscores).
    fn scan_attribute_name(&mut self) -> Result<String, ScanError> {
        let requested = self
            .remaining()
            .iter()
            .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
            .count();
        let mut name = try_string(requested, ScannerAllocationSite::Name)?;

        while let Some(ch) = self.current_char() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                name.push(ch.to_ascii_lowercase());
                self.pos += 1;
            } else {
                break;
            }
        }

        Ok(name)
    }

    /// Scan an attribute value — either quoted or unquoted.
    fn scan_attribute_value(&mut self, tag_start: usize) -> Result<AttributeValue, ScanError> {
        let value = match self.current_char() {
            Some('"') => {
                let s = self.scan_quoted_string(tag_start)?;
                AttributeValue::String(s)
            }
            _ => {
                let s = self.scan_unquoted_value()?;
                AttributeValue::Ident(s)
            }
        };
        let len = match &value {
            AttributeValue::String(s) | AttributeValue::Ident(s) => s.chars().count(),
            AttributeValue::Flag => 0,
        };
        if len > MAX_ATTRIBUTE_VALUE_CHARS {
            return Err(ScanError::AttributeValueTooLong {
                max: MAX_ATTRIBUTE_VALUE_CHARS,
            });
        }
        Ok(value)
    }

    /// Scan a double-quoted string with backslash escapes.
    fn scan_quoted_string(&mut self, _tag_start: usize) -> Result<String, ScanError> {
        let string_start = self.pos;
        let mut cursor = self.pos + 1;
        let mut requested = 0usize;
        let mut chars = 0usize;
        loop {
            let Some(ch) = self.chars.get(cursor).copied() else {
                return Err(ScanError::UnterminatedString {
                    offset: string_start,
                });
            };
            if ch == '"' {
                break;
            }
            if ch == '\\' {
                let Some(escaped) = self.chars.get(cursor + 1).copied() else {
                    return Err(ScanError::UnterminatedString {
                        offset: string_start,
                    });
                };
                let output_bytes = if matches!(escaped, '"' | '\\' | 'n' | 't') {
                    1
                } else {
                    1 + escaped.len_utf8()
                };
                requested = requested
                    .checked_add(output_bytes)
                    .ok_or_else(|| allocation_error(usize::MAX))?;
                chars = chars
                    .checked_add(if output_bytes == 1 { 1 } else { 2 })
                    .ok_or_else(|| allocation_error(usize::MAX))?;
                cursor += 2;
            } else {
                requested = requested
                    .checked_add(ch.len_utf8())
                    .ok_or_else(|| allocation_error(usize::MAX))?;
                chars = chars
                    .checked_add(1)
                    .ok_or_else(|| allocation_error(usize::MAX))?;
                cursor += 1;
            }
            if chars > MAX_ATTRIBUTE_VALUE_CHARS {
                return Err(ScanError::AttributeValueTooLong {
                    max: MAX_ATTRIBUTE_VALUE_CHARS,
                });
            }
        }
        // Skip opening quote
        self.pos += 1;

        let mut value = try_string(requested, ScannerAllocationSite::Value)?;

        loop {
            match self.current_char() {
                None => {
                    return Err(ScanError::UnterminatedString {
                        offset: string_start,
                    });
                }
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.current_char() {
                        Some('"') => {
                            value.push('"');
                            self.pos += 1;
                        }
                        Some('\\') => {
                            value.push('\\');
                            self.pos += 1;
                        }
                        Some('n') => {
                            value.push('\n');
                            self.pos += 1;
                        }
                        Some('t') => {
                            value.push('\t');
                            self.pos += 1;
                        }
                        Some(ch) => {
                            // Unknown escape — include the backslash and char
                            value.push('\\');
                            value.push(ch);
                            self.pos += 1;
                        }
                        None => {
                            return Err(ScanError::UnterminatedString {
                                offset: string_start,
                            });
                        }
                    }
                }
                Some(ch) => {
                    value.push(ch);
                    self.pos += 1;
                }
            }
        }

        Ok(value)
    }

    /// Scan an unquoted attribute value — ends at whitespace, `]`, or `/]`.
    /// Values are preserved as-is (case-sensitive). The parser handles
    /// case-insensitive comparison for keywords like color names.
    fn scan_unquoted_value(&mut self) -> Result<String, ScanError> {
        let mut requested = 0usize;
        let mut chars = 0usize;
        for ch in self.remaining() {
            if ch.is_whitespace() || *ch == ']' || *ch == '/' {
                break;
            }
            requested = requested
                .checked_add(ch.len_utf8())
                .ok_or_else(|| allocation_error(usize::MAX))?;
            chars += 1;
            if chars > MAX_ATTRIBUTE_VALUE_CHARS {
                return Err(ScanError::AttributeValueTooLong {
                    max: MAX_ATTRIBUTE_VALUE_CHARS,
                });
            }
        }
        let mut value = try_string(requested, ScannerAllocationSite::Value)?;

        while let Some(ch) = self.current_char() {
            if ch.is_whitespace() || ch == ']' || ch == '/' {
                break;
            }
            value.push(ch);
            self.pos += 1;
        }

        Ok(value)
    }

    /// Skip whitespace characters.
    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.current_char() {
            if ch.is_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Get the current character without advancing.
    fn current_char(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// Peek ahead by `n` positions.
    fn peek_ahead(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    /// The unconsumed input, empty once the cursor is past the end.
    ///
    /// `self.pos` never exceeds `self.chars.len()`, so the slice always
    /// resolves; taking it through `get` keeps that a property of this one
    /// function rather than of every caller.
    fn remaining(&self) -> &[char] {
        self.chars.get(self.pos..).unwrap_or(&[])
    }
}
