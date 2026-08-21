use crate::color::Color;

/// A parsed AML document.
#[derive(Debug, Clone)]
pub struct Document {
    pub page: Page,
}

/// The root `[page]` element.
#[derive(Debug, Clone)]
pub struct Page {
    pub mode: PageMode,
    pub title: Option<String>,
    pub meta: Vec<MetaEntry>,
    pub style: Option<StyleDefaults>,
    /// Page entrance transition effect (e.g. fade, slide-left).
    pub transition: Option<TransitionKind>,
    /// Duration of the entrance transition in milliseconds.
    pub transition_duration_ms: u32,
    pub children: Vec<Element>,
}

/// Page layout mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PageMode {
    /// Scrollable content that flows vertically.
    Document,
    /// Fixed canvas with absolute positioning.
    /// When cols/rows are None, they default to terminal dimensions.
    Screen {
        cols: Option<u16>,
        rows: Option<u16>,
    },
}

/// A `[meta]` key-value entry.
#[derive(Debug, Clone)]
pub struct MetaEntry {
    pub key: String,
    pub value: String,
}

/// Default styles from `[style]`.
#[derive(Debug, Clone)]
pub struct StyleDefaults {
    pub default_fg: Option<Color>,
    pub default_bg: Option<Color>,
}

/// Any AML element.
#[derive(Debug, Clone)]
pub enum Element {
    // Layout
    Box(BoxElement),
    Row(RowElement),
    Col(ColElement),
    Hr(HrElement),
    Spacer(SpacerElement),
    Header(ContainerElement),
    Body(ContainerElement),
    Footer(ContainerElement),
    Nav(NavElement),

    // Text
    Text(TextElement),
    Pre(PreElement),
    Heading(HeadingElement),
    List(ListElement),
    Item(ItemElement),

    // Interactive
    Link(LinkElement),
    Input(InputElement),
    Select(SelectElement),
    Option(OptionElement),
    Button(ButtonElement),
    Form(FormElement),

    // Media
    Art(ArtElement),
    Table(TableElement),
    Thead(ContainerElement),
    Tbody(ContainerElement),
    Tr(TrElement),
    Th(CellElement),
    Td(CellElement),

    // Animation (parsed, rendered later)
    Animate(AnimateElement),
    Frame(FrameElement),
    ElementDef(ElementDefElement),
    Tween(TweenElement),
    TextAnimate(TextAnimateElement),

    // Live content (parsed, rendered later)
    Live(LiveElement),

    // Interactive panels
    Panel(PanelElement),
    State(StateElement),

    // Collapsible details
    Details(DetailsElement),

    // Pagination
    Pagination(ContainerElement),

    // Event bindings (non-visual, declarative)
    On(OnElement),

    // Server-resolved placeholder
    Include(IncludeElement),
}

