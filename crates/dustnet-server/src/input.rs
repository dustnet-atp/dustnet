//! Accepting form submissions.
//!
//! Without a handler the server refuses `INPUT` with 405, which is what it did
//! before this module existed and what `dustnetd` still does. Installing a
//! handler is what lets a site accept a submission.
//!
//! # Fields are ordered and duplicates are kept
//!
//! [`docs/spec/02-protocol.md`](../../../docs/spec/02-protocol.md) says of the
//! INPUT body: "Repeated field names are permitted and retain document order."
//! So [`FormFields`] is a `Vec`, not a map. Collapsing duplicates would quietly
//! discard data and, worse, make *which* value survives depend on whether the
//! implementation kept the first or the last — a difference an attacker picks
//! and a reviewer does not see.
//!
//! # What a handler is told, and what it returns
//!
//! A handler gets the path, any query riding on the form's action, and the
//! fields. It returns [`InputOutcome`] — which page to answer with, or a
//! refusal — and **never markup**. Rendering goes through the include resolver
//! either way, so there is exactly one place that generates a page and it
//! behaves the same whether or not a submission preceded it.
//!
//! An accepted submission is answered with the target *page*, not a redirect.
//! That is forced by the protocol rather than chosen: `validate_redirect_body`
//! requires an absolute `atp://` target, because the client resolves it with
//! `AtpUri::parse` rather than against the current URI — and a server does not
//! know its own public hostname, only the address it happens to be bound to.
//! Answering with the page is what
//! [`docs/spec/02-protocol.md`](../../../docs/spec/02-protocol.md) permits
//! ("The server responds with a PAGE, REDIRECT, or ERROR") and costs one round
//! trip instead of two.

/// Most fields one submission may carry.
///
/// The body is already bounded by `MAX_INPUT_MESSAGE_SIZE`, but a bounded body
/// can still hold a great many `&`s. This bounds the parsed vector too, so the
/// cost of a submission is bounded in both bytes and elements.
const MAX_FIELDS: usize = 64;

/// The decoded fields of one submission, in the order they were sent.
#[derive(Debug, Default, Clone)]
pub struct FormFields {
    fields: Vec<(String, String)>,
}

impl FormFields {
    /// Decode `key=value&key2=value2`, as the reference client encodes it:
    /// `+` for a space and `%XX` for anything else outside the unreserved set.
    ///
    /// Malformed input is decoded as far as it makes sense rather than
    /// rejected: a stray `%` is kept literally, and a pair with no `=` becomes
    /// a field with an empty value. A submission is not a place to be clever —
    /// the handler decides what it requires, and every value is untrusted
    /// either way.
    pub fn parse(encoded: &str) -> Self {
        let mut fields = Vec::new();
        for pair in encoded.split('&') {
            if pair.is_empty() || fields.len() >= MAX_FIELDS {
                continue;
            }
            let (name, value) = match pair.split_once('=') {
                Some((name, value)) => (name, value),
                None => (pair, ""),
            };
            fields.push((decode(name), decode(value)));
        }
        Self { fields }
    }

    /// The first value sent under `name`.
    ///
    /// First rather than last, and stated here because the choice is only
    /// invisible until two values disagree. A handler that cares which one it
    /// got should use [`FormFields::all`] and decide deliberately.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every value sent under `name`, in order.
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Percent-decode one form component, treating `+` as a space.
///
/// Builds bytes and then converts, so a multi-byte character split across
/// several `%XX` escapes reassembles. Invalid UTF-8 becomes U+FFFD rather than
/// an error: the field is untrusted text either way, and a replacement
/// character is a better answer than refusing a whole submission.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    // Indexed with `get` throughout: the crate denies `clippy::indexing_slicing`
    // because this is a parser over remote bytes, and "the bound is obvious
    // from the loop condition" is exactly the reasoning that goes stale when
    // someone edits the loop.
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                let pair = bytes
                    .get(index + 1..index + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok());
                match pair {
                    Some(decoded) => {
                        out.push(decoded);
                        index += 3;
                    }
                    // Truncated, or not hex: not an escape, so keep the `%`.
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a handler is told about a submission.
#[derive(Debug, Clone, Copy)]
pub struct InputRequest<'a> {
    /// The form's action path, with any query removed.
    pub path: &'a str,
    /// The query the action carried, if any — how a form says *which* thing it
    /// acts on, as in `[form action="/index?reply=12"]`.
    pub query: Option<&'a str>,
    /// The submitted fields.
    pub fields: &'a FormFields,
}

/// A handler's answer to a submission.
#[derive(Debug, Clone)]
pub enum InputOutcome {
    /// Accepted. The server answers with this page, rendered exactly as a GET
    /// for it would be — same include resolution, same everything — so there is
    /// one path that produces a page whether or not a submission preceded it.
    Render { path: String, query: Option<String> },
    /// Refused, with a reason to show the person who submitted it.
    Rejected(String),
}

impl InputOutcome {
    /// Accept, and answer with `path` and no query.
    pub fn render(path: impl Into<String>) -> Self {
        Self::Render {
            path: path.into(),
            query: None,
        }
    }

