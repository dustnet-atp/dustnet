/// Sanitize input by stripping dangerous control characters.
///
/// This is the primary defense against terminal escape sequence injection.
/// A malicious server could embed ANSI escape sequences in text content
/// to manipulate the user's terminal (retitle window, access clipboard,
/// exploit terminal emulator vulnerabilities, etc.).
///
/// We operate on a **whitelist** basis: only explicitly allowed characters
/// pass through. Everything else is stripped.
///
/// Allowed characters:
/// - `\t` (0x09) — tab
/// - `\n` (0x0A) — newline
/// - `\r` (0x0D) — carriage return (stripped to normalize line endings)
/// - All printable Unicode (U+0020 and above, excluding surrogates and
///   certain control ranges)
///
/// Explicitly stripped:
/// - `\x1B` (ESC) — begins ANSI escape sequences
/// - `\x00`–`\x08` — C0 controls (NUL, SOH, STX, etc.)
/// - `\x0B`–`\x0C` — VT, FF
/// - `\x0E`–`\x1A` — SO, SI, DLE, etc.
/// - `\x1C`–`\x1F` — FS, GS, RS, US
/// - `\x7F` — DEL
/// - `\u{9B}` — CSI (8-bit equivalent of ESC [)
/// - `\u{90}` — DCS (Device Control String)
/// - `\u{9D}` — OSC (Operating System Command)
/// - `\u{9E}` — PM (Privacy Message)
/// - `\u{9F}` — APC (Application Program Command)
/// - Bidirectional controls and invisible formatting characters — see
///   [`is_deceptive_format`]. These are printable-range characters, so the
///   whitelist above would otherwise admit them.
///
/// When an ESC character is found, we also skip any following characters
/// that are part of the escape sequence, so partial sequences don't
/// leak through as garbage.
pub fn sanitize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    // `String`'s `fmt::Write` never fails; the `Result` exists for the generic
    // destination. Discarded rather than unwrapped so this path cannot panic
    // even if that ever stops holding — a truncated sanitisation is a better
    // outcome than an abort on remote input.
    let _ = sanitize_into(input, &mut output);
    output
}

/// Write sanitized text without requiring an intermediate allocation.
///
/// The caller controls the destination's allocation policy. In particular,
/// bounded remote consumers can reject `fmt::Write` growth while preserving
/// exactly the same escape-sequence filtering as [`sanitize`].
pub fn sanitize_into(input: &str, output: &mut impl std::fmt::Write) -> std::fmt::Result {
    let mut i = 0usize;

    while let Some((ch, next)) = next_char(input, i) {
        match ch {
            // Allowed control characters
            '\t' | '\n' => {
                output.write_char(ch)?;
                i = next;
            }
            // Carriage return — normalize to just \n
            '\r' => {
                // \r\n → \n (skip the \r, the \n will be added next iteration)
                // \r alone → \n
                if next_char(input, next).is_some_and(|(next_ch, _)| next_ch == '\n') {
                    i = next; // skip \r, let \n be handled next
                } else {
                    output.write_char('\n')?;
                    i = next;
                }
            }
            // ESC — skip the entire escape sequence
            '\x1B' => {
                i = skip_escape_sequence(input, next);
            }
            // 8-bit C1 control characters (when encoded as single codepoints)
            '\u{80}'..='\u{9F}' => {
                // These include CSI (9B), OSC (9D), DCS (90), etc.
                // Skip them. Some (CSI, OSC, DCS) introduce sequences
                // similar to ESC-based ones.
                i = next;
                if matches!(ch, '\u{90}' | '\u{9B}' | '\u{9D}' | '\u{9E}' | '\u{9F}') {
                    i = skip_c1_sequence(input, i, ch);
                }
            }
            // DEL
            '\x7F' => {
                i = next;
            }
            // Other C0 control characters
            '\x00'..='\x08' | '\x0B'..='\x0C' | '\x0E'..='\x1A' | '\x1C'..='\x1F' => {
                i = next;
            }
            // Printable-range characters that reorder or hide their neighbours
            ch if is_deceptive_format(ch) => {
                i = next;
            }
            // All printable Unicode — allow
            _ => {
                output.write_char(ch)?;
                i = next;
            }
        }
    }

    Ok(())
}

