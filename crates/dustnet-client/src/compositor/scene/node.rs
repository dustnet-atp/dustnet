//! The `Node` type: one struct for every scene element, with all capability
//! fields present. Per the compositor.md decision, `Node` is **monomorphic**
//! — not sparse / ECS — because the capability set is bounded and the
//! debugging/auditing benefits outweigh the few bytes of unused `Option`s.

use slotmap::new_key_type;

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::engine::Placement;
use crate::parser::ast::{Alignment, Dimension, LiveScroll, TransitionKind};

new_key_type! {
    /// Stable, tombstone-safe identifier for a node.
    ///
    /// `NodeId`s remain valid across scene mutations until the node is
    /// removed; a removed id does not collide with a later-inserted node.
    pub struct NodeId;
}

/// Generic scene-level properties of a node. Capability-specific data lives
/// inside `NodeKind`. `Option<T>` fields are genuinely optional; unset
/// semantics is "this capability is absent," not "unknown."
///
/// Field visibility is `pub(super)` so that only code inside the `scene`
/// module can mutate — this enforces Invariant 2a from
/// `compositor.md` compiler-side: external writes route through
/// `PatchApplier` or the kind-gated `*_buffer_mut` accessors.
#[derive(Debug)]
pub struct Node {
    pub(super) id: NodeId,
    pub(super) kind: NodeKind,
    pub(super) parent: Option<NodeId>,
    pub(super) children: Vec<NodeId>,

    // Placement is the first-class output of layout. Defaults to
    // an empty placement at (0, 0); `build_scene` does not run
    // layout. Populated by `layout_scene` as it walks the tree.
    pub(super) placement: Placement,

    /// Buffer-absolute rect of the focusable surface for this node.
    /// Differs from `placement.rect` for two cases:
    /// - Inline focusables (Link/Button inside a wrapped Text) — the
    ///   focusable surface is the inline span, not the enclosing
    ///   text block.
    /// - Focusable Flow nodes (Details summary line) — the focusable
    ///   surface is the summary row, not the full open details area.
    ///
    /// `None` for non-focusable nodes and before layout runs.
    pub(super) focusable_screen_rect: Option<Rect>,

    // Node-owned buffer. Populated for `Panel`, `Animation`, and
    // `LiveRegion` nodes by `hydrate_scene_buffers`. `None` for
    // every other kind (containers compose into the page buffer).
    pub(super) buffer: Option<CellBuffer>,

    pub(super) z_index: i16,
    pub(super) transform: Transform,
    pub(super) visible: bool,
    pub(super) focusable: bool,
    pub(super) hit_target: Option<Action>,

    /// The `id=` attribute from AML, if present. Stable across navigation
    /// within a single scene; replaced when the scene is rebuilt.
    pub(super) aml_id: Option<String>,
}

impl Node {
    // Public read-only accessors. Mutation happens only via the
    // scene's `PatchApplier` or its kind-gated `*_buffer_mut`
    // accessors.

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// Buffer-absolute rect of the focusable surface for this node, if any.
    /// Set by the layout pass for focusable nodes (Buttons, Links, Inputs,
    /// Selects, Details summary lines). `None` before layout runs and for
    /// non-focusable nodes.
    pub fn focusable_screen_rect(&self) -> Option<Rect> {
        self.focusable_screen_rect
    }

    pub fn buffer(&self) -> Option<&CellBuffer> {
        self.buffer.as_ref()
    }

    pub fn z_index(&self) -> i16 {
        self.z_index
    }

