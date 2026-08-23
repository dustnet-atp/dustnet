pub mod ast;
pub mod components;
#[cfg(test)]
mod tests;
pub mod validate;

use crate::color::parse_color;
use crate::scanner::{AttributeValue, Token};
use ast::*;
use std::fmt::{self, Write as _};

/// Maximum nesting depth for elements.
const MAX_DEPTH: usize = 32;

/// Maximum total element count.
const MAX_ELEMENTS: usize = 10_000;

/// Maximum number of `[animate]` regions in one document. Region buffers are
/// independently constrained by the aggregate scene-cell budget, and authored
/// frames by `MAX_ANIMATION_FRAMES`; this ceiling bounds per-tick traversal
/// without imposing a small-page design limit. WASM guests have a lower,
/// separate memory-backed ceiling.
pub const MAX_ANIMATE_REGIONS: usize = 1_024;

/// Sixteen 4 MiB guests enforce the 64 MiB aggregate WASM memory envelope.
pub const MAX_WASM_INSTANCES: usize = 16;

/// Maximum cumulative number of authored animation frames in one document.
pub const MAX_ANIMATION_FRAMES: usize = 256;

/// Maximum number of `[on]` bindings in one document.
pub const MAX_ON_BINDINGS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserAllocationSite {
    String,
    Collection,
    TokenCopy,
    Diagnostic,
    ComponentMap,
    SlotMap,
    Substitution,
    ValidationMap,
}

#[cfg(test)]
thread_local! {
    static REJECT_ALLOCATION: std::cell::Cell<Option<ParserAllocationSite>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
fn reject_allocation(site: ParserAllocationSite) -> bool {
    REJECT_ALLOCATION.with(|rejected| rejected.get() == Some(site))
}

#[cfg(not(test))]
fn reject_allocation(_site: ParserAllocationSite) -> bool {
    false
}

/// A diagnostic message (error or warning) from the parser.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiagnosticLevel {
    Error,
    Warning,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.level {
            DiagnosticLevel::Error => "error",
            DiagnosticLevel::Warning => "warning",
        };
        write!(f, "{level}[{}]: {}", self.code, self.message)
    }
}

/// Result of parsing: a document (if possible) plus diagnostics.
pub struct ParseResult {
    pub document: Option<Document>,
    pub diagnostics: Vec<Diagnostic>,
    resource_exhausted: bool,
}

impl ParseResult {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.level == DiagnosticLevel::Error)
    }

    /// Whether parser-owned candidate allocation failed before publication.
    pub fn resource_exhausted(&self) -> bool {
        self.resource_exhausted
    }
}

fn mark_allocation_failure(failed: &mut bool) {
    *failed = true;
}

fn try_copy_string(value: &str, failed: &mut bool) -> String {
    let mut owned = String::new();
    if reject_allocation(ParserAllocationSite::String)
        || owned.try_reserve_exact(value.len()).is_err()
    {
        mark_allocation_failure(failed);
        return owned;
    }
    owned.push_str(value);
    owned
}

/// Whether two runs would render identically, ignoring their text.
///
/// Used to merge adjacent runs. Without it every text token becomes its own run,
/// and an unstyled block -- which is nearly every block -- turns into as many
/// runs as the scanner happened to emit tokens.
fn same_style(a: &PreRun, b: &PreRun) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
        && a.dim == b.dim
        && a.blink == b.blink
}

fn trim_owned_string(mut value: String) -> String {
    // `start` must be measured against the already-truncated value: trailing
    // and leading whitespace overlap when the content is entirely whitespace,
    // and draining a range measured on the longer original would be out of
    // bounds. Both steps stay in place, so neither reallocates.
    value.truncate(value.trim_end().len());
    let start = value.len() - value.trim_start().len();
    if start != 0 {
        value.drain(..start);
    }
    value
}

fn try_push<T>(values: &mut Vec<T>, value: T, failed: &mut bool) {
    if reject_allocation(ParserAllocationSite::Collection) || values.try_reserve(1).is_err() {
        mark_allocation_failure(failed);
        return;
    }
    values.push(value);
}

fn try_clone_attribute(
    value: &crate::scanner::Attribute,
    failed: &mut bool,
) -> crate::scanner::Attribute {
    crate::scanner::Attribute {
        name: try_copy_string(&value.name, failed),
        value: match &value.value {
            AttributeValue::String(value) => AttributeValue::String(try_copy_string(value, failed)),
            AttributeValue::Ident(value) => AttributeValue::Ident(try_copy_string(value, failed)),
            AttributeValue::Flag => AttributeValue::Flag,
        },
    }
}

fn try_clone_token(value: &Token, failed: &mut bool) -> Token {
    if reject_allocation(ParserAllocationSite::TokenCopy) {
        mark_allocation_failure(failed);
        return Token::Eof;
    }
    match value {
        Token::OpenTag {
            name,
            attributes,
            self_closing,
        } => {
            let mut cloned_attributes = Vec::new();
            if cloned_attributes
                .try_reserve_exact(attributes.len())
                .is_err()
            {
                mark_allocation_failure(failed);
            } else {
                for attribute in attributes {
                    cloned_attributes.push(try_clone_attribute(attribute, failed));
                }
            }
            Token::OpenTag {
                name: try_copy_string(name, failed),
                attributes: cloned_attributes,
                self_closing: *self_closing,
            }
        }
        Token::CloseTag { name } => Token::CloseTag {
            name: try_copy_string(name, failed),
        },
        Token::Text(text) => Token::Text(try_copy_string(text, failed)),
        Token::Eof => Token::Eof,
    }
}

struct FallibleString<'a> {
    value: String,
    failed: &'a mut bool,
}

fn try_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    level: DiagnosticLevel,
    code: &'static str,
    args: fmt::Arguments<'_>,
    failed: &mut bool,
) {
    if reject_allocation(ParserAllocationSite::Diagnostic) {
        mark_allocation_failure(failed);
        return;
    }
    let mut writer = FallibleString {
        value: String::new(),
        failed,
    };
    if writer.write_fmt(args).is_err() {
        return;
    }
    try_push(
        diagnostics,
        Diagnostic {
            level,
            code,
            message: writer.value,
        },
        failed,
    );
}

impl fmt::Write for FallibleString<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.value.try_reserve(value.len()).is_err() {
            mark_allocation_failure(self.failed);
            return Err(fmt::Error);
        }
        self.value.push_str(value);
        Ok(())
    }
}