// ─── Layout Elements ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BoxElement {
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub w: Dimension,
    pub h: Dimension,
    pub border: BorderStyle,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub padding: u16,
    pub title: Option<String>,
    pub join_top: Option<u16>,
    pub join_bottom: Option<u16>,
    pub join_left: Option<u16>,
    pub join_right: Option<u16>,
    pub align: Alignment,
    pub sticky: Option<StickyPosition>,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct RowElement {
    pub gap: u16,
    pub align: VerticalAlignment,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct ColElement {
    pub w: Dimension,
    pub align: Alignment,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct HrElement {
    pub style: HrStyle,
    pub fg: Option<Color>,
}

#[derive(Debug, Clone)]
pub struct SpacerElement {
    pub lines: u16,
}

#[derive(Debug, Clone)]
pub struct ContainerElement {
    pub sticky: Option<StickyPosition>,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct NavElement {
    pub sticky: Option<StickyPosition>,
    pub children: Vec<Element>,
}

// ─── Text Elements ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TextElement {
    pub content: String,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub blink: bool,
    pub align: Alignment,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct PreElement {
    pub content: String,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub align: Alignment,
}

#[derive(Debug, Clone)]
pub struct HeadingElement {
    pub level: u8,
    pub fg: Option<Color>,
    pub content: String,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct ListElement {
    pub style: ListStyle,
    pub bullet_char: Option<char>,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct ItemElement {
    pub children: Vec<Element>,
}

// ─── Interactive Elements ────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LinkElement {
    pub id: Option<String>,
    pub href: String,
    pub key: Option<char>,
    pub prefetch: bool,
    /// Transition effect when navigating via this link (overrides page default).
    pub transition: Option<TransitionKind>,
    /// Duration of the link's transition in milliseconds.
    pub transition_duration_ms: u32,
    /// Optional animation that must finish before navigation begins.
    pub defer_animation: Option<String>,
    pub triggers: TriggerAttrs,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct InputElement {
    pub id: Option<String>,
    pub name: String,
    pub maxlen: u32,
    pub placeholder: Option<String>,
    pub multiline: bool,
    pub rows: u16,
    pub password: bool,
    pub value: Option<String>,
    pub triggers: TriggerAttrs,
}

#[derive(Debug, Clone)]
pub struct SelectElement {
    pub name: String,
    pub label: Option<String>,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct OptionElement {
    pub value: String,
    pub selected: bool,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct ButtonElement {
    pub action: ButtonAction,
    pub target: Option<String>,
    pub href: Option<String>,
    pub key: Option<char>,
    pub label: String,
    /// Comma-separated state names for toggle action.
    pub states: Option<Vec<String>>,
    /// Target state name for set action.
    pub to: Option<String>,
    /// Transition effect when navigating via this button (overrides page default).
    pub transition: Option<TransitionKind>,
    /// Duration of the button's transition in milliseconds.
    pub transition_duration_ms: u32,
    /// Trigger attributes for panel state changes.
    pub triggers: TriggerAttrs,
}

#[derive(Debug, Clone)]
pub struct FormElement {
    pub action: String,
    pub children: Vec<Element>,
}

// ─── Panel Elements ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PanelElement {
    pub id: String,
    pub initial_state: String,
    pub children: Vec<Element>, // should contain only State elements
}

#[derive(Debug, Clone)]
pub struct StateElement {
    pub name: String,
    pub transition: Option<String>,
    pub duration_ms: u32,
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub w: Option<Dimension>,
    pub h: Option<Dimension>,
    pub children: Vec<Element>,
}

// ─── Details Element ─────────────────────────────────────────

/// A collapsible details element: shows summary line when collapsed,
/// summary + children when expanded.
#[derive(Debug, Clone)]
pub struct DetailsElement {
    pub summary: String,
    /// Optional inline elements for the summary line (supports links, styled text).
    /// When non-empty, these are rendered instead of the plain `summary` string.
    pub summary_children: Vec<Element>,
    pub open: bool,
    pub children: Vec<Element>,
}

/// A reference to a panel state, e.g. "panel-id:state-name".
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerRef {
    pub panel_id: String,
    pub state_name: String,
}

/// Trigger attributes that can appear on interactive elements.
#[derive(Debug, Clone, Default)]
pub struct TriggerAttrs {
    pub trigger_focus: Option<TriggerRef>,
    pub trigger_blur: Option<TriggerRef>,
    pub trigger_hover: Option<TriggerRef>,
    pub trigger_unhover: Option<TriggerRef>,
}

// ─── Event Binding Elements ─────────────────────────────────

/// A declarative event→action binding: `[on event="focus" source="id" do="animate" target="id"]`.
///
/// Non-visual. Collected during layout as metadata and dispatched at runtime.
#[derive(Debug, Clone)]
pub struct OnElement {
    pub event: EventKind,
    pub source: Option<String>,
    pub action: ActionKind,
    pub target: String,
    /// Target state name for `set` action.
    pub to: Option<String>,
    /// Delay in milliseconds before the action fires.
    pub delay_ms: u32,
}

/// Events that can trigger an `[on]` binding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventKind {
    /// User tabs/arrows to a focusable element.
    Focus,
    /// User leaves a focusable element.
    Blur,
    /// A panel transitions to a new state.
    StateChange,
    /// Page finishes initial render.
    PageLoad,
    /// Element scrolls into the viewport.
    ScrollIntoView,
    /// A named animation completes.
    AnimationEnd,
    /// User activates (Enter) a focusable element.
    Select,
}

/// Actions that an `[on]` binding can perform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActionKind {
    /// Start or restart a named animation.
    Animate,
    /// Set a panel to a specific state.
    Set,
    /// Cycle a panel through its states.
    Toggle,
    /// Stop a running animation.
    Stop,
}

// ─── Media Elements ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ArtElement {
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub encoding: ArtEncoding,
    pub src: Option<String>,
    pub alt: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct TableElement {
    pub border: BorderStyle,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct TrElement {
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct CellElement {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub align: Alignment,
    pub children: Vec<Element>,
}

// ─── Animation Elements ──────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AnimateElement {
    pub id: String,
    pub fps: u8,
    pub loop_behavior: LoopBehavior,
    pub autoplay: bool,
    pub delay_ms: u32,
    pub after: Option<String>,
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub w: Option<u16>,
    pub h: Option<u16>,
    /// WASM module source path (e.g. "/effects/rain.wasm").
    /// When present, animation is driven by a WASM module.
    pub src: Option<String>,
    /// When true, this animation renders behind the base content (z=-1)
    /// instead of on top (z=10). Useful for full-page background effects.
    pub background: bool,
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct FrameElement {
    pub children: Vec<Element>,
}

#[derive(Debug, Clone)]
pub struct ElementDefElement {
    pub id: String,
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub fg: Option<Color>,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct TweenElement {
    pub target: String,
    pub duration_ms: u32,
    pub loop_behavior: LoopBehavior,
    pub easing: Easing,
    pub delay_ms: u32,
    pub keyframes: Vec<Keyframe>,
}

#[derive(Debug, Clone)]
pub struct Keyframe {
    pub t_percent: f32,
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

#[derive(Debug, Clone)]
pub struct TextAnimateElement {
    pub effect: TextEffect,
    pub speed_ms: u32,
    pub direction: Option<Direction>,
    pub content: String,
}

// ─── Live Elements ───────────────────────────────────────────

/// A named placeholder for content a server generates, written
/// `[include name=links /]`.
///
/// The server replaces the whole element with generated content before sending
/// the page, so a client never sees one: an `[include]` arriving over the wire
/// means the origin has no handler for that name, and it renders as nothing
/// rather than as the literal text `[include ...]`.
///
/// Distinct from `[slot]`, which the component system already owns for marking
/// where a `[def]`'s caller content goes. That one is resolved at parse time,
/// entirely within the document; this one is resolved at serve time, from
/// outside it.
#[derive(Debug, Clone)]
pub struct IncludeElement {
    /// Which handler is expected to fill this. Required.
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct LiveElement {
    pub id: String,
    pub endpoint: String,
    pub height: Dimension,
    pub scroll: LiveScroll,
    pub buffer: u32,
    /// When true, the client requests delta updates from the server.
    /// The server sends only new content appended since the last update.
    pub delta: bool,
    pub children: Vec<Element>,
}

// ─── Enums ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Dimension {
    Fixed(u16),
    #[default]
    Fill,
    Fit,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BorderStyle {
    None,
    #[default]
    Single,
    Double,
    Rounded,
    Heavy,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Alignment {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum VerticalAlignment {
    #[default]
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StickyPosition {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum HrStyle {
    #[default]
    Single,
    Double,
    Heavy,
    Dash,
    Dot,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ListStyle {
    #[default]
    Bullet,
    Number,
    Dash,
    Arrow,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ButtonAction {
    #[default]
    Submit,
    Navigate,
    Toggle,
    Set,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ArtEncoding {
    #[default]
    Utf8,
    Cp437,
    Petscii,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LoopBehavior {
    /// Don't loop — play once.
    #[default]
    None,
    /// Loop forever.
    Infinite,
    /// Loop N times.
    Count(u32),
    /// Ping-pong (play forward, then backward, repeat).
    Bounce,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Easing {
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    Step,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextEffect {
    Typewriter,
    Reveal,
    Scramble,
    FadeIn,
    Glitch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LiveScroll {
    Tail,
    Manual,
    #[default]
    None,
    Prepend,
}

/// Kind of panel state transition animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    Cut,
    Fade,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    DrawDown,
    DrawRight,
    DrawOut,
    Dissolve,
}

// ─── Element Accessors ──────────────────────────────────────

const EMPTY_CHILDREN: &[Element] = &[];

fn option_string_capacity(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::capacity)
}

fn trigger_capacity(trigger: &Option<TriggerRef>) -> usize {
    trigger.as_ref().map_or(0, |trigger| {
        trigger
            .panel_id
            .capacity()
            .saturating_add(trigger.state_name.capacity())
    })
}

fn triggers_capacity(triggers: &TriggerAttrs) -> usize {
    trigger_capacity(&triggers.trigger_focus)
        .saturating_add(trigger_capacity(&triggers.trigger_blur))
        .saturating_add(trigger_capacity(&triggers.trigger_hover))
        .saturating_add(trigger_capacity(&triggers.trigger_unhover))
}

impl Document {
    /// Exact sum of the allocated capacities of every remotely supplied
    /// string retained by this AST. Container/vector storage is accounted by
    /// the owning client structures; this value is specifically the string
    /// component required by the shared remote-memory governor.
    pub fn retained_string_capacity(&self) -> usize {
        let page = &self.page;
        let metadata = page.meta.iter().fold(0usize, |total, entry| {
            total
                .saturating_add(entry.key.capacity())
                .saturating_add(entry.value.capacity())
        });
        option_string_capacity(&page.title)
            .saturating_add(metadata)
            .saturating_add(elements_string_capacity(&page.children))
    }
}

fn elements_string_capacity(elements: &[Element]) -> usize {
    elements.iter().fold(0usize, |total, element| {
        total.saturating_add(element.retained_string_capacity())
    })
}

impl Element {
    /// Exact allocated string capacity retained by this element and all of
    /// its descendants.
    pub fn retained_string_capacity(&self) -> usize {
        let own = match self {
            Element::Box(element) => option_string_capacity(&element.title),
            Element::Text(element) => element.content.capacity(),
            Element::Pre(element) => element.content.capacity(),
            Element::Heading(element) => element.content.capacity(),
            Element::Link(element) => option_string_capacity(&element.id)
                .saturating_add(element.href.capacity())
                .saturating_add(option_string_capacity(&element.defer_animation))
                .saturating_add(triggers_capacity(&element.triggers)),
            Element::Input(element) => option_string_capacity(&element.id)
                .saturating_add(element.name.capacity())
                .saturating_add(option_string_capacity(&element.placeholder))
                .saturating_add(option_string_capacity(&element.value))
                .saturating_add(triggers_capacity(&element.triggers)),
            Element::Select(element) => element
                .name
                .capacity()
                .saturating_add(option_string_capacity(&element.label)),
            Element::Option(element) => element
                .value
                .capacity()
                .saturating_add(element.label.capacity()),
            Element::Button(element) => option_string_capacity(&element.target)
                .saturating_add(option_string_capacity(&element.href))
                .saturating_add(element.label.capacity())
                .saturating_add(element.states.as_ref().map_or(0, |states| {
                    states.iter().fold(0usize, |total, state| {
                        total.saturating_add(state.capacity())
                    })
                }))
                .saturating_add(option_string_capacity(&element.to))
                .saturating_add(triggers_capacity(&element.triggers)),
            Element::Form(element) => element.action.capacity(),
            Element::Panel(element) => element
                .id
                .capacity()
                .saturating_add(element.initial_state.capacity()),
            Element::State(element) => element
                .name
                .capacity()
                .saturating_add(option_string_capacity(&element.transition)),
            Element::Details(element) => element.summary.capacity(),
            Element::On(element) => option_string_capacity(&element.source)
                .saturating_add(element.target.capacity())
                .saturating_add(option_string_capacity(&element.to)),
            Element::Art(element) => option_string_capacity(&element.src)
                .saturating_add(option_string_capacity(&element.alt))
                .saturating_add(element.content.capacity()),
            Element::Animate(element) => element
                .id
                .capacity()
                .saturating_add(option_string_capacity(&element.after))
                .saturating_add(option_string_capacity(&element.src)),
            Element::ElementDef(element) => element
                .id
                .capacity()
                .saturating_add(element.content.capacity()),
            Element::Tween(element) => element.target.capacity(),
            Element::TextAnimate(element) => element.content.capacity(),
            Element::Live(element) => element
                .id
                .capacity()
                .saturating_add(element.endpoint.capacity()),
            _ => 0,
        };
        let descendants = match self {
            Element::Details(element) => elements_string_capacity(&element.summary_children)
                .saturating_add(elements_string_capacity(&element.children)),
            _ => elements_string_capacity(self.children()),
        };
        own.saturating_add(descendants)
    }

    /// Access the children of any element (immutable).
    ///
    /// Returns an empty slice for leaf elements (Hr, Spacer, Input, Option,
    /// Art, ElementDef, Tween, TextAnimate, Pre).
    pub fn children(&self) -> &[Element] {
        match self {
            Element::Box(e) => &e.children,
            Element::Row(e) => &e.children,
            Element::Col(e) => &e.children,
            Element::Header(e)
            | Element::Body(e)
            | Element::Footer(e)
            | Element::Thead(e)
            | Element::Tbody(e)
            | Element::Pagination(e) => &e.children,
            Element::Nav(e) => &e.children,
            Element::Text(e) => &e.children,
            Element::Heading(e) => &e.children,
            Element::List(e) => &e.children,
            Element::Item(e) => &e.children,
            Element::Link(e) => &e.children,
            Element::Select(e) => &e.children,
            Element::Form(e) => &e.children,
            Element::Table(e) => &e.children,
            Element::Tr(e) => &e.children,
            Element::Th(e) | Element::Td(e) => &e.children,
            Element::Animate(e) => &e.children,
            Element::Frame(e) => &e.children,
            Element::Live(e) => &e.children,
            Element::Panel(e) => &e.children,
            Element::State(e) => &e.children,
            Element::Details(e) => &e.children,
            _ => EMPTY_CHILDREN,
        }
    }

    /// Access the children of any element (mutable).
    ///
    /// Returns `None` for leaf elements.
    pub fn children_mut(&mut self) -> Option<&mut Vec<Element>> {
        match self {
            Element::Box(e) => Some(&mut e.children),
            Element::Row(e) => Some(&mut e.children),
            Element::Col(e) => Some(&mut e.children),
            Element::Header(e)
            | Element::Body(e)
            | Element::Footer(e)
            | Element::Thead(e)
            | Element::Tbody(e)
            | Element::Pagination(e) => Some(&mut e.children),
            Element::Nav(e) => Some(&mut e.children),
            Element::Text(e) => Some(&mut e.children),
            Element::Heading(e) => Some(&mut e.children),
            Element::List(e) => Some(&mut e.children),
            Element::Item(e) => Some(&mut e.children),
            Element::Link(e) => Some(&mut e.children),
            Element::Select(e) => Some(&mut e.children),
            Element::Form(e) => Some(&mut e.children),
            Element::Table(e) => Some(&mut e.children),
            Element::Tr(e) => Some(&mut e.children),
            Element::Th(e) | Element::Td(e) => Some(&mut e.children),
            Element::Animate(e) => Some(&mut e.children),
            Element::Frame(e) => Some(&mut e.children),
            Element::Live(e) => Some(&mut e.children),
            Element::Panel(e) => Some(&mut e.children),
            Element::State(e) => Some(&mut e.children),
            Element::Details(e) => Some(&mut e.children),
            _ => None,
        }
    }
}

// ─── Value Parsing Helpers ───────────────────────────────────

fn parse_value_error(parts: &[&str]) -> String {
    let Some(capacity) = parts
        .iter()
        .try_fold(0usize, |total, part| total.checked_add(part.len()))
    else {
        return String::new();
    };
    let mut message = String::new();
    if message.try_reserve_exact(capacity).is_err() {
        return message;
    }
    for part in parts {
        message.push_str(part);
    }
    message
}

fn parse_ascii_lower_value_error(prefix: &str, value: &str) -> String {
    let Some(capacity) = prefix.len().checked_add(value.len()) else {
        return String::new();
    };
    let mut message = String::new();
    if message.try_reserve_exact(capacity).is_err() {
        return message;
    }
    message.push_str(prefix);
    message.extend(
        value
            .chars()
            .map(|character| character.to_ascii_lowercase()),
    );
    message
}

/// Maximum explicit width accepted from AML.
pub const MAX_LAYOUT_COLUMNS: u16 = 512;
/// Maximum document coordinate accepted by the layout engine.
pub const MAX_LAYOUT_ROWS: u16 = 4_096;
/// Maximum height or width of one explicitly sized element.
pub const MAX_ELEMENT_DIMENSION: u16 = 2_048;

/// Parse a dimension value: integer, "fill", or "fit".
pub fn parse_dimension(s: &str) -> Result<Dimension, String> {
    if s.eq_ignore_ascii_case("fill") {
        Ok(Dimension::Fill)
    } else if s.eq_ignore_ascii_case("fit") {
        Ok(Dimension::Fit)
    } else {
        let n: u16 = s
            .parse()
            .map_err(|_| parse_value_error(&["invalid dimension: ", s]))?;
        if n > MAX_ELEMENT_DIMENSION {
            return Err(parse_value_error(&[
                "dimension exceeds maximum of 2048: ",
                s,
            ]));
        }
        Ok(Dimension::Fixed(n))
    }
}

/// Parse a border style value.
pub fn parse_border_style(s: &str) -> Result<BorderStyle, String> {
    if s.eq_ignore_ascii_case("none") {
        Ok(BorderStyle::None)
    } else if s.eq_ignore_ascii_case("single") {
        Ok(BorderStyle::Single)
    } else if s.eq_ignore_ascii_case("double") {
        Ok(BorderStyle::Double)
    } else if s.eq_ignore_ascii_case("rounded") {
        Ok(BorderStyle::Rounded)
    } else if s.eq_ignore_ascii_case("heavy") {
        Ok(BorderStyle::Heavy)
    } else if s.eq_ignore_ascii_case("ascii") {
        Ok(BorderStyle::Ascii)
    } else {
        Err(parse_value_error(&["unknown border style: ", s]))
    }
}

/// Parse an alignment value.
pub fn parse_alignment(s: &str) -> Result<Alignment, String> {
    if s.eq_ignore_ascii_case("left") {
        Ok(Alignment::Left)
    } else if s.eq_ignore_ascii_case("center") {
        Ok(Alignment::Center)
    } else if s.eq_ignore_ascii_case("right") {
        Ok(Alignment::Right)
    } else {
        Err(parse_value_error(&["unknown alignment: ", s]))
    }
}

/// Parse a vertical alignment value.
pub fn parse_vertical_alignment(s: &str) -> Result<VerticalAlignment, String> {
    if s.eq_ignore_ascii_case("top") {
        Ok(VerticalAlignment::Top)
    } else if s.eq_ignore_ascii_case("middle") {
        Ok(VerticalAlignment::Middle)
    } else if s.eq_ignore_ascii_case("bottom") {
        Ok(VerticalAlignment::Bottom)
    } else {
        Err(parse_value_error(&["unknown vertical alignment: ", s]))
    }
}

/// Parse a sticky position value.
pub fn parse_sticky(s: &str) -> Result<StickyPosition, String> {
    if s.eq_ignore_ascii_case("top") {
        Ok(StickyPosition::Top)
    } else if s.eq_ignore_ascii_case("bottom") {
        Ok(StickyPosition::Bottom)
    } else {
        Err(parse_value_error(&["unknown sticky position: ", s]))
    }
}

/// Parse an HR style value.
pub fn parse_hr_style(s: &str) -> Result<HrStyle, String> {
    if s.eq_ignore_ascii_case("single") {
        Ok(HrStyle::Single)
    } else if s.eq_ignore_ascii_case("double") {
        Ok(HrStyle::Double)
    } else if s.eq_ignore_ascii_case("heavy") {
        Ok(HrStyle::Heavy)
    } else if s.eq_ignore_ascii_case("dash") {
        Ok(HrStyle::Dash)
    } else if s.eq_ignore_ascii_case("dot") {
        Ok(HrStyle::Dot)
    } else if s.eq_ignore_ascii_case("ascii") {
        Ok(HrStyle::Ascii)
    } else {
        Err(parse_value_error(&["unknown hr style: ", s]))
    }
}

/// Parse a list style value.
pub fn parse_list_style(s: &str) -> Result<ListStyle, String> {
    if s.eq_ignore_ascii_case("bullet") {
        Ok(ListStyle::Bullet)
    } else if s.eq_ignore_ascii_case("number") {
        Ok(ListStyle::Number)
    } else if s.eq_ignore_ascii_case("dash") {
        Ok(ListStyle::Dash)
    } else if s.eq_ignore_ascii_case("arrow") {
        Ok(ListStyle::Arrow)
    } else if s.eq_ignore_ascii_case("none") {
        Ok(ListStyle::None)
    } else {
        Err(parse_value_error(&["unknown list style: ", s]))
    }
}

/// Parse a button action value.
pub fn parse_button_action(s: &str) -> Result<ButtonAction, String> {
    if s.eq_ignore_ascii_case("submit") {
        Ok(ButtonAction::Submit)
    } else if s.eq_ignore_ascii_case("navigate") {
        Ok(ButtonAction::Navigate)
    } else if s.eq_ignore_ascii_case("toggle") {
        Ok(ButtonAction::Toggle)
    } else if s.eq_ignore_ascii_case("set") {
        Ok(ButtonAction::Set)
    } else {
        Err(parse_value_error(&["unknown button action: ", s]))
    }
}

/// Parse an art encoding value.
pub fn parse_art_encoding(s: &str) -> Result<ArtEncoding, String> {
    if s.eq_ignore_ascii_case("utf8") || s.eq_ignore_ascii_case("utf-8") {
        Ok(ArtEncoding::Utf8)
    } else if s.eq_ignore_ascii_case("cp437") {
        Ok(ArtEncoding::Cp437)
    } else if s.eq_ignore_ascii_case("petscii") {
        Ok(ArtEncoding::Petscii)
    } else {
        Err(parse_value_error(&["unknown encoding: ", s]))
    }
}

/// Parse a loop behavior value.
pub fn parse_loop(s: &str) -> Result<LoopBehavior, String> {
    if s.eq_ignore_ascii_case("false")
        || s.eq_ignore_ascii_case("no")
        || s.eq_ignore_ascii_case("none")
    {
        Ok(LoopBehavior::None)
    } else if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes") {
        Ok(LoopBehavior::Infinite)
    } else if s.eq_ignore_ascii_case("bounce") {
        Ok(LoopBehavior::Bounce)
    } else {
        let n: u32 = s
            .parse()
            .map_err(|_| parse_value_error(&["invalid loop value: ", s]))?;
        Ok(LoopBehavior::Count(n))
    }
}

/// Parse an easing function value.
pub fn parse_easing(s: &str) -> Result<Easing, String> {
    if s.eq_ignore_ascii_case("linear") {
        Ok(Easing::Linear)
    } else if s.eq_ignore_ascii_case("ease-in") || s.eq_ignore_ascii_case("easein") {
        Ok(Easing::EaseIn)
    } else if s.eq_ignore_ascii_case("ease-out") || s.eq_ignore_ascii_case("easeout") {
        Ok(Easing::EaseOut)
    } else if s.eq_ignore_ascii_case("ease-in-out") || s.eq_ignore_ascii_case("easeinout") {
        Ok(Easing::EaseInOut)
    } else if s.eq_ignore_ascii_case("step") {
        Ok(Easing::Step)
    } else {
        Err(parse_value_error(&["unknown easing: ", s]))
    }
}

/// Parse a text effect value.
pub fn parse_text_effect(s: &str) -> Result<TextEffect, String> {
    if s.eq_ignore_ascii_case("typewriter") {
        Ok(TextEffect::Typewriter)
    } else if s.eq_ignore_ascii_case("reveal") {
        Ok(TextEffect::Reveal)
    } else if s.eq_ignore_ascii_case("scramble") {
        Ok(TextEffect::Scramble)
    } else if s.eq_ignore_ascii_case("fade-in") || s.eq_ignore_ascii_case("fadein") {
        Ok(TextEffect::FadeIn)
    } else if s.eq_ignore_ascii_case("glitch") {
        Ok(TextEffect::Glitch)
    } else {
        Err(parse_value_error(&["unknown text effect: ", s]))
    }
}

/// Parse a live scroll value.
pub fn parse_live_scroll(s: &str) -> Result<LiveScroll, String> {
    if s.eq_ignore_ascii_case("tail") {
        Ok(LiveScroll::Tail)
    } else if s.eq_ignore_ascii_case("manual") {
        Ok(LiveScroll::Manual)
    } else if s.eq_ignore_ascii_case("none") {
        Ok(LiveScroll::None)
    } else if s.eq_ignore_ascii_case("prepend") {
        Ok(LiveScroll::Prepend)
    } else {
        Err(parse_value_error(&["unknown scroll mode: ", s]))
    }
}

/// Parse a duration string (e.g., "500ms", "2s", "1.5s") to milliseconds.
pub fn parse_duration_ms(s: &str) -> Result<u32, String> {
    let s = s.trim();

    let bytes = s.as_bytes();
    // Suffix positions are taken with `checked_sub`/`get` rather than by
    // indexing: the suffix bytes are ASCII, so the split points are char
    // boundaries and the slices cannot fail, but that is an argument about
    // the data rather than something the compiler checks.
    if let Some(head) = bytes.len().checked_sub(2)
        && bytes
            .get(head)
            .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'm'))
        && bytes
            .last()
            .is_some_and(|byte| byte.eq_ignore_ascii_case(&b's'))
    {
        s.get(..head)
            .unwrap_or(s)
            .trim()
            .parse::<u32>()
            .map_err(|_| parse_ascii_lower_value_error("invalid duration: ", s))
    } else if bytes
        .last()
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(&b's'))
    {
        let secs: f64 = s
            .get(..s.len() - 1)
            .unwrap_or(s)
            .trim()
            .parse()
            .map_err(|_| parse_ascii_lower_value_error("invalid duration: ", s))?;
        Ok((secs * 1000.0) as u32)
    } else {
        // Try as bare milliseconds
        s.parse::<u32>()
            .map_err(|_| parse_ascii_lower_value_error("invalid duration: ", s))
    }
}

/// Parse a direction value.
pub fn parse_direction(s: &str) -> Result<Direction, String> {
    if s.eq_ignore_ascii_case("left") {
        Ok(Direction::Left)
    } else if s.eq_ignore_ascii_case("right") {
        Ok(Direction::Right)
    } else if s.eq_ignore_ascii_case("up") {
        Ok(Direction::Up)
    } else if s.eq_ignore_ascii_case("down") {
        Ok(Direction::Down)
    } else {
        Err(parse_value_error(&["unknown direction: ", s]))
    }
}

/// Parse a trigger reference: "panel-id:state-name".
pub fn parse_trigger_ref(s: &str) -> Result<TriggerRef, String> {
    match s.split_once(':') {
        Some((panel_id, state_name)) => {
            let panel_id = panel_id.trim();
            let state_name = state_name.trim();
            if panel_id.is_empty() || state_name.is_empty() {
                Err(parse_value_error(&[
                    "invalid trigger ref: ",
                    s,
                    " (expected panel-id:state-name)",
                ]))
            } else {
                let mut owned_panel_id = String::new();
                owned_panel_id
                    .try_reserve_exact(panel_id.len())
                    .map_err(|_| String::new())?;
                owned_panel_id.push_str(panel_id);
                let mut owned_state_name = String::new();
                owned_state_name
                    .try_reserve_exact(state_name.len())
                    .map_err(|_| String::new())?;
                owned_state_name.push_str(state_name);
                Ok(TriggerRef {
                    panel_id: owned_panel_id,
                    state_name: owned_state_name,
                })
            }
        }
        None => Err(parse_value_error(&[
            "invalid trigger ref: ",
            s,
            " (expected panel-id:state-name)",
        ])),
    }
}

pub fn parse_transition_kind(s: &str) -> Option<TransitionKind> {
    if s.eq_ignore_ascii_case("cut") {
        Some(TransitionKind::Cut)
    } else if s.eq_ignore_ascii_case("fade") {
        Some(TransitionKind::Fade)
    } else if s.eq_ignore_ascii_case("slide-left") || s.eq_ignore_ascii_case("slideleft") {
        Some(TransitionKind::SlideLeft)
    } else if s.eq_ignore_ascii_case("slide-right") || s.eq_ignore_ascii_case("slideright") {
        Some(TransitionKind::SlideRight)
    } else if s.eq_ignore_ascii_case("slide-up") || s.eq_ignore_ascii_case("slideup") {
        Some(TransitionKind::SlideUp)
    } else if s.eq_ignore_ascii_case("slide-down") || s.eq_ignore_ascii_case("slidedown") {
        Some(TransitionKind::SlideDown)
    } else if s.eq_ignore_ascii_case("draw-down") || s.eq_ignore_ascii_case("drawdown") {
        Some(TransitionKind::DrawDown)
    } else if s.eq_ignore_ascii_case("draw-right") || s.eq_ignore_ascii_case("drawright") {
        Some(TransitionKind::DrawRight)
    } else if s.eq_ignore_ascii_case("draw-out") || s.eq_ignore_ascii_case("drawout") {
        Some(TransitionKind::DrawOut)
    } else if s.eq_ignore_ascii_case("dissolve") {
        Some(TransitionKind::Dissolve)
    } else {
        None
    }
}

/// Parse an event kind string.
pub fn parse_event_kind(s: &str) -> Result<EventKind, String> {
    if s.eq_ignore_ascii_case("focus") {
        Ok(EventKind::Focus)
    } else if s.eq_ignore_ascii_case("blur") {
        Ok(EventKind::Blur)
    } else if s.eq_ignore_ascii_case("state-change") || s.eq_ignore_ascii_case("statechange") {
        Ok(EventKind::StateChange)
    } else if s.eq_ignore_ascii_case("page-load") || s.eq_ignore_ascii_case("pageload") {
        Ok(EventKind::PageLoad)
    } else if s.eq_ignore_ascii_case("scroll-into-view") || s.eq_ignore_ascii_case("scrollintoview")
    {
        Ok(EventKind::ScrollIntoView)
    } else if s.eq_ignore_ascii_case("animation-end") || s.eq_ignore_ascii_case("animationend") {
        Ok(EventKind::AnimationEnd)
    } else if s.eq_ignore_ascii_case("select") {
        Ok(EventKind::Select)
    } else {
        Err(parse_value_error(&["unknown event kind: ", s]))
    }
}

/// Parse an action kind string.
pub fn parse_action_kind(s: &str) -> Result<ActionKind, String> {
    if s.eq_ignore_ascii_case("animate") {
        Ok(ActionKind::Animate)
    } else if s.eq_ignore_ascii_case("set") {
        Ok(ActionKind::Set)
    } else if s.eq_ignore_ascii_case("toggle") {
        Ok(ActionKind::Toggle)
    } else if s.eq_ignore_ascii_case("stop") {
        Ok(ActionKind::Stop)
    } else {
        Err(parse_value_error(&["unknown action kind: ", s]))
    }
}

// ─── AST Value Parsing Tests ─────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dimension() {
        assert_eq!(parse_dimension("40"), Ok(Dimension::Fixed(40)));
        assert_eq!(parse_dimension("fill"), Ok(Dimension::Fill));
        assert_eq!(parse_dimension("Fill"), Ok(Dimension::Fill));
        assert_eq!(parse_dimension("fit"), Ok(Dimension::Fit));
        assert!(parse_dimension("abc").is_err());
        assert!(parse_dimension(&(MAX_ELEMENT_DIMENSION + 1).to_string()).is_err());
    }

    #[test]
    fn test_parse_border_style() {
        assert_eq!(parse_border_style("single"), Ok(BorderStyle::Single));
        assert_eq!(parse_border_style("Double"), Ok(BorderStyle::Double));
        assert_eq!(parse_border_style("rounded"), Ok(BorderStyle::Rounded));
        assert_eq!(parse_border_style("heavy"), Ok(BorderStyle::Heavy));
        assert_eq!(parse_border_style("ascii"), Ok(BorderStyle::Ascii));
        assert_eq!(parse_border_style("none"), Ok(BorderStyle::None));
        assert!(parse_border_style("fancy").is_err());
    }

    #[test]
    fn test_parse_alignment() {
        assert_eq!(parse_alignment("left"), Ok(Alignment::Left));
        assert_eq!(parse_alignment("Center"), Ok(Alignment::Center));
        assert_eq!(parse_alignment("RIGHT"), Ok(Alignment::Right));
        assert!(parse_alignment("justify").is_err());
    }

    #[test]
    fn test_parse_loop() {
        assert_eq!(parse_loop("false"), Ok(LoopBehavior::None));
        assert_eq!(parse_loop("true"), Ok(LoopBehavior::Infinite));
        assert_eq!(parse_loop("bounce"), Ok(LoopBehavior::Bounce));
        assert_eq!(parse_loop("5"), Ok(LoopBehavior::Count(5)));
        assert!(parse_loop("abc").is_err());
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration_ms("500ms"), Ok(500));
        assert_eq!(parse_duration_ms("2s"), Ok(2000));
        assert_eq!(parse_duration_ms("1.5s"), Ok(1500));
        assert_eq!(parse_duration_ms("300"), Ok(300));
        assert!(parse_duration_ms("abc").is_err());
    }

    #[test]
    fn test_parse_hr_style() {
        assert_eq!(parse_hr_style("dash"), Ok(HrStyle::Dash));
        assert_eq!(parse_hr_style("dot"), Ok(HrStyle::Dot));
        assert_eq!(parse_hr_style("Double"), Ok(HrStyle::Double));
    }

    #[test]
    fn test_parse_list_style() {
        assert_eq!(parse_list_style("bullet"), Ok(ListStyle::Bullet));
        assert_eq!(parse_list_style("number"), Ok(ListStyle::Number));
        assert_eq!(parse_list_style("arrow"), Ok(ListStyle::Arrow));
    }

    #[test]
    fn test_parse_easing() {
        assert_eq!(parse_easing("linear"), Ok(Easing::Linear));
        assert_eq!(parse_easing("ease-in"), Ok(Easing::EaseIn));
        assert_eq!(parse_easing("ease-in-out"), Ok(Easing::EaseInOut));
    }

    #[test]
    fn test_parse_text_effect() {
        assert_eq!(parse_text_effect("typewriter"), Ok(TextEffect::Typewriter));
        assert_eq!(parse_text_effect("glitch"), Ok(TextEffect::Glitch));
        assert_eq!(parse_text_effect("fade-in"), Ok(TextEffect::FadeIn));
    }

    #[test]
    fn test_parse_art_encoding() {
        assert_eq!(parse_art_encoding("utf8"), Ok(ArtEncoding::Utf8));
        assert_eq!(parse_art_encoding("utf-8"), Ok(ArtEncoding::Utf8));
        assert_eq!(parse_art_encoding("cp437"), Ok(ArtEncoding::Cp437));
    }

    #[test]
    fn test_parse_live_scroll() {
        assert_eq!(parse_live_scroll("tail"), Ok(LiveScroll::Tail));
        assert_eq!(parse_live_scroll("manual"), Ok(LiveScroll::Manual));
        assert_eq!(parse_live_scroll("none"), Ok(LiveScroll::None));
        assert_eq!(parse_live_scroll("prepend"), Ok(LiveScroll::Prepend));
    }

    #[test]
    fn test_parse_direction() {
        assert_eq!(parse_direction("left"), Ok(Direction::Left));
        assert_eq!(parse_direction("Right"), Ok(Direction::Right));
    }

    #[test]
    fn test_parse_sticky() {
        assert_eq!(parse_sticky("top"), Ok(StickyPosition::Top));
        assert_eq!(parse_sticky("bottom"), Ok(StickyPosition::Bottom));
    }

    #[test]
    fn test_parse_button_action() {
        assert_eq!(parse_button_action("submit"), Ok(ButtonAction::Submit));
        assert_eq!(parse_button_action("navigate"), Ok(ButtonAction::Navigate));
        assert_eq!(parse_button_action("toggle"), Ok(ButtonAction::Toggle));
        assert_eq!(parse_button_action("set"), Ok(ButtonAction::Set));
    }

    #[test]
    fn test_parse_trigger_ref() {
        assert_eq!(
            parse_trigger_ref("drawer:open"),
            Ok(TriggerRef {
                panel_id: "drawer".into(),
                state_name: "open".into(),
            })
        );
        assert_eq!(
            parse_trigger_ref("my-panel:collapsed"),
            Ok(TriggerRef {
                panel_id: "my-panel".into(),
                state_name: "collapsed".into(),
            })
        );
        assert!(parse_trigger_ref("no-colon").is_err());
        assert!(parse_trigger_ref(":empty-id").is_err());
        assert!(parse_trigger_ref("empty-state:").is_err());
    }

    #[test]
    fn test_parse_vertical_alignment() {
        assert_eq!(parse_vertical_alignment("top"), Ok(VerticalAlignment::Top));
        assert_eq!(
            parse_vertical_alignment("middle"),
            Ok(VerticalAlignment::Middle)
        );
        assert_eq!(
            parse_vertical_alignment("bottom"),
            Ok(VerticalAlignment::Bottom)
        );
    }

    #[test]
    fn test_parse_event_kind() {
        assert_eq!(parse_event_kind("focus"), Ok(EventKind::Focus));
        assert_eq!(parse_event_kind("blur"), Ok(EventKind::Blur));
        assert_eq!(parse_event_kind("state-change"), Ok(EventKind::StateChange));
        assert_eq!(parse_event_kind("statechange"), Ok(EventKind::StateChange));
        assert_eq!(parse_event_kind("page-load"), Ok(EventKind::PageLoad));
        assert_eq!(
            parse_event_kind("animation-end"),
            Ok(EventKind::AnimationEnd)
        );
        assert_eq!(
            parse_event_kind("scroll-into-view"),
            Ok(EventKind::ScrollIntoView)
        );
        assert_eq!(parse_event_kind("select"), Ok(EventKind::Select));
        assert!(parse_event_kind("unknown").is_err());
    }

    #[test]
    fn test_parse_action_kind() {
        assert_eq!(parse_action_kind("animate"), Ok(ActionKind::Animate));
        assert_eq!(parse_action_kind("set"), Ok(ActionKind::Set));
        assert_eq!(parse_action_kind("toggle"), Ok(ActionKind::Toggle));
        assert_eq!(parse_action_kind("stop"), Ok(ActionKind::Stop));
        assert!(parse_action_kind("unknown").is_err());
    }

    #[test]
    fn test_parse_transition_kind() {
        assert_eq!(parse_transition_kind("fade"), Some(TransitionKind::Fade));
        assert_eq!(parse_transition_kind("Fade"), Some(TransitionKind::Fade));
        assert_eq!(parse_transition_kind("cut"), Some(TransitionKind::Cut));
        assert_eq!(
            parse_transition_kind("slide-left"),
            Some(TransitionKind::SlideLeft)
        );
        assert_eq!(
            parse_transition_kind("slideright"),
            Some(TransitionKind::SlideRight)
        );
        assert_eq!(
            parse_transition_kind("slide-up"),
            Some(TransitionKind::SlideUp)
        );
        assert_eq!(
            parse_transition_kind("slidedown"),
            Some(TransitionKind::SlideDown)
        );
        assert_eq!(
            parse_transition_kind("draw-down"),
            Some(TransitionKind::DrawDown)
        );
        assert_eq!(
            parse_transition_kind("draw-right"),
            Some(TransitionKind::DrawRight)
        );
        assert_eq!(
            parse_transition_kind("draw-out"),
            Some(TransitionKind::DrawOut)
        );
        assert_eq!(
            parse_transition_kind("dissolve"),
            Some(TransitionKind::Dissolve)
        );
        assert_eq!(parse_transition_kind("unknown"), None);
    }
}