    pub fn transform(&self) -> Transform {
        self.transform
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn focusable(&self) -> bool {
        self.focusable
    }

    pub fn hit_target(&self) -> Option<&Action> {
        self.hit_target.as_ref()
    }

    pub fn aml_id(&self) -> Option<&str> {
        self.aml_id.as_deref()
    }

    pub(crate) fn aml_id_capacity(&self) -> usize {
        option_string_capacity(&self.aml_id)
    }

    /// Short kind tag for debug output and parity assertions. Unlike a full
    /// `NodeKind` this is comparable without caring about inner data.
    pub fn kind_tag(&self) -> KindTag {
        self.kind.tag()
    }

    /// Remotely influenced payload this node retains: its strings, plus the
    /// capacity of the vectors holding them. The name predates the vector
    /// spine being counted; `Action` and `PlacedElement` keep the
    /// string-only meaning.
    pub(crate) fn retained_string_capacity(&self) -> usize {
        option_string_capacity(&self.aml_id)
            .saturating_add(self.kind.retained_string_capacity())
            .saturating_add(
                self.hit_target
                    .as_ref()
                    .map_or(0, Action::retained_string_capacity),
            )
    }
}

fn option_string_capacity(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::capacity)
}

fn string_vec_capacity(values: &[String]) -> usize {
    values.iter().fold(0usize, |total, value| {
        total.saturating_add(value.capacity())
    })
}

/// Kind-specific data. Lives inside the enum rather than as optional fields
/// on `Node` — `NodeKind::Row` has no opinions about panel state, and
/// `NodeKind::Panel` would have no sensible default for a `Row`'s alignment.
#[derive(Debug, Clone)]
pub enum NodeKind {
    /// Scene root. Exactly one per `Scene`.
    Root,

    /// Vertical flow container. Children stack top to bottom. Maps from AML
    /// `Box` (when no `x`/`y`), `Col`, `Header`, `Body`, `Footer`, `Nav`,
    /// `Thead`, `Tbody`, `Pagination`, `List`, `Item`, `Frame`, `Details`,
    /// `State`, `Form`.
    Flow(FlowData),

    /// Horizontal flow container (`Row`). Carries the inter-column gap
    /// in cells; columns themselves are child `Flow` nodes whose
    /// `FlowData.width` drives the column-width allocation.
    Row(RowData),

    /// Self-placing container (`Box` with `x` or `y` set).
    Absolute(AbsoluteData),

    /// Leaf or inline-root text node. Maps from `Text`, `Pre`, `Heading`,
    /// `Art`, `ElementDef`, `TextAnimate`.
    Text(TextContent),

    /// Interactive input. Carries placeholder/name/password/etc.
    Input(InputData),

    /// Dropdown select. Children are `OptionLeaf`.
    Select(SelectData),

    /// Option leaf (child of `Select`). Not a standalone node outside a
    /// `Select` subtree.
    OptionLeaf(OptionData),

    /// Button. Click-activated hit target — its `Action` lives on the
    /// outer `Node.hit_target`, not duplicated here.
    Button(ButtonData),

    /// Horizontal rule — draws a full-width divider.
    Hr(HrData),

    /// Blank vertical spacer — no cells rendered, just flow advance.
    Spacer { lines: u16 },

    /// Collection of panel states with the currently-active one.
    Panel {
        states: Vec<NodeId>,
        active: NodeId,
        initial_state: String,
    },

    /// Animation node: frame-based or WASM. Per-node buffer hydrated
    /// from layout; this variant records the AML data needed to
    /// instantiate the animation.
    Animation(AnimationData),

    /// Live region subscribed to a server endpoint.
    LiveRegion(LiveData),

    /// Table container (`Table`).
    Table,
    /// Table row.
    Tr,
    /// Table header cell.
    Th(CellData),
    /// Table body cell.
    Td(CellData),

    /// Link — the hit_target on the outer node holds the `Navigate`
    /// action; this variant carries the link-specific UI data.
    Link(LinkData),

    /// System-synthesized overlay node. Has no AML source; never produced
    /// by `build::from_document`. Created by `Scene::insert_overlay` for
    /// compositor-level effects (page transitions, future debug overlays,
    /// headless capture) that need to appear in the composite walk but
    /// don't correspond to any author markup.
    ///
    /// Overlay nodes are laid out *only* by their creator (buffer
    /// allocated eagerly at construction); the layout pass skips them.
    /// Hit-testing skips them (they don't capture clicks). The composite
    /// walk blits their buffer like any other buffered node, honoring
    /// `z_index`.
    Overlay(OverlayData),
}