/// Characters that are invisible, or that reorder the text around them.
///
/// These are not terminal controls. They sit in the printable range the
/// whitelist admits, and a conforming terminal renders them *correctly* — the
/// deception happens in the reader's eye rather than in the emulator's parser,
/// which is why the ESC and C1 machinery below does not reach them.
///
/// A client that draws server-authored text beside clickable link targets
/// cannot admit them. U+202E RIGHT-TO-LEFT OVERRIDE makes a label read as one
/// URI while the link points at another; the zero-width and invisible-operator
/// characters let two labels that differ in bytes render identically, so a
/// user comparing what is on screen learns nothing.
///
/// Two families are deliberately **kept**, because stripping them would
/// corrupt legitimate text rather than protect anyone:
///
/// - U+200C ZERO WIDTH NON-JOINER and U+200D ZERO WIDTH JOINER carry meaning.
///   ZWJ composes emoji sequences, and both drive Arabic and Indic shaping.
/// - The U+E0000 tag block, which emoji flag sequences are built from.
///
/// Removal rather than rejection keeps this arm consistent with every other:
/// a hostile document renders with the deception gone, not as an error that
/// denies the user the rest of the page.
fn is_deceptive_format(ch: char) -> bool {
    matches!(
        ch,
        // Bidirectional marks, including the Arabic letter mark.
        '\u{061C}' | '\u{200E}' | '\u{200F}'
        // Explicit bidi embedding and override: LRE, RLE, PDF, LRO, RLO.
        | '\u{202A}'..='\u{202E}'
        // Bidi isolates: LRI, RLI, FSI, PDI.
        | '\u{2066}'..='\u{2069}'
        // Zero-width space, then WORD JOINER and the invisible math operators.
        | '\u{200B}' | '\u{2060}'..='\u{2064}'
        // Interlinear annotation, which hides the text it brackets.
        | '\u{FFF9}'..='\u{FFFB}'
        // Zero-width no-break space, also seen as a byte-order mark.
        | '\u{FEFF}'
    )
}

/// Skip past an ESC-initiated escape sequence.
///
/// Handles the common ANSI sequence types:
/// - `ESC [` (CSI) — terminated by a byte in 0x40–0x7E
/// - `ESC ]` (OSC) — terminated by BEL (0x07) or ST (ESC \)
/// - `ESC P` (DCS) — terminated by ST (ESC \)
/// - `ESC ^` (PM), `ESC _` (APC) — terminated by ST (ESC \)
/// - `ESC` + single character — two-byte sequences (e.g., ESC 7, ESC 8)
fn skip_escape_sequence(input: &str, start: usize) -> usize {
    let mut i = start;

    let Some((introducer, next)) = next_char(input, i) else {
        return input.len();
    };

    match introducer {
        // CSI sequence: ESC [ ... <final byte 0x40-0x7E>
        '[' => {
            i = next;
            while let Some((ch, after)) = next_char(input, i) {
                i = after;
                // Final byte of CSI sequence
                if ('\x40'..='\x7E').contains(&ch) {
                    break;
                }
            }
        }
        // OSC sequence: ESC ] ... (BEL | ESC \)
        ']' => {
            i = skip_until_st_or_bel(input, next);
        }
        // DCS: ESC P ... ST
        'P' => {
            i = skip_until_st_or_bel(input, next);
        }
        // PM: ESC ^ ... ST
        '^' => {
            i = skip_until_st_or_bel(input, next);
        }
        // APC: ESC _ ... ST
        '_' => {
            i = skip_until_st_or_bel(input, next);
        }
        // Any other single character after ESC — skip it
        _ => {
            i = next;
        }
    }

    i
}

