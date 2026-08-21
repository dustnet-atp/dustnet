#![no_main]

use libfuzzer_sys::fuzz_target;

use dustnet_core::scanner::{AttributeValue, Scanner, Token};
use dustnet_core::serialize::to_aml;

// Fuzz the AML serializer's round-trip property:
//
//     scan(to_aml(scan(data))) == scan(data)
//
// This is the property the serializer exists to hold. A server generating AML
// composes tokens and lets `to_aml` write the characters, so that user-supplied
// text cannot become markup. If the round trip can be broken, then some byte
// sequence escapes its context — which is markup injection, and on a page with
// a login form that is credential phishing on a trusted origin.
//
// Fuzzing from arbitrary bytes rather than a token generator is deliberate:
// tokens that came from the scanner are exactly the tokens a real page produces,
// so a counterexample here is directly an authorable document.
//
// The two normalisations mirror `serialize::tests::normalize`. Each is a
// deliberate property of the serializer, not a way to make the assertion pass:
// `Eof` emits nothing, the scanner cannot express an empty or a split text run,
// and values are always emitted quoted.

fn normalize(tokens: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    for token in tokens {
        match (out.last_mut(), token) {
            (_, Token::Eof) => {}
            (_, Token::Text(text)) if text.is_empty() => {}
            (Some(Token::Text(previous)), Token::Text(text)) => previous.push_str(text),
            _ => {
                let mut token = token.clone();
                if let Token::OpenTag { attributes, .. } = &mut token {
                    for attribute in attributes {
                        if let AttributeValue::Ident(value) = &attribute.value {
                            attribute.value = AttributeValue::String(value.clone());
                        }
                    }
                }
                out.push(token);
            }
        }
    }
    out
}

fn scan(bytes: &[u8]) -> Option<Vec<Token>> {
    Scanner::new(bytes).ok()?.scan_all().ok()
}

fuzz_target!(|data: &[u8]| {
    let Some(tokens) = scan(data) else {
        return; // size/utf8/limit rejection is fine
    };
    let tokens = normalize(&tokens);

    // Names come from the scanner, so they are always representable; a failure
    // here would mean the two disagree about what a name is.
    let Ok(emitted) = to_aml(&tokens) else {
        panic!("scanner produced tokens the serializer cannot emit: {tokens:?}");
    };

    // Re-scanning our own output must succeed: emitting AML the scanner then
    // refuses is itself a defect, and one that would strand a generated page.
    let Some(reparsed) = scan(emitted.as_bytes()) else {
        panic!("serializer emitted AML the scanner rejects: {emitted:?}");
    };

    assert_eq!(
        tokens,
        normalize(&reparsed),
        "round trip changed the token stream; emitted: {emitted:?}"
    );
});