/// Per-kind data for `NodeKind::Overlay`. Records what kind of overlay
/// this is — present for debuggability and for exhaustive matching in
/// consumers that care (e.g., a future debug dumper showing overlay
/// provenance).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayData {
    pub source: OverlaySource,
}

/// What synthesized this overlay. Extend as new compositor-level
/// effects land; each variant is its own branch rather than a free-form
/// string so consumers match exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySource {
    /// Page-to-page transition blend (old page → new page). Owned by a
    /// `PageTransitionAdapter` in the animation runtime; removed via
    /// `Patch::Remove` when the adapter finishes.
    PageTransition,
}

impl NodeKind {
    pub fn tag(&self) -> KindTag {
        match self {
            NodeKind::Root => KindTag::Root,
            NodeKind::Flow(_) => KindTag::Flow,
            NodeKind::Row(_) => KindTag::Row,
            NodeKind::Absolute(_) => KindTag::Absolute,
            NodeKind::Text(_) => KindTag::Text,
            NodeKind::Input(_) => KindTag::Input,
            NodeKind::Select(_) => KindTag::Select,
            NodeKind::OptionLeaf(_) => KindTag::OptionLeaf,
            NodeKind::Button(_) => KindTag::Button,
            NodeKind::Hr(_) => KindTag::Hr,
            NodeKind::Spacer { .. } => KindTag::Spacer,
            NodeKind::Panel { .. } => KindTag::Panel,
            NodeKind::Animation(_) => KindTag::Animation,
            NodeKind::LiveRegion(_) => KindTag::LiveRegion,
            NodeKind::Table => KindTag::Table,
            NodeKind::Tr => KindTag::Tr,
            NodeKind::Th(_) => KindTag::Th,
            NodeKind::Td(_) => KindTag::Td,
            NodeKind::Link(_) => KindTag::Link,
            NodeKind::Overlay(_) => KindTag::Overlay,
        }
    }

    fn retained_string_capacity(&self) -> usize {
        match self {
            NodeKind::Flow(data) => option_string_capacity(&data.title)
                .saturating_add(option_string_capacity(&data.form_action))
                .saturating_add(option_string_capacity(&data.state_name))
                .saturating_add(option_string_capacity(&data.details_summary)),
            NodeKind::Absolute(data) => option_string_capacity(&data.title),
            // The run vector's own spine counts too, not only the strings
            // inside it. Charging the strings alone left `runs.capacity() *
            // size_of::<TextRun>()` outside the admission — a `TextRun` is
            // far larger than the pointer it would be if this were a `Vec` of
            // handles, so a text-heavy page under-reported by more than the
            // text itself on nodes with many short runs.
            NodeKind::Text(data) => data
                .runs
                .capacity()
                .saturating_mul(std::mem::size_of::<TextRun>())
                .saturating_add(data.runs.iter().fold(0usize, |total, run| {
                    total.saturating_add(run.text.capacity())
                })),
            NodeKind::Input(data) => data
                .name
                .capacity()
                .saturating_add(option_string_capacity(&data.placeholder))
                .saturating_add(option_string_capacity(&data.value)),
            NodeKind::Select(data) => data
                .name
                .capacity()
                .saturating_add(option_string_capacity(&data.label)),
            NodeKind::OptionLeaf(data) => {
                data.value.capacity().saturating_add(data.label.capacity())
            }
            NodeKind::Button(data) => data
                .label
                .capacity()
                .saturating_add(option_string_capacity(&data.target))
                .saturating_add(data.states.as_deref().map_or(0, string_vec_capacity))
                .saturating_add(option_string_capacity(&data.to))
                .saturating_add(option_string_capacity(&data.href)),
            NodeKind::Panel { initial_state, .. } => initial_state.capacity(),
            NodeKind::Animation(data) => option_string_capacity(&data.src)
                .saturating_add(option_string_capacity(&data.after)),
            NodeKind::LiveRegion(data) => data.endpoint.capacity(),
            NodeKind::Link(data) => data
                .href
                .capacity()
                .saturating_add(option_string_capacity(&data.defer_animation)),
            _ => 0,
        }
    }
}

