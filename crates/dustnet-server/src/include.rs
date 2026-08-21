//! Resolving `[include name=...]` placeholders into generated content.
//!
//! A page may name places a server fills: `[include name="links" /]`. This
//! module replaces those placeholders before the page goes on the wire, and it
//! is the only thing in the server that turns a request into generated markup.
//!
//! # What this does not do
//!
//! It does not make the server dynamic in the sense
//! [`docs/spec/07-security.md`](../../../docs/spec/07-security.md) rules out. A
//! resolver cannot see the connection, the peer, the session or the filesystem
//! — it is handed a name and a request path and returns tokens. `INPUT` is
//! still refused with 405, there is no authentication, and `dustnetd` passes no
//! resolver at all, so its behaviour is byte-for-byte what it was: an
//! `[include]` reaches the client, which renders it as nothing.
//!
//! # Why tokens rather than a string
//!
//! A resolver returns [`Token`]s and never text. That is the whole safety
//! argument: user-submitted content becomes a [`Token::Text`], and
//! [`to_aml`] is the single place that writes a `[`, so a comment body cannot
//! become a `[form]` however hostile it is. A resolver returning `String` would
//! put the escaping burden back on every handler, which is the mistake
//! `examples/unsupported-social` made — see [`dustnet_core::serialize`].
//!
//! # One pass, no recursion
//!
//! Resolution walks the authored token stream once. Tokens a resolver returns
//! are spliced in and **not** rescanned, and a resolver that returns an
//! `[include]` is an error rather than a second round: an include resolving to
//! an include is either a mistake or an attempt to make the server loop, and
//! neither deserves support.

use dustnet_core::protocol::ProtocolError;
use dustnet_core::scanner::{Scanner, Token};
use dustnet_core::serialize::to_aml;

/// The element name a placeholder uses, and the attribute naming its handler.
const INCLUDE_TAG: &str = "include";
const NAME_ATTR: &str = "name";

/// What a resolver is told about the request being served.
///
/// Deliberately thin. A resolver gets the path, the query and who is asking —
/// no peer address, no headers, no connection, and **no session token** — so
/// that generating a page cannot widen what a site learns about its reader, and
/// cannot leak a bearer credential into the page it produces.
#[derive(Debug, Clone, Copy)]
pub struct IncludeRequest<'a> {
    /// The requested path, as it appeared in the GET.
    pub path: &'a str,
    /// The query string, without the leading `?`, if the request carried one.
    pub query: Option<&'a str>,
    /// Who is asking, resolved from their session token, or `None` for
    /// anonymous. A token that did not resolve is `None` too: a resolver must
    /// not be able to tell "unknown token" from "no token".
    pub identity: Option<&'a str>,
}