/// Skip past a C1-initiated sequence (8-bit equivalents).
fn skip_c1_sequence(input: &str, start: usize, introducer: char) -> usize {
    let mut i = start;

    match introducer {
        // CSI (0x9B) — same termination as ESC [
        '\u{9B}' => {
            while let Some((ch, after)) = next_char(input, i) {
                i = after;
                if ('\x40'..='\x7E').contains(&ch) {
                    break;
                }
            }
        }
        // OSC (0x9D), DCS (0x90), PM (0x9E), APC (0x9F) — terminated by ST
        '\u{90}' | '\u{9D}' | '\u{9E}' | '\u{9F}' => {
            i = skip_until_st_or_bel(input, i);
        }
        _ => {}
    }

    i
}

/// Skip until we find ST (ESC \, or U+009C) or BEL (0x07).
fn skip_until_st_or_bel(input: &str, start: usize) -> usize {
    let mut i = start;

    while let Some((ch, next)) = next_char(input, i) {
        if ch == '\x07' {
            // BEL terminates
            i = next;
            break;
        }
        if ch == '\u{9C}' {
            // ST (8-bit) terminates
            i = next;
            break;
        }
        if ch == '\x1B' && next_char(input, next).is_some_and(|(next_ch, _)| next_ch == '\\') {
            // ST (7-bit: ESC \) terminates
            i = next_char(input, next).map_or(next, |(_, after)| after);
            break;
        }
        i = next;
    }

    i
}

