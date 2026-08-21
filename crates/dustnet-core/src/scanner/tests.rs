use super::*;

fn scan(input: &str) -> Vec<Token> {
    Scanner::new(input.as_bytes()).unwrap().scan_all().unwrap()
}

fn scan_err(input: &str) -> ScanError {
    let mut scanner = Scanner::new(input.as_bytes()).unwrap();
    scanner.scan_all().unwrap_err()
}

fn reject_once(site: ScannerAllocationSite, input: &str, scan_all: bool) -> ScanError {
    REJECT_ALLOCATION.with(|rejected| rejected.set(Some(site)));
    let result = Scanner::new(input.as_bytes()).and_then(|mut scanner| {
        if scan_all {
            scanner.scan_all().map(|_| ())
        } else {
            scanner.next_token().map(|_| ())
        }
    });
    REJECT_ALLOCATION.with(|rejected| rejected.set(None));
    result.unwrap_err()
}

#[test]
fn scanner_allocation_rejection_is_recoverable_at_every_site() {
    for (site, input, scan_all) in [
        (
            ScannerAllocationSite::Sanitized,
            "[text]hello[/text]",
            false,
        ),
        (ScannerAllocationSite::Chars, "[text]hello[/text]", false),
        (ScannerAllocationSite::Tokens, "[text]hello[/text]", true),
        (ScannerAllocationSite::Text, "hello", false),
        (ScannerAllocationSite::Attributes, "[text fg=red]", false),
        (ScannerAllocationSite::Name, "[text]", false),
        (ScannerAllocationSite::Value, "[text fg=red]", false),
    ] {
        assert!(matches!(
            reject_once(site, input, scan_all),
            ScanError::ResourceExhausted { .. }
        ));
        assert!(!scan(input).is_empty());
    }
}

// ─── Basic Tags ──────────────────────────────────────────────

#[test]
fn simple_text() {
    let tokens = scan("hello world");
    assert_eq!(tokens, vec![Token::Text("hello world".into()), Token::Eof]);
}

#[test]
fn simple_open_close() {
    let tokens = scan("[text]hello[/text]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::Text("hello".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Eof,
        ]
    );
}

#[test]
fn self_closing_tag() {
    let tokens = scan("[hr /]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "hr".into(),
                attributes: vec![],
                self_closing: true,
            },
            Token::Eof,
        ]
    );
}

#[test]
fn self_closing_br() {
    let tokens = scan("[br /]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "br".into(),
                attributes: vec![],
                self_closing: true,
            },
            Token::Eof,
        ]
    );
}

// ─── Attributes ──────────────────────────────────────────────

#[test]
fn flag_attribute() {
    let tokens = scan("[text bold]hello[/text]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![Attribute {
                    name: "bold".into(),
                    value: AttributeValue::Flag,
                }],
                self_closing: false,
            },
            Token::Text("hello".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Eof,
        ]
    );
}

#[test]
fn ident_attribute() {
    let tokens = scan("[text fg=red]hello[/text]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![Attribute {
                    name: "fg".into(),
                    value: AttributeValue::Ident("red".into()),
                }],
                self_closing: false,
            },
            Token::Text("hello".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Eof,
        ]
    );
}

#[test]
fn string_attribute() {
    let tokens = scan("[box title=\"hello world\"]content[/box]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "box".into(),
                attributes: vec![Attribute {
                    name: "title".into(),
                    value: AttributeValue::String("hello world".into()),
                }],
                self_closing: false,
            },
            Token::Text("content".into()),
            Token::CloseTag { name: "box".into() },
            Token::Eof,
        ]
    );
}

#[test]
fn multiple_attributes() {
    let tokens = scan("[text fg=red bold bg=black]hi[/text]");
    let open = &tokens[0];
    match open {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(attributes.len(), 3);
            assert_eq!(attributes[0].name, "fg");
            assert_eq!(attributes[0].value, AttributeValue::Ident("red".into()));
            assert_eq!(attributes[1].name, "bold");
            assert_eq!(attributes[1].value, AttributeValue::Flag);
            assert_eq!(attributes[2].name, "bg");
            assert_eq!(attributes[2].value, AttributeValue::Ident("black".into()));
        }
        _ => panic!("expected OpenTag"),
    }
}

#[test]
fn hex_color_attribute() {
    let tokens = scan("[text fg=#ff6600]hi[/text]");
    match &tokens[0] {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(attributes[0].value, AttributeValue::Ident("#ff6600".into()));
        }
        _ => panic!("expected OpenTag"),
    }
}