/// Cheap-to-compare discriminant for `NodeKind`. Used by the parity
/// assertion where only the discriminant matters, not the inner data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindTag {
    Root,
    Flow,
    Row,
    Absolute,
    Text,
    Input,
    Select,
    OptionLeaf,
    Button,
    Hr,
    Spacer,
    Panel,
    Animation,
    LiveRegion,
    Table,
    Tr,
    Th,
    Td,
    Link,
    Overlay,
}

/// Per-kind data for `NodeKind::Flow`. Carries every AST attribute
/// that affects layout, so the scene-native kind helpers in
/// `compositor::layout::kinds` can read from `Node` fields directly
/// without reaching back into the AST.
#[derive(Debug, Clone, Default)]
pub struct FlowData {
    /// Source element variant that mapped into `Flow`. Lets downstream
    /// consumers distinguish a `Col` from a `Box` from a `Form` when
    /// relevant, without introducing extra `NodeKind` variants.
    pub source: FlowSource,
    /// Box-style fields (ignored for most `FlowSource` variants).
    pub border: BorderStyleTag,
    pub title: Option<String>,
    pub join_top: Option<u16>,
    pub join_bottom: Option<u16>,
    pub join_left: Option<u16>,
    pub join_right: Option<u16>,
    pub padding: u16,
    pub width: Option<Dimension>,
    pub height: Option<Dimension>,
    /// Foreground color (Box/Col only). Pre-resolution; layout resolves
    /// against the terminal's color-support level.
    pub fg: Option<crate::color::Color>,
    /// Background color (Box/Col only).
    pub bg: Option<crate::color::Color>,
    /// Horizontal alignment (Box/Col only).
    pub align: Alignment,
    /// Sticky positioning (Box only) — pins to viewport top or bottom.
    pub sticky: Option<crate::parser::ast::StickyPosition>,
    /// Form action URL, for `FlowSource::Form`.
    pub form_action: Option<String>,
    /// List style (ordered/unordered/bullet char), for `FlowSource::List`.
    pub list_style: Option<crate::parser::ast::ListStyle>,
    pub list_bullet_char: Option<char>,
    /// `State`: the state name, used by the parent `Panel`'s state map.
    pub state_name: Option<String>,
    /// `State`: the transition kind to play when this state becomes
    /// active. Pre-parsed from the AST string; `None` means no
    /// transition (cut to new state).
    pub state_transition: Option<crate::parser::ast::TransitionKind>,
    /// `State`: the transition duration in milliseconds. Meaningful
    /// only if `state_transition` is `Some`.
    pub state_transition_duration_ms: u32,
    /// `Details`: number of leading scene children that belong to the
    /// summary line (rendered inline after the ▶/▼ indicator).
    /// Remaining children are the body, rendered indented if `open`.
    pub details_summary_count: u16,
    /// `Details`: plain-text summary shown when
    /// `details_summary_count == 0`.
    pub details_summary: Option<String>,
    /// `Details`: whether the body is currently visible.
    pub details_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlowSource {
    #[default]
    Box,
    Col,
    Header,
    Body,
    Footer,
    Nav,
    Thead,
    Tbody,
    Pagination,
    List,
    Item,
    Frame,
    Details,
    State,
    Form,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyleTag {
    #[default]
    None,
    Single,
    Double,
    Rounded,
    Heavy,
    Ascii,
}

impl BorderStyleTag {
    /// Round-trip to the AST's `BorderStyle`. Kept as a helper so
    /// scene-native layout helpers can call `draw_border` without
    /// duplicating the mapping table.
    pub fn to_ast(self) -> crate::parser::ast::BorderStyle {
        use crate::parser::ast::BorderStyle;
        match self {
            BorderStyleTag::None => BorderStyle::None,
            BorderStyleTag::Single => BorderStyle::Single,
            BorderStyleTag::Double => BorderStyle::Double,
            BorderStyleTag::Rounded => BorderStyle::Rounded,
            BorderStyleTag::Heavy => BorderStyle::Heavy,
            BorderStyleTag::Ascii => BorderStyle::Ascii,
        }
    }
}

/// Per-kind data for `NodeKind::Row` — the horizontal container.
#[derive(Debug, Clone, Default)]
pub struct RowData {
    pub gap: u16,
    pub align: crate::parser::ast::VerticalAlignment,
}

/// Per-kind data for an `Absolute` container (positioned `Box`).
#[derive(Debug, Clone)]
pub struct AbsoluteData {
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub w: Dimension,
    pub h: Dimension,
    pub border: BorderStyleTag,
    pub title: Option<String>,
    pub join_top: Option<u16>,
    pub join_bottom: Option<u16>,
    pub join_left: Option<u16>,
    pub join_right: Option<u16>,
    pub padding: u16,
    pub fg: Option<crate::color::Color>,
    pub bg: Option<crate::color::Color>,
    pub align: Alignment,
}

/// Text content: a sequence of styled runs. An inline element inside
/// `[text]` (e.g. `[b]hello[/b]`) flattens into a run here rather than
/// producing a child node, per the compositor.md isomorphism rule #7.
#[derive(Debug, Clone, Default)]
pub struct TextContent {
    pub runs: Vec<TextRun>,
    pub align: Alignment,
    pub source: TextSource,
}

/// Which AST element produced this `Text` node — useful for debug dumps
/// and for layout to know whether to preserve whitespace (`Pre`, `Art`)
/// versus word-wrap (`Text`, `Heading`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextSource {
    #[default]
    Text,
    Pre,
    Heading(u8),
    Art,
    ElementDef,
    TextAnimate,
}

#[derive(Debug, Clone, Default)]
pub struct TextRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub blink: bool,
    /// Foreground color as an unresolved AML color — the layout pass
    /// resolves to the terminal's capability when rendering.
    pub fg: Option<crate::color::Color>,
    pub bg: Option<crate::color::Color>,
}