fn next_char(input: &str, index: usize) -> Option<(char, usize)> {
    let ch = input.get(index..)?.chars().next()?;
    Some((ch, index + ch.len_utf8()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(sanitize("hello world"), "hello world");
    }

    #[test]
    fn tabs_and_newlines_preserved() {
        assert_eq!(sanitize("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn carriage_return_normalized() {
        assert_eq!(sanitize("a\r\nb"), "a\nb");
        assert_eq!(sanitize("a\rb"), "a\nb");
    }

    #[test]
    fn strips_esc_csi_sgr() {
        // ESC[31m = set red foreground
        assert_eq!(sanitize("hello\x1b[31mred\x1b[0mworld"), "helloredworld");
    }

    #[test]
    fn strips_esc_osc_title() {
        // ESC]0;title BEL = set window title
        assert_eq!(sanitize("before\x1b]0;HACKED\x07after"), "beforeafter");
    }

    #[test]
    fn strips_esc_osc_with_st() {
        // ESC]0;title ESC\ = set window title (ST terminator)
        assert_eq!(sanitize("before\x1b]0;HACKED\x1b\\after"), "beforeafter");
    }

    #[test]
    fn strips_csi_8bit() {
        // 0x9B is 8-bit CSI
        assert_eq!(sanitize("hello\u{9b}31mworld"), "helloworld");
    }

    #[test]
    fn strips_osc_8bit() {
        // 0x9D is 8-bit OSC
        assert_eq!(sanitize("hello\u{9d}0;HACKED\x07world"), "helloworld");
    }

    #[test]
    fn strips_dcs() {
        // ESC P ... ESC \
        assert_eq!(sanitize("before\x1bPsome data\x1b\\after"), "beforeafter");
    }

    #[test]
    fn strips_null_and_c0() {
        assert_eq!(sanitize("a\x00b\x01c\x02d"), "abcd");
        assert_eq!(sanitize("a\x0Eb\x1Ac"), "abc");
    }

    #[test]
    fn strips_del() {
        assert_eq!(sanitize("a\x7Fb"), "ab");
    }

    #[test]
    fn strips_remaining_c1_controls() {
        // 0x80-0x8F and 0x91-0x9A and 0x9C range
        assert_eq!(sanitize("a\u{80}b\u{8F}c"), "abc");
    }

    #[test]
    fn unicode_passes_through() {
        assert_eq!(sanitize("こんにちは 🌍 café"), "こんにちは 🌍 café");
    }

    #[test]
    fn box_drawing_passes_through() {
        assert_eq!(sanitize("╔═══╗\n║   ║\n╚═══╝"), "╔═══╗\n║   ║\n╚═══╝");
    }

    #[test]
    fn embedded_esc_in_normal_text() {
        // ESC followed by a regular character (2-byte sequence)
        assert_eq!(sanitize("abc\x1b7def\x1b8ghi"), "abcdefghi");
    }

    #[test]
    fn truncated_escape_at_end() {
        // ESC at very end of input
        assert_eq!(sanitize("hello\x1b"), "hello");
        // ESC [ at end (no final byte)
        assert_eq!(sanitize("hello\x1b["), "hello");
        // ESC ] at end (no terminator)
        assert_eq!(sanitize("hello\x1b]"), "hello");
    }

    #[test]
    fn complex_csi_parameters() {
        // CSI with lots of params: ESC[38;2;255;100;0m (truecolor)
        assert_eq!(sanitize("a\x1b[38;2;255;100;0mb"), "ab");
    }

    #[test]
    fn empty_input() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn only_control_chars() {
        assert_eq!(sanitize("\x00\x01\x1b[31m\x7f"), "");
    }

    /// U+202E RIGHT-TO-LEFT OVERRIDE is the Trojan Source primitive: it makes
    /// the bytes after it render in reverse, so a link label can display a
    /// different target than the one it carries.
    #[test]
    fn strips_bidi_override_from_text() {
        assert_eq!(
            sanitize("atp://\u{202e}moc.live\u{202c}/"),
            "atp://moc.live/"
        );
        // Every explicit embedding and override, and both isolates directions.
        assert_eq!(
            sanitize("a\u{202a}b\u{202b}c\u{202d}d\u{2066}e\u{2067}f\u{2068}g\u{2069}h"),
            "abcdefgh"
        );
        // Bidi marks, including the Arabic letter mark.
        assert_eq!(sanitize("a\u{200e}b\u{200f}c\u{061c}d"), "abcd");
    }

    /// Invisible characters let two labels that differ in bytes render
    /// identically, so what the user compares on screen carries no information.
    #[test]
    fn strips_zero_width_formatting() {
        assert_eq!(sanitize("pay\u{200b}pal.com"), "paypal.com");
        // WORD JOINER and the invisible math operators.
        assert_eq!(
            sanitize("a\u{2060}b\u{2061}c\u{2062}d\u{2063}e\u{2064}f"),
            "abcdef"
        );
        // Byte-order mark anywhere in the document, not just at the start.
        assert_eq!(sanitize("a\u{feff}b"), "ab");
        // Interlinear annotation hides the text it brackets.
        assert_eq!(
            sanitize("a\u{fff9}hidden\u{fffa}gloss\u{fffb}b"),
            "ahiddenglossb"
        );
    }

    /// ZWJ and ZWNJ are load-bearing: ZWJ composes emoji sequences and both
    /// drive Arabic and Indic shaping. Stripping them corrupts ordinary text,
    /// so they are deliberately outside `is_deceptive_format`.
    #[test]
    fn preserves_joiners_that_carry_meaning() {
        // Woman + ZWJ + laptop renders as a single "woman technologist" glyph.
        assert_eq!(
            sanitize("\u{1f469}\u{200d}\u{1f4bb}"),
            "\u{1f469}\u{200d}\u{1f4bb}"
        );
        assert_eq!(sanitize("a\u{200c}b"), "a\u{200c}b");
    }

    /// The deceptive-format arm sits alongside the control-character arms
    /// rather than replacing them: a document carrying both is fully cleaned.
    #[test]
    fn strips_escapes_and_deceptive_formats_together() {
        assert_eq!(
            sanitize("\x1b[31m\u{202e}danger\u{202c}\u{200b}\x1b[0m"),
            "danger"
        );
    }

    #[test]
    fn sanitize_into_propagates_destination_rejection() {
        struct Reject;

        impl std::fmt::Write for Reject {
            fn write_str(&mut self, _text: &str) -> std::fmt::Result {
                Err(std::fmt::Error)
            }
        }

        assert!(sanitize_into("remote", &mut Reject).is_err());
    }
}
