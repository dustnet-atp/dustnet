//! AML serialization: a token stream back to AML text.
//!
//! This exists so that a server generating AML never builds it by
//! concatenating strings. Composing a page by `format!`-ing user data between
//! literal brackets makes every interpolation site a place where a missed
//! escape becomes markup injection — a forged `[form action="/login"]` inside a
//! comment body is a phishing form on an origin the reader trusts. The failure
//! mode is not hypothetical: the quarantined prototype in
//! `examples/unsupported-social` escaped a link's title, author and domain and
//! then interpolated the submitted URL raw on the next line.
//!
//! The fix is to make the escape structural rather than remembered. Content
//! becomes [`Token`]s — user text is a [`Token::Text`], never a fragment of
//! markup — and this module is the single place that turns tokens into
//! characters. There is nowhere else for an escape to be forgotten, because
//! there is nowhere else that writes a `[`.
//!
//! # Why tokens and not the AST
//!
//! [`Token`] has four variants and already derives `PartialEq`, so the
//! correctness property can be stated directly and mechanically:
//!
//! ```text
//! scan(to_aml(tokens)) == tokens
//! ```
//!
//! That compares tag names, every attribute, every value and the self-closing
//! flag, so nothing can be quietly dropped or mangled — a weaker fixed-point
//! property would be satisfied by a serializer that discarded attributes
//! consistently. `Element` has 37 variants and no `PartialEq`, and re-encoding
//! an authored page through it would risk fidelity loss for no gain: authored
//! AML is trusted, and only generated content needs composing.
//!
//! # The escaping contexts
//!
//! Two contexts, with different rules, which is the reason a single
//! `escape()` helper would be wrong:
//!
//! | Context | Escaped |
//! |---|---|
//! | Text content | `[` → `[[`, `]` → `]]` |
//! | Quoted attribute value | `\` → `\\`, `"` → `\"`, newline → `\n`, tab → `\t` |
//!
//! Both are the inverse of what [`crate::scanner`] actually decodes, which is
//! deliberately *not* the same as the escape table in
//! `docs/spec/03-markup.md`. The spec lists `\\` as producing a literal `\` in
//! content; the scanner's text path has no backslash case at all, so a
//! backslash in text is already literal and escaping it here would round-trip
//! as two. Where the two disagree the scanner wins, because the scanner is what
//! will read this back.
//!
//! `$` is likewise left alone. It introduces attribute substitution only inside
//! a `[def]` component body — every `substitute_attrs` call site sits in
//! `expand_component_body` — so it is inert in ordinary content. Generated
//! content must therefore not be placed inside a component body; a `[def]` is
//! the one context where these rules do not hold.
//!
//! # URLs are not covered here
//!
//! Escaping a URL into an attribute makes it *syntactically* safe, not
//! *semantically* safe: `[link href="https://evil.example/"]` is perfectly
//! well-formed and still a phishing link. Scheme and authority checking belongs
//! where the URL enters the system, against [`crate::uri`], not here.

use crate::scanner::{Attribute, AttributeValue, Token};

/// Why a token stream could not be serialized.
///
/// Only names fail. Values are always representable, because any byte can be
/// escaped into a quoted attribute value or into text — which is the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SerializeError {
    /// A tag name the scanner could not read back as the same name.
    TagName(String),
    /// An attribute name the scanner could not read back as the same name.
    AttributeName(String),
}

impl core::fmt::Display for SerializeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TagName(name) => {
                write!(f, "tag name `{name}` is not a valid AML name")
            }
            Self::AttributeName(name) => {
                write!(f, "attribute name `{name}` is not a valid AML name")
            }
        }
    }
}

impl core::error::Error for SerializeError {}

/// Whether `name` survives a scan unchanged.
///
/// The scanner accepts ASCII alphanumerics, `-` and `_`, and **lowercases**
/// what it reads. An uppercase name would therefore come back different, so it
/// is rejected rather than silently normalised: a caller that meant `[Box]` has
/// a bug worth hearing about, and silently rewriting it would break the
/// round-trip property this module is verified against.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

/// Append `text` as AML text content, escaping the bracket metacharacters.
///
/// A lone `]` scans as a literal `]` already, so escaping it is not strictly
/// required — but `]]` also decodes to `]`, and escaping both brackets keeps
/// the rule symmetrical and the output independent of what follows it.
fn push_text(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '[' => out.push_str("[["),
            ']' => out.push_str("]]"),
            _ => out.push(ch),
        }
    }
}

/// Append `value` as the body of a quoted attribute value.
///
/// Newline and tab are escaped because a raw one inside a quoted value is
/// preserved verbatim by the scanner and would make the emitted AML span lines
/// mid-attribute — legal, but unreadable, and it makes a diff of generated
/// output useless.
fn push_attr_value(value: &str, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
}