#[derive(Debug, Clone)]
pub struct InputData {
    pub name: String,
    pub maxlen: u32,
    pub placeholder: Option<String>,
    pub multiline: bool,
    pub rows: u16,
    pub password: bool,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SelectData {
    pub name: String,
    pub label: Option<String>,
    pub selected_index: usize,
}

#[derive(Debug, Clone)]
pub struct OptionData {
    pub value: String,
    pub selected: bool,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct ButtonData {
    pub label: String,
    pub key: Option<char>,
    /// Declared button action — drives focusable dispatch. Must match
    /// the AST `ButtonElement.action` so the focus handler can route
    /// Enter to the right side effect.
    pub action: crate::parser::ast::ButtonAction,
    /// For `Toggle`/`Set` actions: the target panel's aml id.
    pub target: Option<String>,
    /// For `Toggle` actions: comma-separated list of state names.
    pub states: Option<Vec<String>>,
    /// For `Set` actions: the destination state name.
    pub to: Option<String>,
    /// For `Navigate` actions: the href (same as on Link).
    pub href: Option<String>,
    /// Transition overrides — see `LinkData`.
    pub transition: Option<crate::parser::ast::TransitionKind>,
    pub transition_duration_ms: u32,
}

#[derive(Debug, Clone)]
pub struct HrData {
    pub fg: Option<crate::color::Color>,
    pub style: crate::parser::ast::HrStyle,
}

#[derive(Debug, Clone)]
pub struct AnimationData {
    pub fps: u16,
    pub autoplay: bool,
    pub loop_behavior: crate::parser::ast::LoopBehavior,
    pub loop_: bool,
    pub background: bool,
    /// WASM source path, if this is a WASM animation. `None` for frame-based.
    pub src: Option<String>,
    /// Frame children (for frame-based animations). Empty for WASM.
    pub frames: Vec<NodeId>,
    pub delay_ms: u32,
    pub after: Option<String>,
    /// Explicit positioning — used by the `PlacedElement` bounds
    /// emitted when the animation occupies a sub-region rather
    /// than the full containing flow.
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub w: Option<u16>,
    pub h: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct LiveData {
    pub endpoint: String,
    pub height: Dimension,
    pub scroll: LiveScroll,
    pub buffer: u32,
    pub delta: bool,
}

#[derive(Debug, Clone)]
pub struct CellData {
    pub fg: Option<crate::color::Color>,
}

#[derive(Debug, Clone)]
pub struct LinkData {
    pub href: String,
    pub key: Option<char>,
    pub prefetch: bool,
    pub transition: Option<TransitionKind>,
    pub transition_duration_ms: u32,
    pub defer_animation: Option<String>,
}

/// Offset applied to a layer at composition time; distinct from `placement`
/// so animations (slides, wipes) can move a node without re-running layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transform {
    pub dx: i16,
    pub dy: i16,
}

impl Transform {
    pub const IDENTITY: Transform = Transform { dx: 0, dy: 0 };