#[test]
fn attribute_with_escaped_quote() {
    let tokens = scan(r#"[box title="say \"hello\""]x[/box]"#);
    match &tokens[0] {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(
                attributes[0].value,
                AttributeValue::String("say \"hello\"".into())
            );
        }
        _ => panic!("expected OpenTag"),
    }
}

#[test]
fn attribute_with_backslash_escapes() {
    let tokens = scan(r#"[text title="line1\nline2\ttab\\slash"]x[/text]"#);
    match &tokens[0] {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(
                attributes[0].value,
                AttributeValue::String("line1\nline2\ttab\\slash".into())
            );
        }
        _ => panic!("expected OpenTag"),
    }
}

#[test]
fn self_closing_with_attributes() {
    let tokens = scan("[hr style=double fg=yellow /]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "hr".into(),
                attributes: vec![
                    Attribute {
                        name: "style".into(),
                        value: AttributeValue::Ident("double".into()),
                    },
                    Attribute {
                        name: "fg".into(),
                        value: AttributeValue::Ident("yellow".into()),
                    },
                ],
                self_closing: true,
            },
            Token::Eof,
        ]
    );
}

// ─── Escape Sequences (Bracket Escaping) ─────────────────────

#[test]
fn escaped_open_bracket() {
    let tokens = scan("use [[tags]] in AML");
    assert_eq!(
        tokens,
        vec![Token::Text("use [tags] in AML".into()), Token::Eof]
    );
}

#[test]
fn escaped_brackets_in_text() {
    let tokens = scan("[text][[bold]][/text]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::Text("[bold]".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Eof,
        ]
    );
}

// ─── Nested Tags ─────────────────────────────────────────────

#[test]
fn nested_tags() {
    let tokens = scan("[box][text]inside[/text][/box]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "box".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::Text("inside".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::CloseTag { name: "box".into() },
            Token::Eof,
        ]
    );
}

#[test]
fn deeply_nested() {
    let tokens = scan("[a][b][c]deep[/c][/b][/a]");
    assert_eq!(tokens.len(), 8); // 3 opens + text + 3 closes + eof
}

// ─── Whitespace Handling ─────────────────────────────────────

#[test]
fn whitespace_in_content() {
    let tokens = scan("[text]  hello  [/text]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::Text("  hello  ".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Eof,
        ]
    );
}

#[test]
fn whitespace_between_attributes() {
    let tokens = scan("[text  fg=red   bold  ]hi[/text]");
    match &tokens[0] {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(attributes.len(), 2);
        }
        _ => panic!("expected OpenTag"),
    }
}

#[test]
fn newlines_in_content() {
    let tokens = scan("[pre]\nline1\nline2\n[/pre]");
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "\nline1\nline2\n"),
        _ => panic!("expected Text"),
    }
}

// ─── Terminal Injection Prevention ───────────────────────────

#[test]
fn strips_ansi_color_from_text() {
    let tokens = scan("[text]\x1b[31mred\x1b[0m[/text]");
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "red"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn strips_osc_title_injection() {
    let tokens = scan("[text]\x1b]0;EVIL TITLE\x07safe text[/text]");
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "safe text"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn strips_8bit_csi() {
    let tokens = scan("[text]\u{9b}31minjected[/text]");
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "injected"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn strips_null_bytes() {
    let tokens = scan("[text]a\x00b\x00c[/text]");
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "abc"),
        _ => panic!("expected Text"),
    }
}

// ─── Malformed Input ─────────────────────────────────────────

#[test]
fn unterminated_open_tag() {
    let err = scan_err("[text fg=red");
    assert!(matches!(err, ScanError::UnterminatedTag { .. }));
}

#[test]
fn unterminated_close_tag() {
    let err = scan_err("[/text");
    assert!(matches!(err, ScanError::UnterminatedTag { .. }));
}

#[test]
fn unterminated_string() {
    let err = scan_err("[text title=\"unclosed]content[/text]");
    // The scanner should hit unterminated string since ] inside quotes
    // is just a character, and we never find the closing quote before
    // running into content that confuses us. Actually let's check what happens.
    assert!(matches!(
        err,
        ScanError::UnterminatedString { .. } | ScanError::UnterminatedTag { .. }
    ));
}

#[test]
fn empty_tag_name() {
    let err = scan_err("[ ]");
    assert!(matches!(err, ScanError::InvalidTagName { .. }));
}

#[test]
fn stray_close_bracket() {
    // A `]` outside any tag is just text
    let tokens = scan("hello ] world");
    assert_eq!(
        tokens,
        vec![Token::Text("hello ] world".into()), Token::Eof]
    );
}

// ─── Edge Cases ──────────────────────────────────────────────

#[test]
fn empty_document() {
    let tokens = scan("");
    assert_eq!(tokens, vec![Token::Eof]);
}

#[test]
fn tag_with_no_content() {
    let tokens = scan("[text][/text]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Eof,
        ]
    );
}

