use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{LayoutAllocationSite, reject_layout_allocation};
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

/// Configuration for character width calculation.
///
/// Controls how Unicode East Asian Width "Ambiguous" characters
/// (box drawing, block elements, certain symbols) are measured.
/// Different terminals render these as either 1 or 2 columns wide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidthConfig {
    /// Width to use for ambiguous characters: 1 or 2.
    pub ambiguous_width: u8,
}

impl Default for WidthConfig {
    fn default() -> Self {
        WidthConfig { ambiguous_width: 1 }
    }
}

/// A single line of wrapped text with its display width.
#[derive(Debug, Clone, PartialEq)]
pub struct WrappedLine {
    /// The text content of this line.
    pub text: String,
    /// The display width in terminal columns.
    pub width: usize,
}

/// Wrapped lines together with the lease that admitted them.
///
/// The lease is released when the lines are dropped, so the governor sees the
/// wrap's cost for exactly as long as the caller holds the result. This is the
/// plain-text counterpart of `engine::InlineLines`.
pub(crate) struct WrappedLines {
    values: Vec<WrappedLine>,
    _lease: Option<BudgetLease>,
}

impl std::ops::Deref for WrappedLines {
    type Target = [WrappedLine];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

/// Upper bounds on what wrapping a given text can allocate, computed before
/// any of it is allocated.
struct WrapRequirements {
    line_bound: usize,
    payload_bound: usize,
}

/// An empty line. Kept as one function so the single `String::new()` that
/// stands for "no content" is not repeated at five call sites; it is never
/// grown, so it never allocates.
fn blank_line() -> WrappedLine {
    WrappedLine {
        text: String::new(),
        width: 0,
    }
}

/// Measure a wrap without performing it.
///
/// Mirrors `engine::inline_wrap_requirements`: every count is checked, so
/// remote content that would overflow a bound refuses the wrap rather than
/// wrapping around into a small admission. `payload_bound` doubles the word
/// bytes and adds one byte per word, covering both the separating spaces and
/// the slack `try_reserve`'s amortised growth may leave in a line.
fn wrap_requirements(text: &str, max_width: usize, wcfg: WidthConfig) -> Option<WrapRequirements> {
    let mut word_count = 0usize;
    let mut word_payload = 0usize;
    let mut forced_chunks = 0usize;
    let mut blank_lines = 0usize;
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            blank_lines = blank_lines.checked_add(1)?;
            continue;
        }
        for word in paragraph.split_whitespace() {
            let width = display_width(word, wcfg);
            if width == 0 {
                continue;
            }
            word_count = word_count.checked_add(1)?;
            word_payload = word_payload.checked_add(word.len())?;
            if width > max_width {
                forced_chunks =
                    forced_chunks.checked_add(forced_chunk_count(word, max_width, wcfg)?)?;
            }
        }
    }
    let line_bound = word_count
        .checked_add(forced_chunks)?
        .checked_add(blank_lines)?
        .max(1);
    let payload_bound = word_payload.checked_mul(2)?.checked_add(word_count)?;
    Some(WrapRequirements {
        line_bound,
        payload_bound,
    })
}

/// How many lines force-breaking one over-wide word produces.
fn forced_chunk_count(word: &str, max_width: usize, wcfg: WidthConfig) -> Option<usize> {
    let mut chunks = 0usize;
    let mut chunk_width = 0usize;
    let mut chunk_has_content = false;
    for grapheme in word.graphemes(true) {
        let grapheme_width = display_width(grapheme, wcfg);
        if grapheme_width == 0 {
            chunk_has_content = true;
            continue;
        }
        if chunk_width.checked_add(grapheme_width)? > max_width && chunk_has_content {
            chunks = chunks.checked_add(1)?;
            chunk_width = 0;
        }
        chunk_width = chunk_width.checked_add(grapheme_width)?;
        chunk_has_content = true;
    }
    if chunk_has_content {
        chunks = chunks.checked_add(1)?;
    }
    Some(chunks)
}