    pub fn is_identity(self) -> bool {
        self.dx == 0 && self.dy == 0
    }
}

/// What happens when the node is activated (clicked, Enter, key press).
/// Concrete action vocabulary; replaces the scatter of `LinkElement.href`,
/// `ButtonElement.action`, and ad-hoc trigger bindings.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Navigate to a URI.
    Navigate {
        href: String,
        transition: Option<TransitionKind>,
        transition_duration_ms: u32,
        defer_animation: Option<String>,
    },
    /// Set a panel to a specific state.
    SetPanelState { panel: String, state: String },
    /// Toggle a panel through a list of states.
    TogglePanel { panel: String, states: Vec<String> },
    /// Submit a form.
    Submit { form: String },
    /// Fire a trigger event.
    FireTrigger {
        event: String,
        target: Option<String>,
    },
    /// Toggle a `[details]` element open/closed.
    ToggleDetails { id: String },
}

impl Action {
    fn retained_string_capacity(&self) -> usize {
        match self {
            Action::Navigate {
                href,
                defer_animation,
                ..
            } => href
                .capacity()
                .saturating_add(option_string_capacity(defer_animation)),
            Action::SetPanelState { panel, state } => {
                panel.capacity().saturating_add(state.capacity())
            }
            Action::TogglePanel { panel, states } => {
                panel.capacity().saturating_add(string_vec_capacity(states))
            }
            Action::Submit { form } => form.capacity(),
            Action::FireTrigger { event, target } => event
                .capacity()
                .saturating_add(option_string_capacity(target)),
            Action::ToggleDetails { id } => id.capacity(),
        }
    }
}

/// Builder used by `scene::build` to construct a `Node` with sensible
/// defaults. Field visibility (`pub(super)`) means external callers must
/// go through `scene::build` or (in later phases) `PatchApplier`.
pub(super) struct NodeBuilder {
    pub(super) kind: NodeKind,
    pub(super) aml_id: Option<String>,
    pub(super) focusable: bool,
    pub(super) hit_target: Option<Action>,
}

impl NodeBuilder {
    pub(super) fn new(kind: NodeKind) -> Self {
        Self {
            kind,
            aml_id: None,
            focusable: false,
            hit_target: None,
        }
    }

    pub(super) fn aml_id(mut self, id: impl Into<Option<String>>) -> Self {
        self.aml_id = id.into();
        self
    }

    pub(super) fn focusable(mut self, focusable: bool) -> Self {
        self.focusable = focusable;
        self
    }

    pub(super) fn hit_target(mut self, action: Option<Action>) -> Self {
        self.hit_target = action;
        self
    }

    pub(super) fn finish(self, id: NodeId) -> Node {
        Node {
            id,
            kind: self.kind,
            parent: None,
            children: Vec::new(),
            placement: Placement::empty_at(0, 0),
            focusable_screen_rect: None,
            buffer: None,
            z_index: 0,
            transform: Transform::IDENTITY,
            visible: true,
            focusable: self.focusable,
            hit_target: self.hit_target,
            aml_id: self.aml_id,
        }
    }
}