#[test]
fn only_whitespace_content() {
    let tokens = scan("[text]   [/text]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::Text("   ".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Eof,
        ]
    );
}

#[test]
fn tag_name_case_insensitive() {
    let tokens = scan("[TEXT]hello[/Text]");
    match &tokens[0] {
        Token::OpenTag { name, .. } => assert_eq!(name, "text"),
        _ => panic!("expected OpenTag"),
    }
    match &tokens[2] {
        Token::CloseTag { name } => assert_eq!(name, "text"),
        _ => panic!("expected CloseTag"),
    }
}

#[test]
fn tag_name_with_hyphens() {
    let tokens = scan("[text-animate effect=typewriter]hi[/text-animate]");
    match &tokens[0] {
        Token::OpenTag { name, .. } => assert_eq!(name, "text-animate"),
        _ => panic!("expected OpenTag"),
    }
}

#[test]
fn attribute_name_case_insensitive() {
    let tokens = scan("[text FG=Red BOLD]hi[/text]");
    match &tokens[0] {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(attributes[0].name, "fg");
            // Values preserve original case — parser handles normalization
            assert_eq!(attributes[0].value, AttributeValue::Ident("Red".into()));
            assert_eq!(attributes[1].name, "bold");
        }
        _ => panic!("expected OpenTag"),
    }
}

// ─── Unicode ─────────────────────────────────────────────────

#[test]
fn unicode_text_content() {
    let tokens = scan("[text]こんにちは 🌍[/text]");
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "こんにちは 🌍"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn unicode_in_attribute() {
    let tokens = scan("[box title=\"日本語タイトル\"]x[/box]");
    match &tokens[0] {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(
                attributes[0].value,
                AttributeValue::String("日本語タイトル".into())
            );
        }
        _ => panic!("expected OpenTag"),
    }
}