/// Produces content for named `[include]` placeholders.
///
/// `resolve` returns `None` for a name this resolver does not claim, which is
/// not an error: it means no handler owns that placeholder, and the include is
/// dropped so the page renders without it. That matches what a client does with
/// an unresolved include, so a missing handler looks the same whichever side
/// notices it.
pub trait IncludeResolver: Send + Sync {
    fn resolve(&self, name: &str, request: &IncludeRequest<'_>) -> Option<Vec<Token>>;
}

/// The `name` attribute of an `[include]` open tag, if it has one.
fn include_name(attributes: &[dustnet_core::scanner::Attribute]) -> Option<&str> {
    attributes
        .iter()
        .find(|attribute| attribute.name == NAME_ATTR)
        .map(|attribute| match &attribute.value {
            dustnet_core::scanner::AttributeValue::String(value)
            | dustnet_core::scanner::AttributeValue::Ident(value) => value.as_str(),
            dustnet_core::scanner::AttributeValue::Flag => "",
        })
        .filter(|name| !name.is_empty())
}

/// Whether a token is an `[include]` tag of either kind.
fn is_include(token: &Token) -> bool {
    match token {
        Token::OpenTag { name, .. } | Token::CloseTag { name } => name == INCLUDE_TAG,
        _ => false,
    }
}

/// Replace every `[include]` in `content` with what `resolver` provides.
///
/// Returns `content` untouched when it names no includes. That is not only an
/// optimisation: an authored page is trusted input, and re-encoding one that
/// needs no substitution would risk changing it for no reason.
///
/// A `[/include]` is dropped alongside its opening tag. The parser treats
/// `[include]` as self-closing, so a closing tag is already meaningless; leaving
/// it in the output would surface as a stray close tag on the client.
pub fn resolve_page(
    content: &str,
    resolver: &dyn IncludeResolver,
    request: &IncludeRequest<'_>,
) -> Result<String, ProtocolError> {
    if !content.contains("[include") {
        return Ok(content.to_string());
    }

    let mut scanner = Scanner::new(content.as_bytes())
        .map_err(|_| ProtocolError::InvalidMessage("page is not scannable AML".into()))?;
    let tokens = scanner
        .scan_all()
        .map_err(|_| ProtocolError::InvalidMessage("page is not scannable AML".into()))?;

    let mut resolved: Vec<Token> = Vec::new();
    for token in &tokens {
        match token {
            Token::OpenTag {
                name, attributes, ..
            } if name == INCLUDE_TAG => {
                let Some(include) = include_name(attributes) else {
                    // A nameless include names no handler, so nothing can claim
                    // it. `dustnet check` reports it as E011; serving drops it
                    // rather than refusing the whole page over one placeholder.
                    continue;
                };
                let Some(generated) = resolver.resolve(include, request) else {
                    continue;
                };
                if generated.iter().any(is_include) {
                    return Err(ProtocolError::InvalidMessage(
                        "resolver returned an [include]; resolution is a single pass".into(),
                    ));
                }
                resolved.extend(generated);
            }
            Token::CloseTag { name } if name == INCLUDE_TAG => continue,
            other => resolved.push(other.clone()),
        }
    }

    to_aml(&resolved)
        .map_err(|error| ProtocolError::InvalidMessage(format!("generated AML: {error}").into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dustnet_core::scanner::{Attribute, AttributeValue};

    /// A resolver built from a list of `(name, tokens)` pairs.
    struct Fixed(Vec<(&'static str, Vec<Token>)>);

    impl IncludeResolver for Fixed {
        fn resolve(&self, name: &str, _request: &IncludeRequest<'_>) -> Option<Vec<Token>> {
            self.0
                .iter()
                .find(|(claimed, _)| *claimed == name)
                .map(|(_, tokens)| tokens.clone())
        }
    }

    fn text(body: &str) -> Vec<Token> {
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: Vec::new(),
                self_closing: false,
            },
            Token::Text(body.into()),
            Token::CloseTag {
                name: "text".into(),
            },
        ]
    }

    fn request() -> IncludeRequest<'static> {
        IncludeRequest {
            path: "/index.aml",
            query: None,
            identity: None,
        }
    }

    fn resolve(content: &str, resolver: &dyn IncludeResolver) -> String {
        resolve_page(content, resolver, &request()).expect("resolution failed")
    }

    #[test]
    fn substitutes_a_claimed_include() {
        let resolver = Fixed(vec![("links", text("a story"))]);
        let out = resolve(
            r#"[page mode=document][include name="links" /][/page]"#,
            &resolver,
        );
        assert!(out.contains("a story"), "{out}");
        assert!(!out.contains("include"), "{out}");
    }

    /// An unclaimed name is not an error. The page serves without it, which is
    /// what the client would do with the include anyway.
    #[test]
    fn drops_an_unclaimed_include() {
        let resolver = Fixed(Vec::new());
        let out = resolve(
            r#"[page mode=document][text]before[/text][include name="links" /][/page]"#,
            &resolver,
        );
        assert!(out.contains("before"), "{out}");
        assert!(!out.contains("include"), "{out}");
        assert!(!out.contains("links"), "{out}");
    }

    /// A page with no includes is returned byte-for-byte. Authored AML is
    /// trusted and there is no reason to re-encode it.
    #[test]
    fn leaves_a_page_without_includes_untouched() {
        let source = r#"[page mode=document][text fg=red]x[/text][/page]"#;
        let out = resolve(source, &Fixed(Vec::new()));
        assert_eq!(out, source);
    }

    /// The property the whole design exists for: a resolver returning
    /// user-submitted text that looks like markup produces text, not markup.
    #[test]
    fn generated_user_text_cannot_become_markup() {
        let hostile = r#"x" ][form action="/login"][input name="password" password /][/form]"#;
        let resolver = Fixed(vec![("links", text(hostile))]);
        let out = resolve(
            r#"[page mode=document][include name="links" /][/page]"#,
            &resolver,
        );

        // Scan what we would put on the wire and confirm no form appeared.
        let tokens = Scanner::new(out.as_bytes())
            .expect("output is scannable")
            .scan_all()
            .expect("output scans");
        assert!(
            !tokens.iter().any(|token| matches!(
                token,
                Token::OpenTag { name, .. } if name == "form" || name == "input"
            )),
            "injected markup became elements: {out}"
        );
        // And the hostile string is still there, as text.
        assert!(tokens.iter().any(
            |token| matches!(token, Token::Text(body) if body.contains(r#"[form action="/login"]"#))
        ));
    }

    /// A hostile *attribute* value, which is the shape the prototype's raw URL
    /// interpolation actually got wrong.
    #[test]
    fn generated_attribute_values_cannot_close_their_quote() {
        let hostile = r#"https://ok.example/" ][form action="/login"]"#;
        let resolver = Fixed(vec![(
            "links",
            vec![Token::OpenTag {
                name: "link".into(),
                attributes: vec![Attribute {
                    name: "href".into(),
                    value: AttributeValue::String(hostile.into()),
                }],
                self_closing: true,
            }],
        )]);
        let out = resolve(
            r#"[page mode=document][include name="links" /][/page]"#,
            &resolver,
        );
        let tokens = Scanner::new(out.as_bytes())
            .expect("output is scannable")
            .scan_all()
            .expect("output scans");
        assert!(
            !tokens.iter().any(|token| matches!(
                token,
                Token::OpenTag { name, .. } if name == "form"
            )),
            "attribute escaped its quote: {out}"
        );
    }

    /// Resolution is a single pass, so a resolver cannot make the server loop
    /// by returning another placeholder.
    #[test]
    fn a_resolver_returning_an_include_is_refused() {
        let resolver = Fixed(vec![(
            "links",
            vec![Token::OpenTag {
                name: "include".into(),
                attributes: vec![Attribute {
                    name: "name".into(),
                    value: AttributeValue::String("links".into()),
                }],
                self_closing: true,
            }],
        )]);
        let error = resolve_page(
            r#"[page mode=document][include name="links" /][/page]"#,
            &resolver,
            &request(),
        )
        .expect_err("expected refusal");
        assert!(format!("{error}").contains("single pass"), "{error}");
    }

    #[test]
    fn a_nameless_include_is_dropped() {
        let out = resolve("[page mode=document][include /][/page]", &Fixed(Vec::new()));
        assert!(!out.contains("include"), "{out}");
    }

    /// Several placeholders on one page, each claimed by a different name, and
    /// resolved in source order.
    #[test]
    fn resolves_every_placeholder_independently() {
        let resolver = Fixed(vec![("account", text("log in")), ("links", text("story"))]);
        let out = resolve(
            r#"[page mode=document][include name="account" /][include name="links" /][/page]"#,
            &resolver,
        );
        let account = out.find("log in").expect("account content missing");
        let links = out.find("story").expect("links content missing");
        assert!(account < links, "resolved out of source order: {out}");
    }

    /// A closing tag is dropped with its opener rather than left to surface as
    /// a stray close tag on the client.
    #[test]
    fn a_closing_include_tag_is_dropped_too() {
        let resolver = Fixed(vec![("links", text("story"))]);
        let out = resolve(
            r#"[page mode=document][include name="links"][/include][/page]"#,
            &resolver,
        );
        assert!(out.contains("story"), "{out}");
        assert!(!out.contains("include"), "{out}");
    }
}