/// Wrap text to fit within `max_width` terminal columns, admitting the result
/// against `governor` before any of it is built.
///
/// Handles:
/// - Word wrapping at whitespace boundaries
/// - Force-breaking words longer than max_width
/// - Wide characters (CJK, emoji) that occupy 2 columns
/// - Grapheme clusters that must not be split
/// - Preserving explicit newlines
/// - Collapsing multiple spaces within a line to single space
///
/// Returns `None` when the governor refuses the admission or the allocator
/// refuses a reservation. Nothing is exposed on refusal: the partially built
/// lines are dropped with the lease, so the governor returns to its prior
/// usage.
///
/// An empty input produces one empty line.
pub(crate) fn try_wrap_text(
    text: &str,
    max_width: usize,
    wcfg: WidthConfig,
    governor: Option<&ResourceGovernor>,
) -> Option<WrappedLines> {
    if reject_layout_allocation(LayoutAllocationSite::WrappedText) {
        return None;
    }
    let mut values: Vec<WrappedLine> = Vec::new();

    if max_width == 0 {
        values.try_reserve_exact(1).ok()?;
        values.push(blank_line());
        return Some(WrappedLines {
            values,
            _lease: None,
        });
    }

    let requirements = wrap_requirements(text, max_width, wcfg)?;
    let requested = requirements
        .line_bound
        .checked_mul(std::mem::size_of::<WrappedLine>())?
        .checked_add(requirements.payload_bound)?;
    let mut lease = match (requested, governor) {
        (0, _) | (_, None) => None,
        (bytes, Some(governor)) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, bytes)
                .ok()?,
        ),
    };
    values.try_reserve_exact(requirements.line_bound).ok()?;

    // Split on explicit newlines first
    for (index, paragraph) in text.split('\n').enumerate() {
        if index > 0 && paragraph.is_empty() {
            try_push_line(&mut values, blank_line())?;
            continue;
        }

        try_wrap_paragraph(paragraph, max_width, &mut values, wcfg)?;
    }

    if values.is_empty() {
        try_push_line(&mut values, blank_line())?;
    }

    // Reconcile the estimate against what was actually retained. The bound
    // above is what admits the wrap before it is built; it cannot be exact,
    // because every short line rounds up to the allocator's minimum string
    // allocation and `try_reserve`'s amortised growth leaves slack. Resizing
    // leaves the governor holding the real cost rather than the estimate, and
    // a refused growth returns before the lines are exposed.
    if let Some(lease) = lease.as_mut() {
        let actual = retained_bytes(values.capacity(), &values);
        lease.try_resize_with_cost(actual, actual).ok()?;
    }

    Some(WrappedLines {
        values,
        _lease: lease,
    })
}

/// The heap actually held by wrapped lines: the line vector's capacity plus
/// each line's string capacity.
fn retained_bytes(capacity: usize, values: &[WrappedLine]) -> usize {
    capacity
        .saturating_mul(std::mem::size_of::<WrappedLine>())
        .saturating_add(values.iter().map(|line| line.text.capacity()).sum())
}

/// Wrap a single paragraph (no embedded newlines) to max_width.
fn try_wrap_paragraph(
    text: &str,
    max_width: usize,
    result: &mut Vec<WrappedLine>,
    wcfg: WidthConfig,
) -> Option<()> {
    if text.is_empty() {
        return try_push_line(result, blank_line());
    }

    let mut current_line = String::new();
    let mut current_width: usize = 0;

    for word in text.split_whitespace() {
        let word_width = display_width(word, wcfg);

        if word_width == 0 {
            continue;
        }

        // If this single word is wider than max_width, force-break it
        if word_width > max_width {
            if !current_line.is_empty() {
                try_push_line(
                    result,
                    WrappedLine {
                        text: std::mem::take(&mut current_line),
                        width: current_width,
                    },
                )?;
                current_width = 0;
            }

            try_force_break_word(word, max_width, result, wcfg)?;
            continue;
        }

        if current_line.is_empty() {
            current_line = try_owned(word)?;
            current_width = word_width;
        } else {
            let needed = current_width + 1 + word_width;
            if needed <= max_width {
                // One reservation covers the separator and the word.
                current_line.try_reserve(word.len().checked_add(1)?).ok()?;
                current_line.push(' ');
                current_line.push_str(word);
                current_width = needed;
            } else {
                try_push_line(
                    result,
                    WrappedLine {
                        text: std::mem::take(&mut current_line),
                        width: current_width,
                    },
                )?;
                current_line = try_owned(word)?;
                current_width = word_width;
            }
        }
    }

    if !current_line.is_empty() {
        try_push_line(
            result,
            WrappedLine {
                text: current_line,
                width: current_width,
            },
        )?;
    }
    Some(())
}