    /// Accept, and answer with `path?query`.
    pub fn render_query(path: impl Into<String>, query: impl Into<String>) -> Self {
        Self::Render {
            path: path.into(),
            query: Some(query.into()),
        }
    }
}

/// Handles form submissions for a site.
pub trait InputHandler: Send + Sync {
    fn handle(&self, request: &InputRequest<'_>) -> InputOutcome;
}

/// Split a form action into its path and query.
pub(crate) fn split_query(action: &str) -> (&str, Option<&str>) {
    match action.split_once('?') {
        Some((path, query)) if !query.is_empty() => (path, Some(query)),
        Some((path, _)) => (path, None),
        None => (action, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_submission() {
        let fields = FormFields::parse("title=Hello&url=atp%3A%2F%2Fx.dust");
        assert_eq!(fields.get("title"), Some("Hello"));
        assert_eq!(fields.get("url"), Some("atp://x.dust"));
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn plus_is_a_space() {
        let fields = FormFields::parse("q=hello+world");
        assert_eq!(fields.get("q"), Some("hello world"));
    }

    /// A multi-byte character arrives as consecutive escapes and must be
    /// reassembled, not decoded byte by byte into mojibake.
    #[test]
    fn multibyte_escapes_reassemble() {
        let fields = FormFields::parse("t=caf%C3%A9+%E2%98%85");
        assert_eq!(fields.get("t"), Some("café ★"));
    }

    /// Order is preserved and duplicates are kept, which the protocol
    /// specifies. Collapsing them would make which value wins an accident.
    #[test]
    fn duplicate_names_are_all_retained_in_order() {
        let fields = FormFields::parse("tag=a&tag=b&tag=c");
        assert_eq!(fields.all("tag").collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(fields.get("tag"), Some("a"), "get returns the first");
    }

    #[test]
    fn malformed_input_decodes_as_far_as_it_can() {
        let fields = FormFields::parse("bare&a=%zz&b=%&c=x");
        assert_eq!(fields.get("bare"), Some(""), "no = means empty value");
        assert_eq!(fields.get("a"), Some("%zz"), "not hex, so not an escape");
        assert_eq!(fields.get("b"), Some("%"), "truncated escape");
        assert_eq!(fields.get("c"), Some("x"));
    }

    /// A bounded body can still carry a great many separators, so the parsed
    /// vector is bounded too.
    #[test]
    fn field_count_is_bounded() {
        let encoded = (0..MAX_FIELDS * 4)
            .map(|index| format!("f{index}=v"))
            .collect::<Vec<_>>()
            .join("&");
        assert_eq!(FormFields::parse(&encoded).len(), MAX_FIELDS);
    }

    #[test]
    fn an_empty_body_is_no_fields() {
        assert!(FormFields::parse("").is_empty());
    }

    /// A form action carries which thing it acts on as a query.
    #[test]
    fn an_action_query_is_split_off() {
        assert_eq!(split_query("/index?reply=12"), ("/index", Some("reply=12")));
        assert_eq!(split_query("/index"), ("/index", None));
        assert_eq!(split_query("/index?"), ("/index", None));
    }

    #[test]
    fn outcome_constructors_carry_the_query() {
        match InputOutcome::render("/index") {
            InputOutcome::Render { path, query } => {
                assert_eq!(path, "/index");
                assert_eq!(query, None);
            }
            other => panic!("expected Render, got {other:?}"),
        }
        match InputOutcome::render_query("/index", "item=7") {
            InputOutcome::Render { path, query } => {
                assert_eq!(path, "/index");
                assert_eq!(query.as_deref(), Some("item=7"));
            }
            other => panic!("expected Render, got {other:?}"),
        }
    }

    /// Values are never trusted, and nothing here tries to make them safe:
    /// escaping is the serializer's job, at the point markup is written.
    /// This pins that a hostile value survives decoding intact so the handler
    /// sees exactly what was sent.
    #[test]
    fn hostile_values_are_delivered_verbatim() {
        let hostile = r#"x" ][form action="/login"]"#;
        let encoded = format!(
            "title={}",
            hostile
                .bytes()
                .map(|byte| format!("%{byte:02X}"))
                .collect::<String>()
        );
        assert_eq!(FormFields::parse(&encoded).get("title"), Some(hostile));
    }
}