/// Serialize one attribute.
///
/// Values are always emitted quoted, never as a bare identifier. The parser
/// reads both through the same `attr_str_value`, which matches
/// `AttributeValue::String(s) | AttributeValue::Ident(s) => s`, so quoting
/// changes no meaning — and it removes a whole class of hazard, since an
/// unquoted value containing a space or `]` would scan back as several
/// attributes or terminate the tag.
fn push_attribute(attr: &Attribute, out: &mut String) -> Result<(), SerializeError> {
    if !is_valid_name(&attr.name) {
        return Err(SerializeError::AttributeName(attr.name.clone()));
    }
    out.push(' ');
    out.push_str(&attr.name);
    match &attr.value {
        AttributeValue::Flag => {}
        AttributeValue::String(value) | AttributeValue::Ident(value) => {
            out.push_str("=\"");
            push_attr_value(value, out);
            out.push('"');
        }
    }
    Ok(())
}

/// Serialize a token stream to AML text.
///
/// Scanning the result yields the tokens back, modulo the two normalisations
/// the scanner cannot express: adjacent [`Token::Text`] tokens merge into one,
/// and an empty one disappears. [`Token::Eof`] emits nothing.
///
/// Prefer this over [`Token`]'s `Display`, which writes attribute values and
/// text verbatim and so cannot be used to build AML from untrusted data.
pub fn to_aml(tokens: &[Token]) -> Result<String, SerializeError> {
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Text(text) => push_text(text, &mut out),
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } => {
                if !is_valid_name(name) {
                    return Err(SerializeError::TagName(name.clone()));
                }
                out.push('[');
                out.push_str(name);
                for attr in attributes {
                    push_attribute(attr, &mut out)?;
                }
                if *self_closing {
                    out.push_str(" /]");
                } else {
                    out.push(']');
                }
            }
            Token::CloseTag { name } => {
                if !is_valid_name(name) {
                    return Err(SerializeError::TagName(name.clone()));
                }
                out.push_str("[/");
                out.push_str(name);
                out.push(']');
            }
            Token::Eof => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::Scanner;

    fn scan(src: &str) -> Vec<Token> {
        Scanner::new(src.as_bytes())
            .expect("scanner rejected input")
            .scan_all()
            .expect("scan failed")
    }

    /// Collapse the differences that are deliberate, so the round-trip
    /// property compares only what is meant to be preserved. Each one is a
    /// choice made elsewhere in this module, not a fudge:
    ///
    /// - `Eof` is dropped: it emits no characters, so a hand-built stream has
    ///   none and a scanned one always ends with it.
    /// - Adjacent text runs merge and empty ones vanish: the scanner cannot
    ///   express either, since text is whatever lies between two tags.
    /// - `Ident` becomes `String`: values are always emitted quoted, which the
    ///   parser reads identically through `attr_str_value`.
    fn normalize(tokens: &[Token]) -> Vec<Token> {
        let mut out: Vec<Token> = Vec::new();
        for token in tokens {
            match (out.last_mut(), token) {
                (_, Token::Eof) => {}
                (_, Token::Text(text)) if text.is_empty() => {}
                (Some(Token::Text(prev)), Token::Text(text)) => prev.push_str(text),
                _ => out.push(quote_values(token)),
            }
        }
        out
    }

    /// Rewrite every `Ident` value as the `String` the serializer emits.
    fn quote_values(token: &Token) -> Token {
        let mut token = token.clone();
        if let Token::OpenTag { attributes, .. } = &mut token {
            for attr in attributes {
                if let AttributeValue::Ident(value) = &attr.value {
                    attr.value = AttributeValue::String(value.clone());
                }
            }
        }
        token
    }

    fn assert_round_trips(src: &str) {
        let tokens = normalize(&scan(src));
        let aml = to_aml(&tokens).expect("serialization failed");
        let reparsed = normalize(&scan(&aml));
        assert_eq!(
            tokens, reparsed,
            "round trip changed the token stream\n  source: {src:?}\n  emitted: {aml:?}"
        );
    }

    /// The property the module exists to hold. Stated over the token stream
    /// rather than the text, because the text is expected to change: values get
    /// quoted and brackets get escaped.
    #[test]
    fn round_trips_every_construct() {
        for src in [
            "[page mode=document][text]hello[/text][/page]",
            "[page mode=document][spacer lines=1 /][/page]",
            "[page mode=document][text bold dim fg=red]x[/text][/page]",
            r#"[page mode=document][link href="/a?b=c&d=e"]go[/link][/page]"#,
            "[page mode=document][text]a [text bold]b[/text] c[/text][/page]",
            "[page mode=document][hr style=single /][/page]",
            r#"[page mode=document][input name="q" maxlen=20 password /][/page]"#,
            "[page mode=document][box border=rounded][text]in[/text][/box][/page]",
        ] {
            assert_round_trips(src);
        }
    }

    /// Every metacharacter, in each context, in one document.
    #[test]
    fn round_trips_metacharacters_in_both_contexts() {
        assert_round_trips(r#"[page mode=document][text]a[[b]]c$d\e[/text][/page]"#);
        assert_round_trips(r#"[page mode=document][link href="a\"b\\c"]t[/link][/page]"#);
        assert_round_trips("[page mode=document][text]trailing backslash \\[/text][/page]");
        assert_round_trips("[page mode=document][text]$$literal$dollar[/text][/page]");
    }

    /// The whole point, stated as a test: text that looks like markup stays
    /// text. This is the shape that made the prototype's raw URL
    /// interpolation exploitable — a submitted value closing its attribute and
    /// opening a login form.
    #[test]
    fn text_that_looks_like_markup_survives_as_text() {
        let hostile = r#"x" ][form action="/login"][input name="password" password /][/form]"#;
        let tokens = vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: Vec::new(),
                self_closing: false,
            },
            Token::Text(hostile.into()),
            Token::CloseTag {
                name: "text".into(),
            },
        ];
        let aml = to_aml(&tokens).expect("serialization failed");
        let reparsed = normalize(&scan(&aml));

        // One text token, carrying the hostile string verbatim — no form, no
        // input, no extra attributes on the enclosing [text].
        assert_eq!(reparsed, normalize(&tokens));
        assert!(
            !reparsed.iter().any(|t| matches!(
                t,
                Token::OpenTag { name, .. } if name == "form" || name == "input"
            )),
            "injected markup became elements: {aml}"
        );
    }

    /// The same shape, but arriving through an attribute value rather than
    /// text — the exact site of the prototype's defect.
    #[test]
    fn hostile_attribute_value_cannot_close_its_own_quote() {
        let hostile = r#"https://ok.example/" ][form action="/login"]"#;
        let tokens = vec![Token::OpenTag {
            name: "link".into(),
            attributes: vec![Attribute {
                name: "href".into(),
                value: AttributeValue::String(hostile.into()),
            }],
            self_closing: true,
        }];
        let aml = to_aml(&tokens).expect("serialization failed");
        let reparsed = normalize(&scan(&aml));

        // The whole hostile string came back as one attribute value, so it
        // never closed its quote: the `[form` in it is data, not a tag.
        assert_eq!(reparsed, normalize(&tokens));
        assert_eq!(
            reparsed.len(),
            1,
            "hostile value produced extra tokens: {aml}"
        );
        assert!(
            aml.contains(r#"\" ][form"#),
            "the embedded quote should be backslash-escaped: {aml}"
        );
    }

    /// A bare identifier value becomes a quoted one. Meaning is unchanged —
    /// the parser reads both through `attr_str_value` — so the round-trip
    /// property is stated over already-quoted input, and this pins the
    /// normalisation explicitly rather than leaving it implied.
    #[test]
    fn unquoted_values_are_emitted_quoted() {
        let src = "[text fg=red]x[/text]";
        let aml = to_aml(&scan(src)).expect("serialization failed");
        assert!(aml.starts_with(r#"[text fg="red"]"#), "{aml}");
        assert_eq!(normalize(&scan(&aml)), normalize(&scan(src)));
    }

    /// A value containing a space is *unrepresentable* unquoted: it would scan
    /// back as two attributes. Always quoting is what makes it representable,
    /// so this is the case that justifies the choice.
    #[test]
    fn value_with_space_round_trips_because_it_is_quoted() {
        let tokens = vec![Token::OpenTag {
            name: "box".into(),
            attributes: vec![Attribute {
                name: "title".into(),
                value: AttributeValue::String("two words".into()),
            }],
            self_closing: true,
        }];
        let aml = to_aml(&tokens).expect("serialization failed");
        assert_eq!(normalize(&scan(&aml)), normalize(&tokens));
    }

    #[test]
    fn rejects_names_that_would_not_survive_a_scan() {
        for name in ["Box", "has space", "", "b!ng"] {
            let tokens = vec![Token::OpenTag {
                name: name.into(),
                attributes: Vec::new(),
                self_closing: true,
            }];
            assert_eq!(
                to_aml(&tokens),
                Err(SerializeError::TagName(name.to_string())),
                "accepted tag name {name:?}"
            );
        }

        let tokens = vec![Token::OpenTag {
            name: "text".into(),
            attributes: vec![Attribute {
                name: "Bold".into(),
                value: AttributeValue::Flag,
            }],
            self_closing: true,
        }];
        assert_eq!(
            to_aml(&tokens),
            Err(SerializeError::AttributeName("Bold".to_string()))
        );
    }

    #[test]
    fn eof_emits_nothing() {
        assert_eq!(to_aml(&[Token::Eof]).unwrap(), "");
    }
}