/// Parse a token stream into an AML document.
///
/// If the token stream contains `[def]` blocks, components are expanded
/// before parsing. Diagnostics from expansion are merged into the result.
pub fn parse(tokens: Vec<Token>) -> ParseResult {
    // Phase 1: expand components
    let (expanded_tokens, mut component_diags, component_allocation_failed) =
        components::expand_components(tokens);

    // Phase 2: parse the expanded token stream
    let mut parser = Parser::new(expanded_tokens);
    let mut result = parser.parse_document();
    result.resource_exhausted |= component_allocation_failed;
    if result.resource_exhausted {
        result.document = None;
    }

    // Merge component diagnostics (prepend so they appear first)
    if component_diags
        .try_reserve(result.diagnostics.len())
        .is_err()
    {
        result.resource_exhausted = true;
        result.document = None;
    } else {
        component_diags.append(&mut result.diagnostics);
        result.diagnostics = component_diags;
    }

    // Phase 3: validate trigger references
    if let Some(ref doc) = result.document {
        let (trigger_diags, validation_allocation_failed) = validate::validate_triggers(doc);
        if validation_allocation_failed {
            result.resource_exhausted = true;
            result.document = None;
        }
        if result.diagnostics.try_reserve(trigger_diags.len()).is_err() {
            result.resource_exhausted = true;
            result.document = None;
        } else {
            result.diagnostics.extend(trigger_diags);
        }
    }

    result
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    element_count: usize,
    depth: usize,
    allocation_failed: bool,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
            element_count: 0,
            depth: 0,
            allocation_failed: false,
        }
    }

    fn copy_string(&mut self, value: &str) -> String {
        try_copy_string(value, &mut self.allocation_failed)
    }

    fn parse_trigger_ref(&mut self, value: &str) -> Option<TriggerRef> {
        let (panel_id, state_name) = value.split_once(':')?;
        let panel_id = panel_id.trim();
        let state_name = state_name.trim();
        if panel_id.is_empty() || state_name.is_empty() {
            return None;
        }
        Some(TriggerRef {
            panel_id: self.copy_string(panel_id),
            state_name: self.copy_string(state_name),
        })
    }

    fn current(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn diagnostic(&mut self, level: DiagnosticLevel, code: &'static str, args: fmt::Arguments<'_>) {
        try_diagnostic(
            &mut self.diagnostics,
            level,
            code,
            args,
            &mut self.allocation_failed,
        );
    }

    fn error(&mut self, code: &'static str, message: &str) {
        self.diagnostic(DiagnosticLevel::Error, code, format_args!("{message}"));
    }

    fn error_fmt(&mut self, code: &'static str, args: fmt::Arguments<'_>) {
        self.diagnostic(DiagnosticLevel::Error, code, args);
    }

    fn warning(&mut self, code: &'static str, message: &str) {
        self.diagnostic(DiagnosticLevel::Warning, code, format_args!("{message}"));
    }

    /// Parse a transition name, warning when it is not one this build knows.
    ///
    /// `parse_transition_kind` returns None for an unrecognised name, and None
    /// means cut -- so a typo, or a name from a newer build than the client
    /// reading the page, renders as the box simply appearing. Nothing reported
    /// it: `transition="banana"` validated clean and popped. The failure looked
    /// like a bug in the panel rather than a name the client had never heard of.
    ///
    /// A warning rather than an error, because a page authored against a newer
    /// build should still render on an older client -- just visibly plainly,
    /// with a reason available.
    fn transition_attr(&mut self, element: &str, value: &str) -> Option<TransitionKind> {
        let kind = parse_transition_kind(value);
        if kind.is_none() && !value.trim().is_empty() {
            self.warning_fmt(
                "W008",
                format_args!(
                    "unknown transition \"{value}\" on [{element}]; \
                     this build will cut instead of animating"
                ),
            );
        }
        kind
    }

    fn warning_fmt(&mut self, code: &'static str, args: fmt::Arguments<'_>) {
        self.diagnostic(DiagnosticLevel::Warning, code, args);
    }

    fn current_owned(&mut self) -> Token {
        let mut failed = false;
        let token = try_clone_token(self.current(), &mut failed);
        self.allocation_failed |= failed;
        token
    }

    fn check_element_limit(&mut self) -> bool {
        self.element_count += 1;
        if self.element_count > MAX_ELEMENTS {
            self.error_fmt(
                "E010",
                format_args!("maximum element count exceeded ({MAX_ELEMENTS})"),
            );
            return false;
        }
        true
    }

    fn check_depth_limit(&mut self) -> bool {
        if self.depth > MAX_DEPTH {
            self.error_fmt(
                "E009",
                format_args!("maximum nesting depth exceeded ({MAX_DEPTH})"),
            );
            return false;
        }
        true
    }

    // ─── Document parsing ────────────────────────────────────

    fn parse_document(&mut self) -> ParseResult {
        // Skip leading text (whitespace)
        self.skip_text();

        // Expect [page] as root
        let page = match self.current_owned() {
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } if name == "page" => {
                self.advance();
                if self_closing {
                    self.error("E001", "page element cannot be self-closing");
                    None
                } else {
                    Some(self.parse_page(&attributes))
                }
            }
            Token::Eof => {
                self.error("E001", "empty document — expected [page] as root element");
                None
            }
            _ => {
                self.error("E001", "document must have [page] as root element");
                None
            }
        };

        let document = page.map(|p| Document { page: p });

        // The grammar is `document = ws page ws`: the root element *is* the
        // document. Anything after `[/page]` other than whitespace is a second
        // document smuggled into the first, and until this check existed it was
        // silently discarded — so a server and a client could disagree about
        // what a page contained without either reporting a problem.
        //
        // The document is still published alongside the diagnostic. Every
        // consumer gates on `has_errors()` before reading it, and `dustnet
        // check` can show the structure it did parse.
        if document.is_some() {
            self.skip_text();
            if !matches!(self.current_owned(), Token::Eof) {
                self.error(
                    "E001",
                    "content after [/page] — a document has exactly one [page] root",
                );
            }
        }

        ParseResult {
            document: (!self.allocation_failed).then_some(document).flatten(),
            diagnostics: std::mem::take(&mut self.diagnostics),
            resource_exhausted: self.allocation_failed,
        }
    }

    fn parse_page(&mut self, attrs: &[crate::scanner::Attribute]) -> Page {
        let mut mode = PageMode::Document;
        let mut title = None;
        let mut cols: Option<u16> = None;
        let mut rows: Option<u16> = None;
        let mut transition = None;
        let mut transition_duration_ms = 300u32;

        for attr in attrs {
            match attr.name.as_str() {
                "mode" => {
                    let val = attr_str_value(&attr.value);
                    if val.eq_ignore_ascii_case("document") {
                        mode = PageMode::Document;
                    } else if !val.eq_ignore_ascii_case("screen") {
                        self.warning_fmt(
                            "W002",
                            format_args!("unknown page mode: {val}, defaulting to document"),
                        );
                    }
                }
                "title" => title = Some(self.copy_string(attr_str_value(&attr.value))),
                "cols" => {
                    cols = attr_str_value(&attr.value)
                        .parse::<u16>()
                        .ok()
                        .filter(|value| *value <= MAX_LAYOUT_COLUMNS);
                    if cols.is_none() {
                        self.warning_fmt(
                            "W002",
                            format_args!("screen columns must be at most {MAX_LAYOUT_COLUMNS}"),
                        );
                    }
                }
                "rows" => {
                    rows = attr_str_value(&attr.value)
                        .parse::<u16>()
                        .ok()
                        .filter(|value| *value <= MAX_LAYOUT_ROWS);
                    if rows.is_none() {
                        self.warning_fmt(
                            "W002",
                            format_args!("screen rows must be at most {MAX_LAYOUT_ROWS}"),
                        );
                    }
                }
                "transition" => {
                    transition = self.transition_attr("page", attr_str_value(&attr.value));
                }
                "duration" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        transition_duration_ms = d;
                    }
                }
                "scroll" | "paginated" => {} // valid but handled elsewhere
                other => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{other}\" on [page]"),
                    );
                }
            }
        }

        // Check if screen mode
        let screen_mode = attrs
            .iter()
            .find(|a| a.name == "mode")
            .is_some_and(|a| attr_str_value(&a.value).eq_ignore_ascii_case("screen"));
        if screen_mode {
            mode = PageMode::Screen { cols, rows };
        }

        let children = self.parse_children("page");

        Page {
            mode,
            title,
            meta: Vec::new(),
            style: None,
            transition,
            transition_duration_ms,
            children,
        }
    }

    // ─── Children parsing ────────────────────────────────────

    /// Parse children until we hit a matching close tag or EOF.
    fn parse_children(&mut self, parent_tag: &str) -> Vec<Element> {
        let mut children = Vec::new();

        loop {
            match self.current_owned() {
                Token::Eof => {
                    if parent_tag != "page" {
                        self.warning_fmt(
                            "W003",
                            format_args!("missing closing tag [/{parent_tag}]"),
                        );
                    }
                    break;
                }
                Token::CloseTag { name } => {
                    if name == parent_tag {
                        self.advance();
                        break;
                    } else {
                        // Mismatched close tag — warn and skip
                        self.warning_fmt(
                            "W004",
                            format_args!("unexpected [/{name}], expected [/{parent_tag}]"),
                        );
                        self.advance();
                    }
                }
                Token::Text(text) => {
                    self.advance();
                    // For inline-capable parents (text, heading, link, item),
                    // preserve whitespace-only text as it serves as a word
                    // separator between styled spans. For other parents,
                    // discard whitespace-only text.
                    let inline_parent = matches!(parent_tag, "text" | "heading" | "link" | "item");
                    let keep = if inline_parent {
                        !text.is_empty()
                    } else {
                        !text.trim().is_empty()
                    };
                    if keep {
                        try_push(
                            &mut children,
                            Element::Text(TextElement {
                                content: text,
                                fg: None,
                                bg: None,
                                bold: false,
                                italic: false,
                                underline: false,
                                strikethrough: false,
                                dim: false,
                                blink: false,
                                align: Alignment::default(),
                                children: Vec::new(),
                            }),
                            &mut self.allocation_failed,
                        );
                    }
                }
                Token::OpenTag {
                    name,
                    attributes,
                    self_closing,
                } => {
                    self.advance();

                    if !self.check_element_limit() {
                        break;
                    }

                    if let Some(elem) =
                        self.parse_element(&name, &attributes, self_closing, parent_tag)
                    {
                        try_push(&mut children, elem, &mut self.allocation_failed);
                    }
                }
            }
        }

        children
    }

    // ─── Element parsing ─────────────────────────────────────

    fn parse_element(
        &mut self,
        name: &str,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
        parent_tag: &str,
    ) -> Option<Element> {
        self.depth += 1;
        if !self.check_depth_limit() {
            self.depth -= 1;
            // Skip children if not self-closing
            if !self_closing {
                self.skip_until_close(name);
            }
            return None;
        }

        let elem = match name {
            // Layout
            "box" => Some(self.parse_box(attrs, self_closing)),
            "row" => Some(self.parse_row(attrs, self_closing)),
            "col" => {
                if parent_tag != "row" {
                    self.error("E002", "[col] must be inside [row]");
                }
                Some(self.parse_col(attrs, self_closing))
            }
            "hr" | "divider" => Some(self.parse_hr(attrs)),
            "spacer" => Some(self.parse_spacer(attrs)),
            "header" | "body" | "footer" => Some(self.parse_container(name, attrs, self_closing)),
            "nav" => Some(self.parse_nav(attrs, self_closing)),

            // Text
            "text" => Some(self.parse_text(attrs, self_closing)),
            "pre" => Some(self.parse_pre(attrs, self_closing)),
            "heading" => Some(self.parse_heading(attrs, self_closing)),
            "list" => Some(self.parse_list(attrs, self_closing)),
            "item" => {
                if parent_tag != "list" {
                    self.error("E003", "[item] must be inside [list]");
                }
                Some(self.parse_item(attrs, self_closing))
            }

            // Interactive
            "link" => Some(self.parse_link(attrs, self_closing)),
            "input" => Some(self.parse_input(attrs)),
            "select" => Some(self.parse_select(attrs, self_closing)),
            "option" => {
                if parent_tag != "select" {
                    self.error("E004", "[option] must be inside [select]");
                }
                Some(self.parse_option(attrs, self_closing))
            }
            "button" => Some(self.parse_button(attrs, self_closing)),
            "form" => Some(self.parse_form(attrs, self_closing)),

            // Media
            "art" => Some(self.parse_art(attrs, self_closing)),
            "table" => Some(self.parse_table(attrs, self_closing)),
            "thead" | "tbody" => Some(self.parse_container(name, attrs, self_closing)),
            "tr" => {
                if !matches!(parent_tag, "thead" | "tbody" | "table") {
                    self.error("E005", "[tr] must be inside table structure");
                }
                Some(self.parse_tr(attrs, self_closing))
            }
            "td" | "th" => {
                if parent_tag != "tr" {
                    self.error_fmt("E006", format_args!("[{name}] must be inside [tr]"));
                }
                Some(self.parse_cell(name, attrs, self_closing))
            }

            // Animation
            "animate" => Some(self.parse_animate(attrs, self_closing)),
            "frame" => {
                if parent_tag != "animate" {
                    self.error("E007", "[frame] must be inside [animate]");
                }
                Some(self.parse_frame(attrs, self_closing))
            }
            "element" => Some(self.parse_element_def(attrs, self_closing)),
            "tween" => Some(self.parse_tween(attrs, self_closing)),
            "text-animate" => Some(self.parse_text_animate(attrs, self_closing)),
            "at" => {
                if parent_tag != "tween" {
                    self.error("E008", "[at] must be inside [tween]");
                }
                // at elements are handled inside parse_tween
                if !self_closing {
                    self.skip_until_close("at");
                }
                None
            }

            // Live
            "live" => Some(self.parse_live(attrs, self_closing)),

            // Server-resolved placeholder
            "include" => Some(self.parse_include(attrs)),

            // Panels
            "panel" => Some(self.parse_panel(attrs, self_closing)),
            "state" => {
                if parent_tag != "panel" {
                    self.error("E025", "[state] must be inside [panel]");
                }
                Some(self.parse_state(attrs, self_closing))
            }

            // Collapsible details
            "details" => Some(self.parse_details(attrs, self_closing)),

            // Meta (special handling)
            "meta" => {
                self.parse_meta(attrs);
                None
            }
            "style" => {
                self.parse_style_element(attrs);
                None
            }

            // Event bindings
            "on" => {
                if !self_closing {
                    self.warning("W005", "[on] should be self-closing");
                    self.skip_until_close("on");
                }
                Some(self.parse_on(attrs))
            }

            // Pagination
            "pagination" => Some(self.parse_container("pagination", attrs, self_closing)),

            // Unknown
            _ => {
                self.warning_fmt("W001", format_args!("unknown element [{name}], ignored"));
                if !self_closing {
                    // Treat as transparent container
                    let children = self.parse_children(name);
                    self.depth -= 1;
                    // Pass through children
                    return if children.is_empty() {
                        None
                    } else if children.len() == 1 {
                        // `next()` already yields the `Option` this arm wants.
                        children.into_iter().next()
                    } else {
                        Some(Element::Body(ContainerElement {
                            sticky: None,
                            children,
                        }))
                    };
                }
                self.depth -= 1;
                return None;
            }
        };

        self.depth -= 1;
        elem
    }

    // ─── Layout element parsers ──────────────────────────────

    fn parse_box(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut elem = BoxElement {
            x: None,
            y: None,
            w: Dimension::Fill,
            h: Dimension::Fit,
            border: BorderStyle::Single,
            fg: None,
            bg: None,
            padding: 1,
            title: None,
            join_top: None,
            join_bottom: None,
            join_left: None,
            join_right: None,
            align: Alignment::Left,
            sticky: None,
            children: Vec::new(),
        };

        for attr in attrs {
            match attr.name.as_str() {
                "x" => elem.x = parse_u16_attr(&attr.value),
                "y" => elem.y = parse_u16_attr(&attr.value),
                "w" => {
                    if let Ok(d) = parse_dimension(attr_str_value(&attr.value)) {
                        elem.w = d;
                    } else {
                        self.warning("W002", "invalid width value on [box]");
                    }
                }
                "h" => {
                    if let Ok(d) = parse_dimension(attr_str_value(&attr.value)) {
                        elem.h = d;
                    } else {
                        self.warning("W002", "invalid height value on [box]");
                    }
                }
                "border" => {
                    if let Ok(b) = parse_border_style(attr_str_value(&attr.value)) {
                        elem.border = b;
                    } else {
                        self.warning("W002", "invalid border style on [box]");
                    }
                }
                "fg" => elem.fg = self.parse_color_attr(&attr.value, "box"),
                "bg" => elem.bg = self.parse_color_attr(&attr.value, "box"),
                "padding" => {
                    elem.padding = parse_u16_attr(&attr.value).unwrap_or(1);
                }
                "title" => elem.title = Some(self.copy_string(attr_str_value(&attr.value))),
                "join-top" => elem.join_top = parse_u16_attr(&attr.value),
                "join-bottom" => elem.join_bottom = parse_u16_attr(&attr.value),
                "join-left" => elem.join_left = parse_u16_attr(&attr.value),
                "join-right" => elem.join_right = parse_u16_attr(&attr.value),
                "align" => {
                    if let Ok(a) = parse_alignment(attr_str_value(&attr.value)) {
                        elem.align = a;
                    }
                }
                "sticky" => {
                    if let Ok(s) = parse_sticky(attr_str_value(&attr.value)) {
                        elem.sticky = Some(s);
                    }
                }
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [box]", attr.name),
                    );
                }
            }
        }

        if !self_closing {
            elem.children = self.parse_children("box");
        }

        Element::Box(elem)
    }

    fn parse_row(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut gap = 1u16;
        let mut align = VerticalAlignment::Top;

        for attr in attrs {
            match attr.name.as_str() {
                "gap" => gap = parse_u16_attr(&attr.value).unwrap_or(1),
                "align" => {
                    if let Ok(a) = parse_vertical_alignment(attr_str_value(&attr.value)) {
                        align = a;
                    }
                }
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [row]", attr.name),
                    );
                }
            }
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("row")
        };

        Element::Row(RowElement {
            gap,
            align,
            children,
        })
    }

    fn parse_col(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut w = Dimension::Fill;
        let mut align = Alignment::Left;

        for attr in attrs {
            match attr.name.as_str() {
                "w" => {
                    if let Ok(d) = parse_dimension(attr_str_value(&attr.value)) {
                        w = d;
                    }
                }
                "align" => {
                    if let Ok(a) = parse_alignment(attr_str_value(&attr.value)) {
                        align = a;
                    }
                }
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [col]", attr.name),
                    );
                }
            }
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("col")
        };

        Element::Col(ColElement { w, align, children })
    }

    fn parse_hr(&mut self, attrs: &[crate::scanner::Attribute]) -> Element {
        let mut style = HrStyle::Single;
        let mut fg = None;

        for attr in attrs {
            match attr.name.as_str() {
                "style" => {
                    if let Ok(s) = parse_hr_style(attr_str_value(&attr.value)) {
                        style = s;
                    }
                }
                "fg" => fg = self.parse_color_attr(&attr.value, "hr"),
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [hr]", attr.name),
                    );
                }
            }
        }

        Element::Hr(HrElement { style, fg })
    }

    fn parse_spacer(&mut self, attrs: &[crate::scanner::Attribute]) -> Element {
        let mut lines = 1u16;

        for attr in attrs {
            match attr.name.as_str() {
                "lines" => match parse_u16_attr(&attr.value) {
                    Some(value) if value <= MAX_ELEMENT_DIMENSION => lines = value,
                    _ => self.warning_fmt(
                        "W002",
                        format_args!("spacer lines must be at most {MAX_ELEMENT_DIMENSION}"),
                    ),
                },
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [spacer]", attr.name),
                    );
                }
            }
        }

        Element::Spacer(SpacerElement { lines })
    }

    fn parse_container(
        &mut self,
        name: &str,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Element {
        let mut sticky = None;

        for attr in attrs {
            if attr.name == "sticky"
                && let Ok(s) = parse_sticky(attr_str_value(&attr.value))
            {
                sticky = Some(s);
            }
            // Containers accept any other attrs silently for forward compatibility.
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children(name)
        };

        let container = ContainerElement { sticky, children };

        match name {
            "header" => Element::Header(container),
            "body" => Element::Body(container),
            "footer" => Element::Footer(container),
            "thead" => Element::Thead(container),
            "tbody" => Element::Tbody(container),
            "pagination" => Element::Pagination(container),
            _ => Element::Body(container),
        }
    }

    fn parse_nav(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut sticky = None;

        for attr in attrs {
            if attr.name == "sticky"
                && let Ok(s) = parse_sticky(attr_str_value(&attr.value))
            {
                sticky = Some(s);
            }
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("nav")
        };

        Element::Nav(NavElement { sticky, children })
    }

    // ─── Text element parsers ────────────────────────────────

    fn parse_text(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut elem = TextElement {
            content: String::new(),
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            dim: false,
            blink: false,
            align: Alignment::Left,
            children: Vec::new(),
        };

        for attr in attrs {
            match attr.name.as_str() {
                "fg" => elem.fg = self.parse_color_attr(&attr.value, "text"),
                "bg" => elem.bg = self.parse_color_attr(&attr.value, "text"),
                "bold" => elem.bold = true,
                "italic" => elem.italic = true,
                "underline" => elem.underline = true,
                "strikethrough" => elem.strikethrough = true,
                "dim" => elem.dim = true,
                "blink" => elem.blink = true,
                "align" => {
                    if let Ok(a) = parse_alignment(attr_str_value(&attr.value)) {
                        elem.align = a;
                    }
                }
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [text]", attr.name),
                    );
                }
            }
        }

        if !self_closing {
            // Collect text content and child elements
            let mut children = self.parse_children("text");

            // Optimization: if the only child is a plain text element,
            // absorb its content into elem.content (avoids nesting).
            // Otherwise, keep all children in order for inline layout.
            if children.len() == 1
                && let Some(Element::Text(t)) = children.first()
                && t.children.is_empty()
            {
                let Some(Element::Text(text)) = children.pop() else {
                    unreachable!("the sole child was just matched as text")
                };
                elem.content = text.content;
                return Element::Text(elem);
            }
            elem.children = children;
        }

        Element::Text(elem)
    }

    fn parse_pre(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut fg = None;
        let mut bg = None;
        let mut align = crate::parser::ast::Alignment::Left;

        for attr in attrs {
            match attr.name.as_str() {
                "fg" => fg = self.parse_color_attr(&attr.value, "pre"),
                "bg" => bg = self.parse_color_attr(&attr.value, "pre"),
                "align" => {
                    if let Ok(a) = parse_alignment(attr_str_value(&attr.value)) {
                        align = a;
                    }
                }
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [pre]", attr.name),
                    );
                }
            }
        }

        let runs = if self_closing {
            Vec::new()
        } else {
            self.collect_styled_runs("pre")
        };

        Element::Pre(PreElement {
            runs,
            fg,
            bg,
            align,
        })
    }

    fn parse_heading(
        &mut self,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Element {
        let mut level = 1u8;
        let mut fg = None;

        for attr in attrs {
            match attr.name.as_str() {
                "level" => {
                    level = attr_str_value(&attr.value).parse().unwrap_or(1).clamp(1, 6);
                }
                "fg" => fg = self.parse_color_attr(&attr.value, "heading"),
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [heading]", attr.name),
                    );
                }
            }
        }

        let mut content = String::new();
        let mut children = Vec::new();

        if !self_closing {
            for child in self.parse_children("heading") {
                match child {
                    Element::Text(t) if content.is_empty() && t.children.is_empty() => {
                        content = t.content;
                    }
                    _ => try_push(&mut children, child, &mut self.allocation_failed),
                }
            }
        }

        Element::Heading(HeadingElement {
            level,
            fg,
            content,
            children,
        })
    }

    fn parse_list(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut style = ListStyle::Bullet;
        let mut bullet_char = None;

        for attr in attrs {
            match attr.name.as_str() {
                "style" => {
                    if let Ok(s) = parse_list_style(attr_str_value(&attr.value)) {
                        style = s;
                    }
                }
                "bullet-char" => {
                    let val = attr_str_value(&attr.value);
                    bullet_char = val.chars().next();
                }
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [list]", attr.name),
                    );
                }
            }
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("list")
        };

        Element::List(ListElement {
            style,
            bullet_char,
            children,
        })
    }

    fn parse_item(&mut self, _attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("item")
        };

        Element::Item(ItemElement { children })
    }

    // ─── Interactive element parsers ─────────────────────────

    fn parse_link(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut id = None;
        let mut href = String::new();
        let mut key = None;
        let mut prefetch = false;
        let mut transition = None;
        let mut transition_duration_ms = 300u32;
        let mut defer_animation = None;

        for attr in attrs {
            match attr.name.as_str() {
                "id" => id = Some(self.copy_string(attr_str_value(&attr.value))),
                "href" => href = self.copy_string(attr_str_value(&attr.value)),
                "transition" => {
                    transition = self.transition_attr("link", attr_str_value(&attr.value));
                }
                "duration" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        transition_duration_ms = d;
                    }
                }
                "key" => key = attr_str_value(&attr.value).chars().next(),
                "prefetch" => prefetch = true,
                "defer" => defer_animation = Some(self.copy_string(attr_str_value(&attr.value))),
                "trigger-focus" | "trigger-blur" | "trigger-hover" | "trigger-unhover" => {}
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [link]", attr.name),
                    );
                }
            }
        }

        let triggers = self.parse_triggers(attrs);

        if href.is_empty() {
            self.error("E011", "link element requires href attribute");
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("link")
        };

        Element::Link(LinkElement {
            id,
            href,
            key,
            prefetch,
            transition,
            transition_duration_ms,
            defer_animation,
            triggers,
            children,
        })
    }

    fn parse_input(&mut self, attrs: &[crate::scanner::Attribute]) -> Element {
        let mut id = None;
        let mut name = String::new();
        let mut maxlen = 1024u32;
        let mut placeholder = None;
        let mut multiline = false;
        let mut rows = 1u16;
        let mut password = false;
        let mut value = None;

        for attr in attrs {
            match attr.name.as_str() {
                "id" => id = Some(self.copy_string(attr_str_value(&attr.value))),
                "name" => name = self.copy_string(attr_str_value(&attr.value)),
                "maxlen" => {
                    maxlen = attr_str_value(&attr.value).parse().unwrap_or(1024);
                }
                "placeholder" => placeholder = Some(self.copy_string(attr_str_value(&attr.value))),
                "multiline" => multiline = true,
                "rows" => {
                    rows = parse_u16_attr(&attr.value)
                        .filter(|value| *value <= MAX_ELEMENT_DIMENSION)
                        .unwrap_or(1)
                }
                "password" => password = true,
                "value" => value = Some(self.copy_string(attr_str_value(&attr.value))),
                "trigger-focus" | "trigger-blur" | "trigger-hover" | "trigger-unhover" => {}
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [input]", attr.name),
                    );
                }
            }
        }

        let triggers = self.parse_triggers(attrs);

        if name.is_empty() {
            self.error("E011", "input element requires name attribute");
        }

        Element::Input(InputElement {
            id,
            name,
            maxlen,
            placeholder,
            multiline,
            rows,
            password,
            value,
            triggers,
        })
    }

    fn parse_select(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut name = String::new();
        let mut label = None;

        for attr in attrs {
            match attr.name.as_str() {
                "name" => name = self.copy_string(attr_str_value(&attr.value)),
                "label" => label = Some(self.copy_string(attr_str_value(&attr.value))),
                _ => {}
            }
        }

        if name.is_empty() {
            self.error("E011", "select element requires name attribute");
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("select")
        };

        let option_count = children
            .iter()
            .filter(|child| matches!(child, Element::Option(_)))
            .count();
        let selected_count = children
            .iter()
            .filter(|child| matches!(child, Element::Option(option) if option.selected))
            .count();
        if option_count == 0 {
            self.error("E011", "select element requires at least one option");
        }
        if selected_count > 1 {
            self.error(
                "E011",
                "select element may have at most one selected option",
            );
        }

        Element::Select(SelectElement {
            name,
            label,
            children,
        })
    }

    fn parse_option(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut value = String::new();
        let mut selected = false;

        for attr in attrs {
            match attr.name.as_str() {
                "value" => value = self.copy_string(attr_str_value(&attr.value)),
                "selected" => selected = true,
                _ => {}
            }
        }

        let label = if self_closing {
            self.copy_string(&value)
        } else {
            trim_owned_string(self.collect_text_content("option"))
        };

        Element::Option(OptionElement {
            value,
            selected,
            label,
        })
    }

    fn parse_button(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut action = ButtonAction::Submit;
        let mut target = None;
        let mut href = None;
        let mut key = None;
        let mut states = None;
        let mut to = None;
        let mut transition = None;
        let mut transition_duration_ms = 300u32;

        for attr in attrs {
            match attr.name.as_str() {
                "action" => {
                    if let Ok(a) = parse_button_action(attr_str_value(&attr.value)) {
                        action = a;
                    }
                }
                "target" => target = Some(self.copy_string(attr_str_value(&attr.value))),
                "href" => href = Some(self.copy_string(attr_str_value(&attr.value))),
                "key" => key = attr_str_value(&attr.value).chars().next(),
                "states" => {
                    let val = attr_str_value(&attr.value);
                    let mut parsed_states = Vec::new();
                    for state in val
                        .split(',')
                        .map(str::trim)
                        .filter(|state| !state.is_empty())
                    {
                        let state = self.copy_string(state);
                        try_push(&mut parsed_states, state, &mut self.allocation_failed);
                    }
                    states = Some(parsed_states);
                }
                "to" => to = Some(self.copy_string(attr_str_value(&attr.value))),
                "transition" => {
                    transition = self.transition_attr("state", attr_str_value(&attr.value));
                }
                "duration" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        transition_duration_ms = d;
                    }
                }
                "trigger-focus" | "trigger-blur" | "trigger-hover" | "trigger-unhover" => {}
                _ => {}
            }
        }

        let triggers = self.parse_triggers(attrs);

        // Validate toggle requires states with >= 2 entries
        if action == ButtonAction::Toggle {
            match &states {
                Some(s) if s.len() < 2 => {
                    self.error("E023", "toggle requires at least 2 states");
                }
                None => {
                    self.error("E023", "toggle requires states attribute");
                }
                _ => {}
            }
        }

        // Validate set requires to
        if action == ButtonAction::Set && to.is_none() {
            self.error("E011", "button with action=set requires to attribute");
        }

        let label = if self_closing {
            String::new()
        } else {
            trim_owned_string(self.collect_text_content("button"))
        };

        Element::Button(ButtonElement {
            action,
            target,
            href,
            key,
            label,
            states,
            to,
            transition,
            transition_duration_ms,
            triggers,
        })
    }

    fn parse_form(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut action = String::new();

        for attr in attrs {
            match attr.name.as_str() {
                "action" => action = self.copy_string(attr_str_value(&attr.value)),
                "method" => {} // accepted but not stored separately
                _ => {}
            }
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("form")
        };

        if action.is_empty() {
            self.error("E011", "form element requires action attribute");
        }

        Element::Form(FormElement { action, children })
    }

    // ─── Media element parsers ───────────────────────────────

    fn parse_art(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut width = None;
        let mut height = None;
        let mut encoding = ArtEncoding::Utf8;
        let mut src = None;
        let mut alt = None;

        for attr in attrs {
            match attr.name.as_str() {
                "width" => width = parse_u16_attr(&attr.value),
                "height" => height = parse_u16_attr(&attr.value),
                "encoding" => {
                    if let Ok(e) = parse_art_encoding(attr_str_value(&attr.value)) {
                        encoding = e;
                    }
                }
                "src" => src = Some(self.copy_string(attr_str_value(&attr.value))),
                "alt" => alt = Some(self.copy_string(attr_str_value(&attr.value))),
                _ => {}
            }
        }

        let content = if self_closing {
            String::new()
        } else {
            self.collect_text_content("art")
        };

        Element::Art(ArtElement {
            width,
            height,
            encoding,
            src,
            alt,
            content,
        })
    }

    fn parse_table(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut border = BorderStyle::Single;

        for attr in attrs {
            if attr.name == "border"
                && let Ok(b) = parse_border_style(attr_str_value(&attr.value))
            {
                border = b;
            }
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("table")
        };

        Element::Table(TableElement { border, children })
    }

    fn parse_tr(&mut self, _attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("tr")
        };

        Element::Tr(TrElement { children })
    }

    fn parse_cell(
        &mut self,
        name: &str,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Element {
        let mut fg = None;
        let mut bg = None;
        let mut align = Alignment::Left;

        for attr in attrs {
            match attr.name.as_str() {
                "fg" => fg = self.parse_color_attr(&attr.value, name),
                "bg" => bg = self.parse_color_attr(&attr.value, name),
                "align" => {
                    if let Ok(a) = parse_alignment(attr_str_value(&attr.value)) {
                        align = a;
                    }
                }
                _ => {}
            }
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children(name)
        };

        let cell = CellElement {
            fg,
            bg,
            align,
            children,
        };

        match name {
            "th" => Element::Th(cell),
            _ => Element::Td(cell),
        }
    }

    // ─── Animation element parsers ───────────────────────────

    fn parse_animate(
        &mut self,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Element {
        let mut elem = AnimateElement {
            id: String::new(),
            fps: 10,
            loop_behavior: LoopBehavior::None,
            autoplay: true,
            delay_ms: 0,
            after: None,
            x: None,
            y: None,
            w: None,
            h: None,
            src: None,
            background: false,
            children: Vec::new(),
        };

        for attr in attrs {
            match attr.name.as_str() {
                "id" => elem.id = self.copy_string(attr_str_value(&attr.value)),
                "fps" => {
                    elem.fps = attr_str_value(&attr.value)
                        .parse()
                        .unwrap_or(10)
                        .clamp(1, 30);
                }
                "loop" => {
                    if let Ok(l) = parse_loop(attr_str_value(&attr.value)) {
                        elem.loop_behavior = l;
                    }
                }
                "autoplay" => {
                    elem.autoplay = attr_str_value(&attr.value) != "false";
                }
                "delay" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        elem.delay_ms = d;
                    }
                }
                "after" => elem.after = Some(self.copy_string(attr_str_value(&attr.value))),
                "x" => elem.x = parse_u16_attr(&attr.value),
                "y" => elem.y = parse_u16_attr(&attr.value),
                "w" => elem.w = parse_u16_attr(&attr.value),
                "h" => elem.h = parse_u16_attr(&attr.value),
                "src" => elem.src = Some(self.copy_string(attr_str_value(&attr.value))),
                "background" => {
                    elem.background = attr_str_value(&attr.value) != "false";
                }
                "effect" | "region" => {} // accepted for compat
                _ => {}
            }
        }

        if !self_closing {
            elem.children = self.parse_children("animate");
        }

        Element::Animate(elem)
    }

    fn parse_frame(&mut self, _attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("frame")
        };

        Element::Frame(FrameElement { children })
    }

    fn parse_element_def(
        &mut self,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Element {
        let mut id = String::new();
        let mut x = None;
        let mut y = None;
        let mut fg = None;

        for attr in attrs {
            match attr.name.as_str() {
                "id" => id = self.copy_string(attr_str_value(&attr.value)),
                "x" => x = parse_u16_attr(&attr.value),
                "y" => y = parse_u16_attr(&attr.value),
                "fg" => fg = self.parse_color_attr(&attr.value, "element"),
                _ => {}
            }
        }

        let content = if self_closing {
            String::new()
        } else {
            self.collect_text_content("element")
        };

        Element::ElementDef(ElementDefElement {
            id,
            x,
            y,
            fg,
            content,
        })
    }

    fn parse_tween(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut target = String::new();
        let mut duration_ms = 1000u32;
        let mut loop_behavior = LoopBehavior::None;
        let mut easing = Easing::Linear;
        let mut delay_ms = 0u32;

        for attr in attrs {
            match attr.name.as_str() {
                "target" => target = self.copy_string(attr_str_value(&attr.value)),
                "duration" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        duration_ms = d;
                    }
                }
                "loop" => {
                    if let Ok(l) = parse_loop(attr_str_value(&attr.value)) {
                        loop_behavior = l;
                    }
                }
                "easing" => {
                    if let Ok(e) = parse_easing(attr_str_value(&attr.value)) {
                        easing = e;
                    }
                }
                "delay" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        delay_ms = d;
                    }
                }
                _ => {}
            }
        }

        if target.is_empty() {
            self.error("E014", "tween requires target attribute");
        }

        // Parse [at] keyframes from children
        let mut keyframes = Vec::new();
        if !self_closing {
            loop {
                match self.current_owned() {
                    Token::Eof => break,
                    Token::CloseTag { name } if name == "tween" => {
                        self.advance();
                        break;
                    }
                    Token::CloseTag { .. } => {
                        self.advance();
                    }
                    Token::Text(_) => {
                        self.advance();
                    }
                    Token::OpenTag {
                        name,
                        attributes,
                        self_closing: sc,
                    } => {
                        self.advance();
                        if name == "at" {
                            if let Some(kf) = self.parse_keyframe(&attributes, sc) {
                                try_push(&mut keyframes, kf, &mut self.allocation_failed);
                            }
                        } else if !sc {
                            self.skip_until_close(&name);
                        }
                    }
                }
            }
        }

        Element::Tween(TweenElement {
            target,
            duration_ms,
            loop_behavior,
            easing,
            delay_ms,
            keyframes,
        })
    }

    fn parse_keyframe(
        &mut self,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Option<Keyframe> {
        let mut t_percent = 0.0f32;

        for attr in attrs {
            if attr.name == "t" {
                let val = attr_str_value(&attr.value);
                let val = val.trim_end_matches('%');
                t_percent = val.parse().unwrap_or(0.0);
            }
        }

        // The content of [at] contains property assignments like "x=10 fg=white"
        let content = if self_closing {
            String::new()
        } else {
            self.collect_text_content("at")
        };

        let mut x = None;
        let mut y = None;
        let mut fg = None;
        let mut bg = None;

        // Parse the inline properties
        for part in content.split_whitespace() {
            if let Some((key, val)) = part.split_once('=') {
                match key {
                    "x" => x = val.parse().ok(),
                    "y" => y = val.parse().ok(),
                    "fg" => fg = parse_color(val).ok(),
                    "bg" => bg = parse_color(val).ok(),
                    _ => {}
                }
            }
        }

        Some(Keyframe {
            t_percent,
            x,
            y,
            fg,
            bg,
        })
    }

    fn parse_text_animate(
        &mut self,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Element {
        let mut effect = TextEffect::Typewriter;
        let mut speed_ms = 50u32;
        let mut direction = None;

        for attr in attrs {
            match attr.name.as_str() {
                "effect" => {
                    if let Ok(e) = parse_text_effect(attr_str_value(&attr.value)) {
                        effect = e;
                    }
                }
                "speed" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        speed_ms = d;
                    }
                }
                "direction" => {
                    if let Ok(d) = parse_direction(attr_str_value(&attr.value)) {
                        direction = Some(d);
                    }
                }
                _ => {}
            }
        }

        let content = if self_closing {
            String::new()
        } else {
            trim_owned_string(self.collect_text_content("text-animate"))
        };

        Element::TextAnimate(TextAnimateElement {
            effect,
            speed_ms,
            direction,
            content,
        })
    }

    // ─── Live element parser ─────────────────────────────────

    /// Parse `[include name=... /]`.
    ///
    /// Always treated as self-closing — an `[include]` has no children, because
    /// whatever a page author wrote inside one would be discarded when the
    /// server replaced the element. Accepting children and then dropping them
    /// silently would be worse than not accepting them, so a closing tag is
    /// simply never consumed and `[/include]` surfaces as a stray close tag.
    fn parse_include(&mut self, attrs: &[crate::scanner::Attribute]) -> Element {
        let mut name = String::new();

        for attr in attrs {
            if attr.name == "name" {
                name = self.copy_string(attr_str_value(&attr.value));
            }
        }

        if name.is_empty() {
            self.error("E011", "include element requires name attribute");
        }

        Element::Include(IncludeElement { name })
    }

    fn parse_live(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut id = String::new();
        let mut endpoint = String::new();
        let mut height = Dimension::Fill;
        let mut scroll = LiveScroll::Tail;
        let mut buffer = 100u32;
        let mut delta = false;

        for attr in attrs {
            match attr.name.as_str() {
                "id" => id = self.copy_string(attr_str_value(&attr.value)),
                "endpoint" => endpoint = self.copy_string(attr_str_value(&attr.value)),
                "height" => {
                    if let Ok(d) = parse_dimension(attr_str_value(&attr.value)) {
                        height = d;
                    }
                }
                "scroll" => {
                    if let Ok(s) = parse_live_scroll(attr_str_value(&attr.value)) {
                        scroll = s;
                    }
                }
                "buffer" => {
                    buffer = attr_str_value(&attr.value).parse().unwrap_or(100);
                }
                "delta" => {
                    delta = attr_str_value(&attr.value) != "false";
                }
                "position" => {} // accepted for compat
                _ => {}
            }
        }

        if id.is_empty() {
            self.error("E011", "live element requires id attribute");
        }
        if endpoint.is_empty() {
            self.error("E011", "live element requires endpoint attribute");
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("live")
        };

        Element::Live(LiveElement {
            id,
            endpoint,
            height,
            scroll,
            buffer,
            delta,
            children,
        })
    }

    // ─── Panel/State parsers ────────────────────────────────

    fn parse_panel(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut id = String::new();
        let mut initial_state = String::new();

        for attr in attrs {
            match attr.name.as_str() {
                "id" => id = self.copy_string(attr_str_value(&attr.value)),
                "state" => initial_state = self.copy_string(attr_str_value(&attr.value)),
                _ => {}
            }
        }

        if id.is_empty() {
            self.error("E011", "panel requires id attribute");
        }
        if initial_state.is_empty() {
            self.error(
                "E026",
                "panel requires state attribute (initial state name)",
            );
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("panel")
        };

        // Validate: panel must have at least one state child
        let state_count = children
            .iter()
            .filter(|c| matches!(c, Element::State(_)))
            .count();
        if state_count == 0 && !self_closing {
            self.error_fmt(
                "E025",
                format_args!("panel \"{id}\" has no [state] children"),
            );
        }

        // Validate: no duplicate state names
        for (index, child) in children.iter().enumerate() {
            if let Element::State(state) = child
                && children
                    .get(..index)
                    .unwrap_or(&[])
                    .iter()
                    .any(|prior| matches!(prior, Element::State(other) if other.name == state.name))
            {
                self.error_fmt(
                    "E024",
                    format_args!("duplicate state name \"{}\" in panel \"{id}\"", state.name),
                );
            }
        }

        // Validate: initial state exists
        if !initial_state.is_empty() && state_count > 0 {
            let initial_exists = children
                .iter()
                .any(|c| matches!(c, Element::State(s) if s.name == initial_state));
            if !initial_exists {
                self.error_fmt(
                    "E026",
                    format_args!("initial state \"{initial_state}\" not found in panel \"{id}\""),
                );
            }
        }

        Element::Panel(PanelElement {
            id,
            initial_state,
            children,
        })
    }

    fn parse_state(&mut self, attrs: &[crate::scanner::Attribute], self_closing: bool) -> Element {
        let mut name = String::new();
        let mut transition = None;
        let mut duration_ms = 200u32;
        let mut x = None;
        let mut y = None;
        let mut w = None;
        let mut h = None;

        for attr in attrs {
            match attr.name.as_str() {
                "name" => name = self.copy_string(attr_str_value(&attr.value)),
                "transition" => {
                    // Kept as the authored string -- the client resolves it when
                    // it builds the scene -- but checked here, because an
                    // unrecognised name silently becomes a cut and a page that
                    // pops instead of animating looks like a broken panel rather
                    // than a name this build has never heard of.
                    let value = attr_str_value(&attr.value);
                    let _ = self.transition_attr("state", value);
                    transition = Some(self.copy_string(value));
                }
                "duration" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        duration_ms = d;
                    }
                }
                "x" => x = parse_u16_attr(&attr.value),
                "y" => y = parse_u16_attr(&attr.value),
                "w" => {
                    w = Some(
                        parse_dimension(attr_str_value(&attr.value)).unwrap_or(Dimension::Fill),
                    )
                }
                "h" => {
                    h = Some(parse_dimension(attr_str_value(&attr.value)).unwrap_or(Dimension::Fit))
                }
                _ => {}
            }
        }

        if name.is_empty() {
            self.error("E011", "state requires name attribute");
        }

        let children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("state")
        };

        Element::State(StateElement {
            name,
            transition,
            duration_ms,
            x,
            y,
            w,
            h,
            children,
        })
    }

    // ─── Event binding parser ─────────────────────────────────

    fn parse_on(&mut self, attrs: &[crate::scanner::Attribute]) -> Element {
        let mut event = None;
        let mut source = None;
        let mut action = None;
        let mut target = String::new();
        let mut to = None;
        let mut delay_ms = 0u32;

        for attr in attrs {
            match attr.name.as_str() {
                "event" => match parse_event_kind(attr_str_value(&attr.value)) {
                    Ok(e) => event = Some(e),
                    Err(e) => self.error_fmt("E041", format_args!("invalid event kind: {e}")),
                },
                "source" => source = Some(self.copy_string(attr_str_value(&attr.value))),
                "do" => match parse_action_kind(attr_str_value(&attr.value)) {
                    Ok(a) => action = Some(a),
                    Err(e) => self.error_fmt("E042", format_args!("invalid action kind: {e}")),
                },
                "target" => target = self.copy_string(attr_str_value(&attr.value)),
                "to" => to = Some(self.copy_string(attr_str_value(&attr.value))),
                "delay" => {
                    if let Ok(d) = parse_duration_ms(attr_str_value(&attr.value)) {
                        delay_ms = d;
                    }
                }
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [on]", attr.name),
                    );
                }
            }
        }

        if event.is_none() {
            self.error("E041", "[on] requires event attribute");
        }
        if action.is_none() {
            self.error("E042", "[on] requires do attribute");
        }
        if target.is_empty() {
            self.error("E011", "[on] requires target attribute");
        }

        // animation-end requires a source to know which animation finished
        if matches!(event, Some(EventKind::AnimationEnd)) && source.is_none() {
            self.error(
                "E043",
                "[on] with event=animation-end requires source attribute",
            );
        }

        Element::On(OnElement {
            event: event.unwrap_or(EventKind::PageLoad),
            source,
            action: action.unwrap_or(ActionKind::Animate),
            target,
            to,
            delay_ms,
        })
    }

    /// Parse trigger attributes (trigger-focus, trigger-blur, etc.) from an attribute list.
    fn parse_triggers(&mut self, attrs: &[crate::scanner::Attribute]) -> TriggerAttrs {
        let mut triggers = TriggerAttrs::default();

        for attr in attrs {
            match attr.name.as_str() {
                "trigger-focus" => {
                    if let Some(tr) = self.parse_trigger_ref(attr_str_value(&attr.value)) {
                        triggers.trigger_focus = Some(tr);
                    } else {
                        self.warning("E011", "invalid trigger-focus value");
                    }
                }
                "trigger-blur" => {
                    if let Some(tr) = self.parse_trigger_ref(attr_str_value(&attr.value)) {
                        triggers.trigger_blur = Some(tr);
                    } else {
                        self.warning("E011", "invalid trigger-blur value");
                    }
                }
                "trigger-hover" => {
                    if let Some(tr) = self.parse_trigger_ref(attr_str_value(&attr.value)) {
                        triggers.trigger_hover = Some(tr);
                    } else {
                        self.warning("E011", "invalid trigger-hover value");
                    }
                }
                "trigger-unhover" => {
                    if let Some(tr) = self.parse_trigger_ref(attr_str_value(&attr.value)) {
                        triggers.trigger_unhover = Some(tr);
                    } else {
                        self.warning("E011", "invalid trigger-unhover value");
                    }
                }
                _ => {} // non-trigger attrs handled by caller
            }
        }

        triggers
    }

    // ─── Details ───────────────────────────────────────────────

    fn parse_details(
        &mut self,
        attrs: &[crate::scanner::Attribute],
        self_closing: bool,
    ) -> Element {
        let mut summary = String::new();
        let mut open = false;

        for attr in attrs {
            match attr.name.as_str() {
                "summary" => summary = self.copy_string(attr_str_value(&attr.value)),
                "open" => open = true,
                _ => {
                    self.warning_fmt(
                        "W002",
                        format_args!("unknown attribute \"{}\" on [details]", attr.name),
                    );
                }
            }
        }

        let all_children = if self_closing {
            Vec::new()
        } else {
            self.parse_children("details")
        };

        // If the first child is a [text] that starts with the summary marker
        // prefix "\x01", extract it as inline summary content (supports links).
        // Otherwise, summary_children stays empty and we use the plain text attribute.
        let mut summary_children = Vec::new();
        let mut body_children = Vec::new();

        let mut iter = all_children.into_iter();
        if let Some(first) = iter.next() {
            if summary.is_empty() {
                // No summary attribute — use the first child as the summary content
                match first {
                    Element::Text(t) => {
                        if t.children.is_empty() {
                            summary = t.content;
                        } else {
                            // Inline summary with children (links, styled text)
                            try_push(
                                &mut summary_children,
                                Element::Text(t),
                                &mut self.allocation_failed,
                            );
                        }
                    }
                    other => try_push(&mut body_children, other, &mut self.allocation_failed),
                }
            } else {
                try_push(&mut body_children, first, &mut self.allocation_failed);
            }
        }
        for child in iter {
            try_push(&mut body_children, child, &mut self.allocation_failed);
        }

        if summary.is_empty() && summary_children.is_empty() {
            self.error(
                "E011",
                "details element requires summary attribute or text content",
            );
        }

        Element::Details(DetailsElement {
            summary,
            summary_children,
            open,
            children: body_children,
        })
    }

    // ─── Meta/Style ──────────────────────────────────────────

    fn parse_meta(&mut self, attrs: &[crate::scanner::Attribute]) {
        // Meta is self-closing, no children to parse.
        // Currently we just validate it exists. Meta entries
        // will be accessible through the page's children if needed.
        for _attr in attrs {
            // Accept any attributes silently
        }
    }

    fn parse_style_element(&mut self, attrs: &[crate::scanner::Attribute]) {
        for _attr in attrs {
            // Accept any attributes silently
        }
    }

    // ─── Helpers ─────────────────────────────────────────────

    /// Collect all text content inside a tag, discarding child tags.
    /// Collect a preformatted block as styled spans in source order.
    ///
    /// The plain-text collector below skips nested tags *and their content*,
    /// which for a block that can carry styling means a `[text]` span silently
    /// disappears from the page while the document still validates. Nothing
    /// reports it, because from the parser's point of view nothing went wrong.
    ///
    /// Order is why this cannot reuse the `content` + `children` shape that
    /// `[text]` uses: that shape loses the interleaving between an element's own
    /// text and its children, which flowing text can absorb and a preformatted
    /// grid cannot. `IE· `, `GB`, `· BE·` has to stay in that sequence.
    ///
    /// Nested styling inherits and flattens: `[text bold][text fg=red]x[/text]`
    /// yields one bold red run. Only `[text]` may nest here -- anything else is
    /// reported rather than dropped, which is the behaviour the plain collector
    /// was missing.
    fn collect_styled_runs(&mut self, until_close: &str) -> Vec<PreRun> {
        let mut runs: Vec<PreRun> = Vec::new();
        let mut stack: Vec<PreRun> = Vec::new();

        loop {
            match self.current_owned() {
                Token::Eof => break,
                Token::CloseTag { name } if name == until_close && stack.is_empty() => {
                    self.advance();
                    break;
                }
                Token::CloseTag { .. } => {
                    self.advance();
                    stack.pop();
                }
                Token::Text(ref text) => {
                    let style = stack.last().cloned().unwrap_or_default();
                    // Merged with the run before it when the styling matches, so
                    // that adjacent text tokens do not become separate runs and a
                    // block with no styling stays exactly one run.
                    match runs.last_mut() {
                        Some(last) if same_style(last, &style) => last.text.push_str(text),
                        _ => {
                            let mut run = style;
                            run.text.push_str(text);
                            runs.push(run);
                        }
                    }
                    self.advance();
                }
                Token::OpenTag {
                    name,
                    ref attributes,
                    self_closing,
                } => {
                    if name != "text" {
                        self.error_fmt(
                            "E020",
                            format_args!(
                                "[{name}] is not allowed inside [{until_close}]; only [text] may nest"
                            ),
                        );
                        self.advance();
                        if !self_closing {
                            self.skip_until_close(&name);
                        }
                        continue;
                    }
                    let mut style = stack.last().cloned().unwrap_or_default();
                    style.text.clear();
                    for attr in attributes {
                        match attr.name.as_str() {
                            "fg" => style.fg = self.parse_color_attr(&attr.value, "text"),
                            "bg" => style.bg = self.parse_color_attr(&attr.value, "text"),
                            "bold" => style.bold = true,
                            "italic" => style.italic = true,
                            "underline" => style.underline = true,
                            "strikethrough" => style.strikethrough = true,
                            "dim" => style.dim = true,
                            "blink" => style.blink = true,
                            _ => {
                                self.warning_fmt(
                                    "W002",
                                    format_args!(
                                        "unknown attribute \"{}\" on [text] inside [{until_close}]",
                                        attr.name
                                    ),
                                );
                            }
                        }
                    }
                    self.advance();
                    if !self_closing {
                        stack.push(style);
                    }
                }
            }
        }

        runs
    }

    fn collect_text_content(&mut self, until_close: &str) -> String {
        let mut content = String::new();

        loop {
            match self.current_owned() {
                Token::Eof => break,
                Token::CloseTag { name } if name == until_close => {
                    self.advance();
                    break;
                }
                Token::CloseTag { .. } => {
                    self.advance();
                }
                Token::Text(ref text) => {
                    content.push_str(text);
                    self.advance();
                }
                Token::OpenTag {
                    name, self_closing, ..
                } => {
                    self.advance();
                    if !self_closing {
                        // Skip nested tags and their content
                        self.skip_until_close(&name);
                    }
                }
            }
        }

        content
    }

    /// Skip tokens until we find the matching close tag.
    fn skip_until_close(&mut self, tag: &str) {
        let mut depth = 1u32;
        loop {
            match self.current_owned() {
                Token::Eof => break,
                Token::OpenTag {
                    ref name,
                    self_closing,
                    ..
                } => {
                    if !self_closing && name == tag {
                        depth += 1;
                    }
                    self.advance();
                }
                Token::CloseTag { ref name } if name == tag => {
                    depth -= 1;
                    self.advance();
                    if depth == 0 {
                        break;
                    }
                }
                Token::CloseTag { .. } => self.advance(),
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn skip_text(&mut self) {
        while let Token::Text(ref t) = self.current_owned() {
            if t.trim().is_empty() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn parse_color_attr(
        &mut self,
        value: &AttributeValue,
        element_name: &str,
    ) -> Option<crate::color::Color> {
        let val = attr_str_value(value);
        match parse_color(val) {
            Ok(c) => Some(c),
            Err(e) => {
                self.warning_fmt(
                    "E011",
                    format_args!("invalid color \"{val}\" on [{element_name}]: {e}"),
                );
                None
            }
        }
    }
}

// ─── Attribute helper functions ──────────────────────────────

/// Extract the string value from an attribute value.
fn attr_str_value(val: &AttributeValue) -> &str {
    match val {
        AttributeValue::String(s) | AttributeValue::Ident(s) => s,
        AttributeValue::Flag => "",
    }
}

/// Parse a u16 from an attribute value.
fn parse_u16_attr(val: &AttributeValue) -> Option<u16> {
    attr_str_value(val)
        .parse()
        .ok()
        .filter(|value| *value <= MAX_ELEMENT_DIMENSION)
}