#[test]
fn emoji_in_content() {
    let tokens = scan("[text]🔥 fire 🔥[/text]");
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "🔥 fire 🔥"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn box_drawing_characters() {
    let input = "[pre]╔═══╗\n║ A ║\n╚═══╝[/pre]";
    let tokens = scan(input);
    match &tokens[1] {
        Token::Text(s) => assert_eq!(s, "╔═══╗\n║ A ║\n╚═══╝"),
        _ => panic!("expected Text"),
    }
}

// ─── Size Limits ─────────────────────────────────────────────

#[test]
fn rejects_oversized_input() {
    let huge = vec![b'a'; MAX_INPUT_SIZE + 1];
    let err = Scanner::new(&huge).unwrap_err();
    assert!(matches!(err, ScanError::InputTooLarge { .. }));
}

#[test]
#[cfg_attr(miri, ignore = "16 MiB boundary loop is covered by native tests")]
fn accepts_max_size_input() {
    let big = vec![b'a'; MAX_INPUT_SIZE];
    assert!(Scanner::new(&big).is_ok());
}

#[test]
#[cfg_attr(miri, ignore = "large token payload is covered by native tests")]
fn rejects_oversized_text_token() {
    let input = "x".repeat(MAX_TEXT_TOKEN_SIZE + 1);
    let mut scanner = Scanner::new(input.as_bytes()).unwrap();
    assert!(matches!(
        scanner.scan_all(),
        Err(ScanError::TextTooLong { .. })
    ));
}

#[test]
fn rejects_oversized_attribute_value() {
    let value = "x".repeat(MAX_ATTRIBUTE_VALUE_CHARS + 1);
    let input = format!("[page title=\"{value}\"][/page]");
    let mut scanner = Scanner::new(input.as_bytes()).unwrap();
    assert!(matches!(
        scanner.scan_all(),
        Err(ScanError::AttributeValueTooLong { .. })
    ));
}

#[test]
fn rejects_invalid_utf8() {
    let invalid = vec![0xFF, 0xFE, 0x00];
    let err = Scanner::new(&invalid).unwrap_err();
    assert!(matches!(err, ScanError::InvalidUtf8));
}

// ─── Token Count Limit ──────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore = "high token-count loop is covered by native tests")]
fn rejects_too_many_tokens() {
    // Each [x][/x] pair generates 2 tokens minimum, plus text between them
    // We need > 50,000 tokens. Simplest: lots of adjacent tags.
    let mut input = String::new();
    for _ in 0..25_001 {
        input.push_str("[x][/x]");
    }
    let mut scanner = Scanner::new(input.as_bytes()).unwrap();
    let result = scanner.scan_all();
    assert!(matches!(result, Err(ScanError::TooManyTokens { .. })));
}

// ─── Complex Documents ───────────────────────────────────────

#[test]
fn realistic_page() {
    let input = r#"[page mode=document title="My Page"]
  [meta author="alice" /]
  [header]
    [text bold fg=cyan]Welcome[/text]
  [/header]
  [body]
    [box border=double fg=green w=40 title="Status"]
      [text]All systems operational[/text]
    [/box]
    [hr style=dash /]
    [link href="atp://other.site/page" transition="dissolve"]
      [text fg=yellow]Visit Other Site[/text]
    [/link]
  [/body]
[/page]"#;

    let tokens = scan(input);

    // Verify key tokens are present
    let open_names: Vec<&str> = tokens
        .iter()
        .filter_map(|t| match t {
            Token::OpenTag { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    assert!(open_names.contains(&"page"));
    assert!(open_names.contains(&"meta"));
    assert!(open_names.contains(&"header"));
    assert!(open_names.contains(&"text"));
    assert!(open_names.contains(&"body"));
    assert!(open_names.contains(&"box"));
    assert!(open_names.contains(&"hr"));
    assert!(open_names.contains(&"link"));

    // Verify page attributes
    match &tokens[0] {
        Token::OpenTag { attributes, .. } => {
            assert_eq!(attributes.len(), 2);
            assert_eq!(attributes[0].name, "mode");
            assert_eq!(
                attributes[0].value,
                AttributeValue::Ident("document".into())
            );
            assert_eq!(attributes[1].name, "title");
            assert_eq!(
                attributes[1].value,
                AttributeValue::String("My Page".into())
            );
        }
        _ => panic!("expected OpenTag"),
    }

    // Last token should be Eof
    assert_eq!(tokens.last(), Some(&Token::Eof));
}

#[test]
fn mixed_content_and_tags() {
    let tokens = scan("before[text]middle[/text]after");
    assert_eq!(
        tokens,
        vec![
            Token::Text("before".into()),
            Token::OpenTag {
                name: "text".into(),
                attributes: vec![],
                self_closing: false,
            },
            Token::Text("middle".into()),
            Token::CloseTag {
                name: "text".into(),
            },
            Token::Text("after".into()),
            Token::Eof,
        ]
    );
}

#[test]
fn multiple_self_closing_in_sequence() {
    let tokens = scan("[hr /][br /][spacer lines=2 /]");
    assert_eq!(
        tokens,
        vec![
            Token::OpenTag {
                name: "hr".into(),
                attributes: vec![],
                self_closing: true,
            },
            Token::OpenTag {
                name: "br".into(),
                attributes: vec![],
                self_closing: true,
            },
            Token::OpenTag {
                name: "spacer".into(),
                attributes: vec![Attribute {
                    name: "lines".into(),
                    value: AttributeValue::Ident("2".into()),
                }],
                self_closing: true,
            },
            Token::Eof,
        ]
    );
}

// ─── Display Trait ───────────────────────────────────────────

#[test]
fn token_display() {
    let tag = Token::OpenTag {
        name: "text".into(),
        attributes: vec![
            Attribute {
                name: "fg".into(),
                value: AttributeValue::Ident("red".into()),
            },
            Attribute {
                name: "bold".into(),
                value: AttributeValue::Flag,
            },
        ],
        self_closing: false,
    };
    assert_eq!(format!("{tag}"), "[text fg=red bold]");

    let self_close = Token::OpenTag {
        name: "hr".into(),
        attributes: vec![],
        self_closing: true,
    };
    assert_eq!(format!("{self_close}"), "[hr /]");

    let close = Token::CloseTag {
        name: "text".into(),
    };
    assert_eq!(format!("{close}"), "[/text]");
}