/// Force-break a word that is wider than max_width into multiple lines.
fn try_force_break_word(
    word: &str,
    max_width: usize,
    result: &mut Vec<WrappedLine>,
    wcfg: WidthConfig,
) -> Option<()> {
    let mut current_line = String::new();
    let mut current_width: usize = 0;

    for grapheme in word.graphemes(true) {
        let gw = display_width(grapheme, wcfg);

        if gw == 0 {
            current_line.try_reserve(grapheme.len()).ok()?;
            current_line.push_str(grapheme);
            continue;
        }

        if current_width + gw > max_width {
            if !current_line.is_empty() {
                try_push_line(
                    result,
                    WrappedLine {
                        text: std::mem::take(&mut current_line),
                        width: current_width,
                    },
                )?;
            }
            current_width = 0;
        }

        current_line.try_reserve(grapheme.len()).ok()?;
        current_line.push_str(grapheme);
        current_width += gw;
    }

    if !current_line.is_empty() {
        try_push_line(
            result,
            WrappedLine {
                text: current_line,
                width: current_width,
            },
        )?;
    }
    Some(())
}

/// Append one line, admitting the slot before the push.
fn try_push_line(lines: &mut Vec<WrappedLine>, line: WrappedLine) -> Option<()> {
    lines.try_reserve(1).ok()?;
    lines.push(line);
    Some(())
}

/// Copy a string with its capacity reserved exactly, refusing rather than
/// aborting when the allocator declines.
fn try_owned(value: &str) -> Option<String> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).ok()?;
    copy.push_str(value);
    Some(copy)
}

/// Calculate the display width of a string in terminal columns.
///
/// Uses `unicode-width` for base widths, then adjusts ambiguous-width
/// characters according to the config.
pub fn display_width(s: &str, wcfg: WidthConfig) -> usize {
    if wcfg.ambiguous_width == 1 {
        // Fast path: unicode-width already treats ambiguous as 1
        UnicodeWidthStr::width(s)
    } else {
        // Slow path: calculate per-character, adjusting ambiguous chars
        s.chars().map(|ch| char_width(ch, wcfg)).sum()
    }
}

/// Calculate the display width of a single character.
pub fn char_width(ch: char, wcfg: WidthConfig) -> usize {
    let base = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);

    if wcfg.ambiguous_width == 2 && base == 1 && is_ambiguous_width(ch) {
        2
    } else {
        base
    }
}

/// Check if a character has East Asian Width "Ambiguous".
///
/// These are characters that Unicode marks as having ambiguous width —
/// some terminals render them as 1 column, others as 2. The main
/// categories that matter for terminal art:
/// - Box drawing (U+2500–U+257F)
/// - Block elements (U+2580–U+259F)
/// - Geometric shapes (U+25A0–U+25FF)
/// - Miscellaneous symbols (U+2600–U+26FF)
/// - Dingbats (U+2700–U+27BF)
/// - Braille patterns (U+2800–U+28FF)
/// - Arrows (U+2190–U+21FF)
/// - Mathematical operators (U+2200–U+22FF)
fn is_ambiguous_width(ch: char) -> bool {
    // This covers the most common ambiguous characters seen in terminal art.
    // Full Unicode East Asian Width data has many more ranges, but these
    // are the ones that matter for AML content.
    matches!(ch as u32,
        // Arrows
        0x2190..=0x21FF |
        // Mathematical operators
        0x2200..=0x22FF |
        // Miscellaneous technical
        0x2300..=0x23FF |
        // Box drawing
        0x2500..=0x257F |
        // Block elements
        0x2580..=0x259F |
        // Geometric shapes
        0x25A0..=0x25FF |
        // Miscellaneous symbols
        0x2600..=0x26FF |
        // Dingbats
        0x2700..=0x27BF |
        // Braille patterns
        0x2800..=0x28FF |
        // CJK symbols and punctuation (some)
        0x3000..=0x303F |
        // Greek capital letters (some are ambiguous)
        0x0391..=0x03C9 |
        // Cyrillic (some)
        0x0400..=0x04FF |
        // Latin extended with diacritics used in CJK contexts
        0x00A0..=0x00FF |
        // General punctuation
        0x2010..=0x2027 |
        0x2030..=0x2043 |
        // Letterlike symbols
        0x2100..=0x214F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapping tests below assert wrapping semantics, not admission, so
    /// they drive the ungoverned path and unwrap the refusal case.
    fn wrap_text(text: &str, max_width: usize, wcfg: WidthConfig) -> Vec<WrappedLine> {
        try_wrap_text(text, max_width, wcfg, None)
            .expect("ungoverned wrap must not be refused")
            .values
    }

    const W1: WidthConfig = WidthConfig { ambiguous_width: 1 };
    const W2: WidthConfig = WidthConfig { ambiguous_width: 2 };

    #[test]
    fn governed_wrap_admits_at_least_what_it_retains() {
        let governor = ResourceGovernor::new();
        let text = "the quick brown fox jumps over the lazy dog\nand a second paragraph too";
        let lines = try_wrap_text(text, 12, W1, Some(&governor)).expect("wrap refused");

        let retained = retained_bytes(lines.values.capacity(), &lines.values);
        let admitted = lines._lease.as_ref().expect("governed wrap has a lease");
        assert_eq!(
            admitted.amount(),
            retained,
            "the lease must hold the exact retained cost after reconciliation"
        );
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            admitted.amount()
        );

        drop(lines);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn wrap_rejection_preserves_existing_usage_and_exposes_nothing() {
        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY - 1,
            )
            .unwrap();
        let before = governor.used(ResourceCategory::RemoteCollections);

        assert!(try_wrap_text("some remote text to wrap", 8, W1, Some(&governor)).is_none());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);

        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn force_broken_words_are_admitted_before_they_are_built() {
        // A single word wider than the line exercises the force-break path,
        // whose chunk count the requirements pass has to predict without
        // performing the break.
        let governor = ResourceGovernor::new();
        let lines = try_wrap_text("abcdefghijklmnopqrstuvwxyz", 4, W1, Some(&governor))
            .expect("wrap refused");
        assert_eq!(lines.len(), 7);
        // The estimate under-admits this case by a few bytes: seven short
        // lines each round up to the allocator's minimum string allocation.
        // Reconciliation is what makes the held amount exact, and this test
        // is the one that found it.
        let retained = retained_bytes(lines.values.capacity(), &lines.values);
        assert_eq!(lines._lease.as_ref().unwrap().amount(), retained);
    }

    #[test]
    fn ungoverned_wrap_takes_no_budget() {
        let governor = ResourceGovernor::new();
        let lines = try_wrap_text("hello world", 5, W1, None).expect("wrap refused");
        assert!(lines._lease.is_none());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    fn widths(lines: &[WrappedLine]) -> Vec<usize> {
        lines.iter().map(|l| l.width).collect()
    }

    fn texts(lines: &[WrappedLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    // ─── Basic Wrapping ──────────────────────────────────────

    #[test]
    fn no_wrap_needed() {
        let lines = wrap_text("hello", 20, W1);
        assert_eq!(texts(&lines), vec!["hello"]);
        assert_eq!(widths(&lines), vec![5]);
    }

    #[test]
    fn single_word_exact_width() {
        let lines = wrap_text("hello", 5, W1);
        assert_eq!(texts(&lines), vec!["hello"]);
    }

    #[test]
    fn two_words_that_fit() {
        let lines = wrap_text("hello world", 20, W1);
        assert_eq!(texts(&lines), vec!["hello world"]);
    }

    #[test]
    fn two_words_that_wrap() {
        let lines = wrap_text("hello world", 8, W1);
        assert_eq!(texts(&lines), vec!["hello", "world"]);
    }

    #[test]
    fn multiple_words_wrapping() {
        let lines = wrap_text("the quick brown fox jumps", 12, W1);
        assert_eq!(texts(&lines), vec!["the quick", "brown fox", "jumps"]);
    }

    #[test]
    fn wrap_at_exact_boundary() {
        let lines = wrap_text("aa bb", 5, W1);
        assert_eq!(texts(&lines), vec!["aa bb"]);
    }

    #[test]
    fn wrap_one_char_over() {
        let lines = wrap_text("aa bbb", 5, W1);
        assert_eq!(texts(&lines), vec!["aa", "bbb"]);
    }

    // ─── Force Breaking ──────────────────────────────────────

    #[test]
    fn force_break_long_word() {
        let lines = wrap_text("abcdefghij", 4, W1);
        assert_eq!(texts(&lines), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn force_break_exact_multiple() {
        let lines = wrap_text("abcdef", 3, W1);
        assert_eq!(texts(&lines), vec!["abc", "def"]);
    }

    #[test]
    fn mixed_long_and_short_words() {
        let lines = wrap_text("hi abcdefghij bye", 5, W1);
        assert_eq!(texts(&lines), vec!["hi", "abcde", "fghij", "bye"]);
    }

    // ─── Whitespace Handling ─────────────────────────────────

    #[test]
    fn multiple_spaces_collapsed() {
        let lines = wrap_text("hello    world", 20, W1);
        assert_eq!(texts(&lines), vec!["hello world"]);
    }

    #[test]
    fn leading_trailing_whitespace() {
        let lines = wrap_text("  hello  ", 20, W1);
        assert_eq!(texts(&lines), vec!["hello"]);
    }

    #[test]
    fn only_whitespace() {
        let lines = wrap_text("   ", 20, W1);
        assert_eq!(texts(&lines), vec![""]);
    }

    // ─── Newlines ────────────────────────────────────────────

    #[test]
    fn explicit_newlines() {
        let lines = wrap_text("line1\nline2\nline3", 20, W1);
        assert_eq!(texts(&lines), vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn newlines_with_wrapping() {
        let lines = wrap_text("hello world\nfoo bar baz", 8, W1);
        assert_eq!(texts(&lines), vec!["hello", "world", "foo bar", "baz"]);
    }

    #[test]
    fn consecutive_newlines() {
        let lines = wrap_text("a\n\nb", 20, W1);
        assert_eq!(texts(&lines), vec!["a", "", "b"]);
    }

    #[test]
    fn trailing_newline() {
        let lines = wrap_text("hello\n", 20, W1);
        assert_eq!(texts(&lines), vec!["hello", ""]);
    }

    // ─── Empty Input ─────────────────────────────────────────

    #[test]
    fn empty_string() {
        let lines = wrap_text("", 20, W1);
        assert_eq!(texts(&lines), vec![""]);
    }

    #[test]
    fn zero_width() {
        let lines = wrap_text("hello", 0, W1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width, 0);
    }

    // ─── Unicode Width ───────────────────────────────────────

    #[test]
    fn cjk_characters_are_double_width() {
        assert_eq!(display_width("日", W1), 2);
        assert_eq!(display_width("日本語", W1), 6);
    }

    #[test]
    fn cjk_wrapping() {
        let lines = wrap_text("日本語", 4, W1);
        assert_eq!(texts(&lines), vec!["日本", "語"]);
        assert_eq!(widths(&lines), vec![4, 2]);
    }

    #[test]
    fn cjk_mixed_with_ascii() {
        let lines = wrap_text("hi 日本", 6, W1);
        assert_eq!(texts(&lines), vec!["hi", "日本"]);
    }

    #[test]
    fn cjk_no_split_mid_char() {
        let lines = wrap_text("日本語テスト", 5, W1);
        assert_eq!(texts(&lines), vec!["日本", "語テ", "スト"]);
        assert_eq!(widths(&lines), vec![4, 4, 4]);
    }

    #[test]
    fn emoji_width() {
        assert_eq!(display_width("🔥", W1), 2);
        assert_eq!(display_width("🌍", W1), 2);
    }

    #[test]
    fn emoji_wrapping() {
        let lines = wrap_text("🔥🌍🎉", 4, W1);
        assert_eq!(lines.len(), 2);
        assert_eq!(widths(&lines), vec![4, 2]);
    }

    #[test]
    fn ascii_is_single_width() {
        assert_eq!(display_width("hello", W1), 5);
        assert_eq!(display_width("a", W1), 1);
    }

    #[test]
    fn box_drawing_width_w1() {
        assert_eq!(display_width("┌", W1), 1);
        assert_eq!(display_width("─", W1), 1);
        assert_eq!(display_width("│", W1), 1);
        assert_eq!(display_width("╔═╗", W1), 3);
    }

    #[test]
    fn block_elements_width_w1() {
        assert_eq!(display_width("█", W1), 1);
        assert_eq!(display_width("▓", W1), 1);
        assert_eq!(display_width("░", W1), 1);
    }

    // ─── Ambiguous Width = 2 ─────────────────────────────────

    #[test]
    fn box_drawing_width_w2() {
        assert_eq!(char_width('┌', W2), 2);
        assert_eq!(char_width('─', W2), 2);
        assert_eq!(char_width('│', W2), 2);
        assert_eq!(display_width("╔═╗", W2), 6);
    }

    #[test]
    fn block_elements_width_w2() {
        assert_eq!(char_width('█', W2), 2);
        assert_eq!(char_width('▓', W2), 2);
        assert_eq!(char_width('░', W2), 2);
    }

    #[test]
    fn ascii_unaffected_by_w2() {
        assert_eq!(display_width("hello", W2), 5);
        assert_eq!(char_width('a', W2), 1);
    }

    #[test]
    fn cjk_unaffected_by_w2() {
        // CJK is already 2, should stay 2 not become something else
        assert_eq!(char_width('日', W2), 2);
    }

    #[test]
    fn wrapping_with_ambiguous_w2() {
        // "▒▒▒▒" with W2 = 8 columns. max_width=6 → should break.
        let lines = wrap_text("▒▒▒▒", 6, W2);
        assert_eq!(lines.len(), 2);
        assert_eq!(widths(&lines), vec![6, 2]);
    }

    #[test]
    fn mixed_ambiguous_and_ascii_w2() {
        // "a▒b" with W2 = 1+2+1 = 4 columns
        assert_eq!(display_width("a▒b", W2), 4);
    }

    // ─── is_ambiguous_width ──────────────────────────────────

    #[test]
    fn ambiguous_chars_detected() {
        assert!(is_ambiguous_width('─')); // box drawing
        assert!(is_ambiguous_width('█')); // block element
        assert!(is_ambiguous_width('▒')); // block element
        assert!(is_ambiguous_width('→')); // arrow
        assert!(is_ambiguous_width('■')); // geometric shape
    }

    #[test]
    fn non_ambiguous_chars_not_detected() {
        assert!(!is_ambiguous_width('a'));
        assert!(!is_ambiguous_width('Z'));
        assert!(!is_ambiguous_width('1'));
        assert!(!is_ambiguous_width(' '));
    }

    // ─── char_width ──────────────────────────────────────────

    #[test]
    fn char_width_values() {
        assert_eq!(char_width('a', W1), 1);
        assert_eq!(char_width(' ', W1), 1);
        assert_eq!(char_width('日', W1), 2);
        assert_eq!(char_width('─', W1), 1);
    }

    // ─── Edge Cases ──────────────────────────────────────────

    #[test]
    fn single_char_width_1() {
        let lines = wrap_text("a", 1, W1);
        assert_eq!(texts(&lines), vec!["a"]);
    }

    #[test]
    fn single_wide_char_width_1() {
        let lines = wrap_text("日", 1, W1);
        assert_eq!(lines.len(), 1);
        assert_eq!(texts(&lines), vec!["日"]);
    }

    #[test]
    fn many_single_chars() {
        let lines = wrap_text("a b c d e", 3, W1);
        assert_eq!(texts(&lines), vec!["a b", "c d", "e"]);
    }

    #[test]
    fn word_exactly_max_width() {
        let lines = wrap_text("abcde fghij", 5, W1);
        assert_eq!(texts(&lines), vec!["abcde", "fghij"]);
    }

    // ─── Combining Characters ────────────────────────────────

    #[test]
    fn combining_marks_zero_width() {
        let e_acute = "e\u{0301}";
        assert_eq!(display_width(e_acute, W1), 1);
    }

    #[test]
    fn text_with_combining_marks() {
        let text = "cafe\u{0301}";
        assert_eq!(display_width(text, W1), 4);
    }

    // ─── Realistic Content ───────────────────────────────────

    #[test]
    fn typical_paragraph() {
        let text = "Dustnet is a terminal-native network of sites with rich ANSI rendering and animations.";
        let lines = wrap_text(text, 40, W1);
        for line in &lines {
            assert!(
                line.width <= 40,
                "line too wide: {} ({})",
                line.text,
                line.width
            );
        }
        assert!(lines.len() >= 2);
    }

    #[test]
    fn mixed_content() {
        let text = "Welcome to 日本語テスト! Enjoy 🔥 your stay.";
        let lines = wrap_text(text, 20, W1);
        for line in &lines {
            assert!(
                line.width <= 20,
                "line too wide: '{}' width={}",
                line.text,
                line.width
            );
        }
    }
}
