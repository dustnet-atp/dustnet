//! Build a `Scene` from a parsed `Document`.
//!
//! The mapping is specified by the table in `docs/internals/compositor.md`; this file
//! implements it, and `parity.rs` asserts its correctness.
//!
//! Three element categories exist:
//!
//! - **Node-bearing**: produces exactly one scene node. Its node-bearing
//!   children are recursively mapped.
//! - **Ancillary**: produces zero nodes. Consumed elsewhere (e.g. `On`
//!   becomes an event binding attached to the scene, not a node).
//! - **Inline**: produces zero nodes. Its data becomes a styled run inside
//!   the containing `Text` node.
//!
//! `classify(e)` is exhaustive over `Element` — adding a new AST variant
//! is a compile error until it gets a mapping row.

use std::sync::Arc;

use crate::color::Color;
use crate::parser::ast::{
    self, Alignment, Dimension, Document, Element, PanelElement, TextElement,
};
use crate::resource::{ResourceCategory, ResourceGovernor};

use super::events::{EventBinding, EventBindings};
use super::node::{
    AbsoluteData, Action, AnimationData, BorderStyleTag, ButtonData, CellData, FlowData,
    FlowSource, HrData, InputData, LinkData, LiveData, NodeBuilder, NodeId, NodeKind, OptionData,
    SelectData, TextContent, TextRun, TextSource,
};
use super::tree::{AmlIdIndex, Scene, SceneNodes};

/// Category of an AST element in the mapping table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    NodeBearing,
    /// Element that contributes no node (e.g. `On` becomes an event
    /// binding, `Tween` contributes only animation data).
    Ancillary,
    /// Inline element — valid only as a child of a `Text`-family node.
    /// Outside that context it is either ancillary (ignored) or a bug.
    Inline,
}

/// Classify every `Element` variant. Exhaustive match — a new
/// variant added to the AST fails to compile here, which keeps the
/// mapping table complete.
pub fn classify(element: &Element) -> Category {
    match element {
        // Node-bearing layout containers
        Element::Box(_)
        | Element::Row(_)
        | Element::Col(_)
        | Element::Hr(_)
        | Element::Spacer(_)
        | Element::Header(_)
        | Element::Body(_)
        | Element::Footer(_)
        | Element::Nav(_)
        | Element::Thead(_)
        | Element::Tbody(_)
        | Element::Pagination(_)
        | Element::List(_)
        | Element::Item(_)
        | Element::Form(_) => Category::NodeBearing,

        // Text-family leaves & their near-kin
        Element::Text(_)
        | Element::Pre(_)
        | Element::Heading(_)
        | Element::Art(_)
        | Element::ElementDef(_)
        | Element::TextAnimate(_) => Category::NodeBearing,

        // Interactive
        Element::Link(_) | Element::Input(_) | Element::Select(_) | Element::Button(_) => {
            Category::NodeBearing
        }
        // `Option` is node-bearing only inside a `Select`; at the top level
        // it would be ancillary. We still classify it as node-bearing here
        // because the build sweep only visits Option beneath Select.
        Element::Option(_) => Category::NodeBearing,

        // Table family
        Element::Table(_) | Element::Tr(_) | Element::Th(_) | Element::Td(_) => {
            Category::NodeBearing
        }

        // Animation family
        Element::Animate(_) | Element::Frame(_) => Category::NodeBearing,

        // Live
        Element::Live(_) => Category::NodeBearing,

        // Panels
        Element::Panel(_) | Element::State(_) => Category::NodeBearing,

        // Collapsible
        Element::Details(_) => Category::NodeBearing,

        // Ancillary — no node produced.
        //
        // `Include` is ancillary because the server resolves it before sending:
        // one reaching a client means that origin has no handler for the name.
        // Rendering nothing is deliberate — the failure mode worth avoiding is
        // showing the reader the marker, which is what the `{{links}}` text
        // convention did when nothing expanded it.
        Element::Tween(_) | Element::On(_) | Element::Include(_) => Category::Ancillary,
    }
}

/// Build a `Scene` from a parsed document. Pure function; does not
/// run layout and does not produce per-node buffer content — buffers
/// are allocated afterward by `hydrate_scene_buffers` once layout
/// has produced rects. Subsequent mutations go through
/// `PatchApplier`.
pub fn from_document(doc: &Document) -> Scene {
    from_document_with_governor(doc, None)
}

/// Build a scene whose remotely influenced node buffers each retain their
/// own exact `SceneCells` lease. The governor is installed before layout or
/// hydration can allocate a buffer.
pub fn from_document_governed(doc: &Document, governor: &ResourceGovernor) -> Scene {
    from_document_with_governor(doc, Some(governor.clone()))
}

fn from_document_with_governor(doc: &Document, governor: Option<ResourceGovernor>) -> Scene {
    let element_bound = count_elements(&doc.page.children);
    let node_capacity = element_bound.and_then(|elements| elements.checked_add(2));
    let requested_node_bytes = node_capacity.and_then(SceneNodes::requested_bytes);
    let mut node_lease = match (governor.as_ref(), requested_node_bytes) {
        (Some(governor), Some(bytes)) => governor
            .reserve(ResourceCategory::RemoteCollections, bytes)
            .ok(),
        (None, Some(_)) => None,
        (_, None) => None,
    };
    let mut node_failed =
        requested_node_bytes.is_none() || governor.is_some() && node_lease.is_none();
    let mut nodes = if node_failed {
        SceneNodes::default()
    } else {
        match node_capacity.and_then(|capacity| SceneNodes::try_with_capacity(capacity).ok()) {
            Some(nodes) => nodes,
            None => {
                node_failed = true;
                SceneNodes::default()
            }
        }
    };
    if let (Some(lease), Some(actual)) = (node_lease.as_mut(), nodes.retained_bytes()) {
        node_failed |= lease.try_resize_with_cost(actual, actual).is_err();
    }
    if node_failed {
        nodes = SceneNodes::default();
        node_lease = None;
    }
    let requested_relation_bytes = element_bound
        .and_then(|elements| elements.checked_mul(2))
        .and_then(|slots| slots.checked_add(1))
        .and_then(|slots| slots.checked_mul(std::mem::size_of::<NodeId>()));
    let mut relation_lease = match (governor.as_ref(), requested_relation_bytes) {
        (Some(governor), Some(bytes)) => governor
            .reserve(ResourceCategory::RemoteCollections, bytes)
            .ok(),
        (None, Some(_)) => None,
        (_, None) => None,
    };
    let mut relation_failed = requested_relation_bytes.is_none()
        || governor.is_some() && relation_lease.is_none()
        || super::tree::reject_scene_allocation(super::tree::SceneAllocationSite::RelationTopology);
    let mut aml_id_index = AmlIdIndex::new();
    if !relation_failed
        && aml_id_index
            .try_reserve_exact(element_bound.unwrap_or(0))
            .is_err()
    {
        relation_failed = true;
    }

    let root_id = if node_failed {
        NodeId::default()
    } else {
        nodes
            .insert_with_key(|id| NodeBuilder::new(NodeKind::Root).finish(id))
            .unwrap_or_else(|| {
                node_failed = true;
                NodeId::default()
            })
    };

    let mut cx = BuildCtx {
        nodes: &mut nodes,
        aml_id_index: &mut aml_id_index,
        relation_failed: &mut relation_failed,
    };

    if !node_failed {
        for child in &doc.page.children {
            if let Some(child_id) = build_element(&mut cx, child) {
                attach_child(&mut cx, root_id, child_id);
            }
        }
    }

    if !relation_failed
        && nodes
            .get_mut(root_id)
            .is_none_or(|root| root.children.try_reserve_exact(1).is_err())
    {
        relation_failed = true;
    }

    let actual_relation_bytes = aml_id_index
        .capacity()
        .checked_add(
            nodes
                .values()
                .try_fold(0usize, |total, node| {
                    total.checked_add(node.children.capacity())
                })
                .unwrap_or(usize::MAX),
        )
        .and_then(|slots| slots.checked_mul(std::mem::size_of::<NodeId>()));
    if let (Some(lease), Some(actual)) = (relation_lease.as_mut(), actual_relation_bytes) {
        relation_failed |= lease.try_resize_with_cost(actual, actual).is_err();
    } else if actual_relation_bytes.is_none() {
        relation_failed = true;
    }
    if relation_failed {
        aml_id_index = Vec::new();
        for node in nodes.values_mut() {
            node.children = Vec::new();
            node.parent = None;
        }
        relation_lease = None;
    }

    let event_bindings = collect_event_bindings(&doc.page.children);

    let invalidation =
        super::invalidation::Invalidation::try_for_nodes(nodes.len(), governor.as_ref());
    let resource_error =
        node_failed || relation_failed || invalidation.is_none() || event_bindings.is_none();
    let event_bindings = event_bindings.unwrap_or_default();

    Scene {
        root: root_id,
        nodes,
        aml_id_index,
        invalidation: invalidation.unwrap_or_default(),
        focus: None,
        scroll: super::tree::ScrollState::default(),
        event_bindings,
        page_mode: doc.page.mode,
        default_fg: doc.page.style.as_ref().and_then(|s| s.default_fg.clone()),
        default_bg: doc.page.style.as_ref().and_then(|s| s.default_bg.clone()),
        transition: doc.page.transition,
        transition_duration_ms: doc.page.transition_duration_ms,
        title: doc.page.title.as_deref().map(Arc::from),
        resource_error,
        governor,
        relayout_journal: None,
        page_transition_overlay: None,
        _node_topology_lease: node_lease,
        _node_relation_topology_lease: relation_lease,
        layout_pass: 0,
    }
}

fn count_elements(elements: &[Element]) -> Option<usize> {
    elements.iter().try_fold(0usize, |total, element| {
        let descendants = count_elements(element.children())?;
        let summary = match element {
            Element::Details(details) => count_elements(&details.summary_children)?,
            _ => 0,
        };
        total
            .checked_add(1)?
            .checked_add(descendants)?
            .checked_add(summary)
    })
}

/// Walk the AST and collect every `[on]` binding. Called once at
/// `build_scene` time so the runtime `EventDispatcher` can source
/// bindings from the scene rather than from the layout result.
fn collect_event_bindings(elements: &[Element]) -> Option<EventBindings> {
    let mut out = EventBindings::new();
    walk_for_on(elements, &mut out).then_some(out)
}

fn walk_for_on(elements: &[Element], out: &mut EventBindings) -> bool {
    for elem in elements {
        if let Element::On(e) = elem {
            let Some(source) = try_clone_optional_string(&e.source) else {
                return false;
            };
            let Some(target) = try_clone_string(&e.target) else {
                return false;
            };
            let Some(to) = try_clone_optional_string(&e.to) else {
                return false;
            };
            if out
                .try_push(EventBinding {
                    event: e.event,
                    source,
                    action: e.action,
                    target,
                    to,
                    delay_ms: e.delay_ms,
                })
                .is_err()
            {
                return false;
            }
            continue;
        }
        let children = elem.children();
        if !children.is_empty() && !walk_for_on(children, out) {
            return false;
        }
    }
    true
}

fn try_clone_string(value: &str) -> Option<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).ok()?;
    owned.push_str(value);
    Some(owned)
}

fn try_clone_optional_string(value: &Option<String>) -> Option<Option<String>> {
    match value {
        Some(value) => try_clone_string(value).map(Some),
        None => Some(None),
    }
}

#[cfg(test)]
thread_local! {
    static AML_ID_COPY_REJECTION: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(test)]
pub(super) fn reject_next_aml_id_copy() {
    AML_ID_COPY_REJECTION.with(|site| site.set(true));
}

fn try_clone_aml_id(cx: &mut BuildCtx<'_>, value: Option<&str>) -> Option<String> {
    let value = value?;
    #[cfg(test)]
    if AML_ID_COPY_REJECTION.with(|site| site.replace(false)) {
        *cx.relation_failed = true;
        return None;
    }
    match try_clone_string(value) {
        Some(value) => Some(value),
        None => {
            *cx.relation_failed = true;
            None
        }
    }
}

struct BuildCtx<'a> {
    nodes: &'a mut SceneNodes,
    aml_id_index: &'a mut AmlIdIndex,
    relation_failed: &'a mut bool,
}

/// How many runs flattening one inline `Text` element produces.
///
/// Mirrors `flatten_inline_text` exactly: one run for non-empty content, plus
/// whatever each `Text` grandchild contributes. Counting before building is
/// what lets the run vector be reserved exactly rather than grown from remote
/// nesting. Checked throughout, so depth that would overflow the count
/// refuses instead of wrapping into a small reservation.
fn count_flattened_runs(child: &TextElement) -> Option<usize> {
    let mut total = usize::from(!child.content.is_empty());
    for grandchild in &child.children {
        if let Element::Text(gc) = grandchild {
            total = total.checked_add(count_flattened_runs(gc)?)?;
        }
    }
    Some(total)
}

/// The run bound for a `Text` element flattened into a single node.
fn count_text_runs(t: &TextElement) -> Option<usize> {
    let mut total = usize::from(!t.content.is_empty());
    for child in &t.children {
        if let Element::Text(child_t) = child {
            total = total.checked_add(count_flattened_runs(child_t)?)?;
        }
    }
    Some(total)
}

fn insert_node(cx: &mut BuildCtx, builder: NodeBuilder) -> NodeId {
    let indexed = builder.aml_id.is_some();
    let Some(id) = cx.nodes.insert_with_key(|id| builder.finish(id)) else {
        *cx.relation_failed = true;
        return NodeId::default();
    };
    if indexed {
        if cx.aml_id_index.len() < cx.aml_id_index.capacity() {
            cx.aml_id_index.push(id);
        } else {
            *cx.relation_failed = true;
        }
    }
    id
}

fn attach_child(cx: &mut BuildCtx, parent: NodeId, child: NodeId) {
    if *cx.relation_failed {
        return;
    }
    if cx
        .nodes
        .get_mut(parent)
        .is_none_or(|parent_node| parent_node.children.try_reserve_exact(1).is_err())
    {
        *cx.relation_failed = true;
        return;
    }
    if let Some(child_node) = cx.nodes.get_mut(child) {
        child_node.parent = Some(parent);
    }
    if let Some(parent_node) = cx.nodes.get_mut(parent) {
        parent_node.children.push(child);
    }
}

/// Build a node for one element. Returns `None` for ancillary
/// elements — those contribute no scene node but may have side
/// effects elsewhere: `[on]` bindings are collected separately by
/// `collect_event_bindings` into `Scene.event_bindings`.
fn build_element(cx: &mut BuildCtx, element: &Element) -> Option<NodeId> {
    build_element_inner(cx, element)
}

fn build_element_inner(cx: &mut BuildCtx, element: &Element) -> Option<NodeId> {
    match element {
        Element::Box(b) => Some(build_box(cx, b)),
        Element::Row(r) => Some(build_row(cx, r)),
        Element::Col(c) => {
            let data = FlowData {
                source: FlowSource::Col,
                width: Some(c.w),
                align: c.align,
                ..FlowData::default()
            };
            Some(build_flow_container_with_data(cx, data, &c.children))
        }
        Element::Hr(h) => Some(build_leaf(
            cx,
            NodeKind::Hr(HrData {
                fg: color_name(h.fg.as_ref()),
                style: h.style,
            }),
        )),
        Element::Spacer(s) => Some(build_leaf(cx, NodeKind::Spacer { lines: s.lines })),

        Element::Header(c) => Some(build_flow_container(cx, FlowSource::Header, &c.children)),
        Element::Body(c) => Some(build_flow_container(cx, FlowSource::Body, &c.children)),
        Element::Footer(c) => Some(build_flow_container(cx, FlowSource::Footer, &c.children)),
        Element::Nav(n) => {
            let data = FlowData {
                source: FlowSource::Nav,
                sticky: n.sticky,
                ..FlowData::default()
            };
            Some(build_flow_container_with_data(cx, data, &n.children))
        }
        Element::Thead(c) => Some(build_flow_container(cx, FlowSource::Thead, &c.children)),
        Element::Tbody(c) => Some(build_flow_container(cx, FlowSource::Tbody, &c.children)),
        Element::Pagination(c) => Some(build_flow_container(
            cx,
            FlowSource::Pagination,
            &c.children,
        )),
        Element::List(l) => {
            let data = FlowData {
                source: FlowSource::List,
                list_style: Some(l.style),
                list_bullet_char: l.bullet_char,
                ..FlowData::default()
            };
            Some(build_flow_container_with_data(cx, data, &l.children))
        }
        Element::Item(i) => Some(build_flow_container(cx, FlowSource::Item, &i.children)),
        Element::Form(f) => {
            let data = FlowData {
                source: FlowSource::Form,
                form_action: Some(f.action.clone()),
                ..FlowData::default()
            };
            Some(build_flow_container_with_data(cx, data, &f.children))
        }

        Element::Text(t) => Some(build_text(cx, t)),
        Element::Pre(p) => {
            // One scene run per parsed run, with the block's own fg/bg standing
            // in for spans that set neither. This vector was always here and was
            // always handed exactly one element; carrying the spans through to it
            // is the whole of the change -- the layout and the cell buffer could
            // already draw per-run colour, there was simply no way to say it in
            // the markup.
            // Reserved exactly, and a refusal marks the scene's relation storage
            // failed the way `build_text` does for the same shape of growth. A
            // bare `.collect()` grows a vector straight from remote nesting
            // without admitting it -- which is what the allocation audit flagged,
            // and it was right: the run count comes from the page.
            let mut runs: Vec<TextRun> = Vec::new();
            if runs.try_reserve_exact(p.runs.len()).is_err() {
                *cx.relation_failed = true;
            }
            runs.extend(
                p.runs
                    .iter()
                    .take(if *cx.relation_failed { 0 } else { p.runs.len() })
                    .map(|r| TextRun {
                        text: r.text.clone(),
                        fg: color_name(r.fg.as_ref().or(p.fg.as_ref())),
                        bg: color_name(r.bg.as_ref().or(p.bg.as_ref())),
                        bold: r.bold,
                        italic: r.italic,
                        underline: r.underline,
                        strikethrough: r.strikethrough,
                        dim: r.dim,
                        blink: r.blink,
                    }),
            );
            let tc = TextContent {
                runs,
                align: p.align,
                source: TextSource::Pre,
            };
            Some(build_leaf(cx, NodeKind::Text(tc)))
        }
        Element::Heading(h) => {
            let tc = TextContent {
                runs: vec![TextRun {
                    text: h.content.clone(),
                    fg: color_name(h.fg.as_ref()),
                    bold: true,
                    ..TextRun::default()
                }],
                align: Alignment::Left,
                source: TextSource::Heading(h.level),
            };
            // Heading child text flattens into the same node (inline);
            // `kinds::text::layout_heading` handles node-bearing inline
            // children (links, buttons) via `collect_inline_segments`.
            let node = insert_node(cx, NodeBuilder::new(NodeKind::Text(tc)));
            for child in &h.children {
                if classify(child) == Category::NodeBearing
                    && let Some(child_id) = build_element(cx, child)
                {
                    attach_child(cx, node, child_id);
                }
            }
            Some(node)
        }
        Element::Art(a) => {
            let tc = TextContent {
                runs: vec![TextRun {
                    text: a.content.clone(),
                    ..TextRun::default()
                }],
                align: Alignment::Left,
                source: TextSource::Art,
            };
            Some(build_leaf(cx, NodeKind::Text(tc)))
        }
        Element::ElementDef(e) => {
            let tc = TextContent {
                runs: vec![TextRun {
                    text: e.content.trim().to_string(),
                    fg: color_name(e.fg.as_ref()),
                    ..TextRun::default()
                }],
                align: Alignment::Left,
                source: TextSource::ElementDef,
            };
            let aml_id = try_clone_aml_id(cx, Some(e.id.as_str()));
            Some(insert_node(
                cx,
                NodeBuilder::new(NodeKind::Text(tc)).aml_id(aml_id),
            ))
        }
        Element::TextAnimate(ta) => {
            let tc = TextContent {
                runs: vec![TextRun {
                    text: ta.content.clone(),
                    ..TextRun::default()
                }],
                align: Alignment::Left,
                source: TextSource::TextAnimate,
            };
            Some(build_leaf(cx, NodeKind::Text(tc)))
        }

        Element::Link(l) => Some(build_link(cx, l)),
        Element::Input(i) => Some(build_input(cx, i)),
        Element::Select(s) => Some(build_select(cx, s)),
        Element::Option(o) => {
            // Option under Select is a leaf with OptionData.
            Some(build_leaf(
                cx,
                NodeKind::OptionLeaf(OptionData {
                    value: o.value.clone(),
                    selected: o.selected,
                    label: o.label.clone(),
                }),
            ))
        }
        Element::Button(b) => Some(build_button(cx, b)),

        Element::Table(_) => {
            let node = insert_node(cx, NodeBuilder::new(NodeKind::Table));
            if let Element::Table(tbl) = element {
                for child in &tbl.children {
                    if classify(child) == Category::NodeBearing
                        && let Some(child_id) = build_element(cx, child)
                    {
                        attach_child(cx, node, child_id);
                    }
                }
            }
            Some(node)
        }
        Element::Tr(tr) => {
            let node = insert_node(cx, NodeBuilder::new(NodeKind::Tr));
            for child in &tr.children {
                if classify(child) == Category::NodeBearing
                    && let Some(child_id) = build_element(cx, child)
                {
                    attach_child(cx, node, child_id);
                }
            }
            Some(node)
        }
        Element::Th(c) => {
            let node = insert_node(
                cx,
                NodeBuilder::new(NodeKind::Th(CellData {
                    fg: color_name(c.fg.as_ref()),
                })),
            );
            for child in &c.children {
                if classify(child) == Category::NodeBearing
                    && let Some(child_id) = build_element(cx, child)
                {
                    attach_child(cx, node, child_id);
                }
            }
            Some(node)
        }
        Element::Td(c) => {
            let node = insert_node(
                cx,
                NodeBuilder::new(NodeKind::Td(CellData {
                    fg: color_name(c.fg.as_ref()),
                })),
            );
            for child in &c.children {
                if classify(child) == Category::NodeBearing
                    && let Some(child_id) = build_element(cx, child)
                {
                    attach_child(cx, node, child_id);
                }
            }
            Some(node)
        }

        Element::Animate(a) => Some(build_animation(cx, a)),
        Element::Frame(f) => Some(build_flow_container(cx, FlowSource::Frame, &f.children)),

        Element::Live(l) => Some(build_live(cx, l)),

        Element::Panel(p) => Some(build_panel(cx, p)),
        Element::State(s) => Some(build_state(cx, s)),
        Element::Details(d) => Some(build_details(cx, d)),

        // Ancillary — no node. See `classify` for why `Include` is here.
        Element::Tween(_) | Element::On(_) | Element::Include(_) => None,
    }
}

fn build_leaf(cx: &mut BuildCtx, kind: NodeKind) -> NodeId {
    insert_node(cx, NodeBuilder::new(kind))
}

fn build_flow_container(cx: &mut BuildCtx, source: FlowSource, children: &[Element]) -> NodeId {
    let flow = FlowData {
        source,
        ..FlowData::default()
    };
    build_flow_container_with_data(cx, flow, children)
}

fn build_flow_container_with_data(
    cx: &mut BuildCtx,
    flow: FlowData,
    children: &[Element],
) -> NodeId {
    let node = insert_node(cx, NodeBuilder::new(NodeKind::Flow(flow)));
    for child in children {
        if classify(child) == Category::NodeBearing
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
        }
    }
    node
}

fn build_box(cx: &mut BuildCtx, b: &ast::BoxElement) -> NodeId {
    let border = border_tag(b.border);
    let kind = if b.x.is_some() || b.y.is_some() {
        NodeKind::Absolute(AbsoluteData {
            x: b.x,
            y: b.y,
            w: b.w,
            h: b.h,
            border,
            title: b.title.clone(),
            join_top: b.join_top,
            join_bottom: b.join_bottom,
            join_left: b.join_left,
            join_right: b.join_right,
            padding: b.padding,
            fg: b.fg.clone(),
            bg: b.bg.clone(),
            align: b.align,
        })
    } else {
        NodeKind::Flow(FlowData {
            source: FlowSource::Box,
            border,
            title: b.title.clone(),
            join_top: b.join_top,
            join_bottom: b.join_bottom,
            join_left: b.join_left,
            join_right: b.join_right,
            padding: b.padding,
            width: Some(b.w),
            height: Some(b.h),
            fg: b.fg.clone(),
            bg: b.bg.clone(),
            align: b.align,
            sticky: b.sticky,
            ..FlowData::default()
        })
    };
    let node = insert_node(cx, NodeBuilder::new(kind));
    for child in &b.children {
        if classify(child) == Category::NodeBearing
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
        }
    }
    node
}

fn build_row(cx: &mut BuildCtx, r: &ast::RowElement) -> NodeId {
    let data = super::node::RowData {
        gap: r.gap,
        align: r.align,
    };
    let node = insert_node(cx, NodeBuilder::new(NodeKind::Row(data)));
    for child in &r.children {
        if classify(child) == Category::NodeBearing
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
        }
    }
    node
}

fn build_text(cx: &mut BuildCtx, t: &TextElement) -> NodeId {
    let style = TextRun {
        bold: t.bold,
        italic: t.italic,
        underline: t.underline,
        strikethrough: t.strikethrough,
        dim: t.dim,
        blink: t.blink,
        fg: color_name(t.fg.as_ref()),
        bg: color_name(t.bg.as_ref()),
        text: String::new(),
    };

    // Detect whether this Text contains any node-bearing inline
    // child (Link/Button). If so, we keep sibling ordering intact
    // by emitting each text-inline child as its own scene Text node
    // rather than flattening into `runs`. This lets the scene-
    // native inline collector walk children in source order.
    let has_node_bearing_inline = t
        .children
        .iter()
        .any(|c| !matches!(c, Element::Text(_)) && classify(c) == Category::NodeBearing);

    // Reserve exactly what the flattening will produce. A refusal marks the
    // scene's relation storage failed — the channel `insert_node` already
    // uses — and the fill loops below are skipped, so the finished scene
    // reports `resource_error` rather than growing a run vector from remote
    // nesting it never admitted.
    let run_bound = if has_node_bearing_inline {
        Some(usize::from(!t.content.is_empty()))
    } else {
        count_text_runs(t)
    };
    let mut runs = Vec::new();
    match run_bound {
        Some(bound) if runs.try_reserve_exact(bound).is_ok() => {}
        _ => *cx.relation_failed = true,
    }
    if !*cx.relation_failed && !t.content.is_empty() {
        runs.push(TextRun {
            text: t.content.clone(),
            ..style.clone()
        });
    }

    let mut inline_child_ids: Vec<NodeId> = Vec::new();
    if has_node_bearing_inline
        && !*cx.relation_failed
        && inline_child_ids
            .try_reserve_exact(t.children.len())
            .is_err()
    {
        *cx.relation_failed = true;
    }

    if has_node_bearing_inline && !*cx.relation_failed {
        // Preserve source ordering: each sibling (text or node-bearing)
        // becomes a separate scene child.
        for child in &t.children {
            match child {
                Element::Text(child_t) => {
                    // Build a scene Text node whose runs inherit the
                    // parent style so the inline collector can merge
                    // styling correctly.
                    let mut child_runs = Vec::new();
                    match count_flattened_runs(child_t) {
                        Some(bound) if child_runs.try_reserve_exact(bound).is_ok() => {
                            flatten_inline_text(&style, child_t, &mut child_runs);
                        }
                        _ => *cx.relation_failed = true,
                    }
                    if !child_runs.is_empty() {
                        let tc = TextContent {
                            runs: child_runs,
                            align: child_t.align,
                            source: TextSource::Text,
                        };
                        let id = insert_node(cx, NodeBuilder::new(NodeKind::Text(tc)));
                        inline_child_ids.push(id);
                    }
                }
                other if classify(other) == Category::NodeBearing => {
                    if let Some(child_id) = build_element(cx, other) {
                        inline_child_ids.push(child_id);
                    }
                }
                _ => {}
            }
        }
    } else if !*cx.relation_failed {
        // No node-bearing inline children: classic flatten.
        for child in &t.children {
            if let Element::Text(child_t) = child {
                flatten_inline_text(&style, child_t, &mut runs);
            }
        }
    }

    let tc = TextContent {
        runs,
        align: t.align,
        source: TextSource::Text,
    };
    let node = insert_node(cx, NodeBuilder::new(NodeKind::Text(tc)));
    for child_id in inline_child_ids {
        attach_child(cx, node, child_id);
    }
    node
}

fn flatten_inline_text(parent_style: &TextRun, child: &TextElement, runs: &mut Vec<TextRun>) {
    let merged = TextRun {
        text: String::new(),
        bold: parent_style.bold || child.bold,
        italic: parent_style.italic || child.italic,
        underline: parent_style.underline || child.underline,
        strikethrough: parent_style.strikethrough || child.strikethrough,
        dim: parent_style.dim || child.dim,
        blink: parent_style.blink || child.blink,
        fg: color_name(child.fg.as_ref()).or_else(|| parent_style.fg.clone()),
        bg: color_name(child.bg.as_ref()).or_else(|| parent_style.bg.clone()),
    };
    if !child.content.is_empty() {
        runs.push(TextRun {
            text: child.content.clone(),
            ..merged.clone()
        });
    }
    for grandchild in &child.children {
        if let Element::Text(gc) = grandchild {
            flatten_inline_text(&merged, gc, runs);
        }
    }
}

fn build_link(cx: &mut BuildCtx, l: &ast::LinkElement) -> NodeId {
    let kind = NodeKind::Link(LinkData {
        href: l.href.clone(),
        key: l.key,
        prefetch: l.prefetch,
        transition: l.transition,
        transition_duration_ms: l.transition_duration_ms,
        defer_animation: l.defer_animation.clone(),
    });
    let action = Action::Navigate {
        href: l.href.clone(),
        transition: l.transition,
        transition_duration_ms: l.transition_duration_ms,
        defer_animation: l.defer_animation.clone(),
    };
    let aml_id = try_clone_aml_id(cx, l.id.as_deref());
    let node = insert_node(
        cx,
        NodeBuilder::new(kind)
            .aml_id(aml_id)
            .focusable(true)
            .hit_target(Some(action)),
    );
    for child in &l.children {
        if classify(child) == Category::NodeBearing
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
        }
    }
    node
}

fn build_input(cx: &mut BuildCtx, i: &ast::InputElement) -> NodeId {
    let kind = NodeKind::Input(InputData {
        name: i.name.clone(),
        maxlen: i.maxlen,
        placeholder: i.placeholder.clone(),
        multiline: i.multiline,
        rows: i.rows,
        password: i.password,
        value: i.value.clone(),
    });
    let aml_id = try_clone_aml_id(cx, i.id.as_deref());
    insert_node(cx, NodeBuilder::new(kind).aml_id(aml_id).focusable(true))
}

fn build_select(cx: &mut BuildCtx, s: &ast::SelectElement) -> NodeId {
    let selected_index = s
        .children
        .iter()
        .filter_map(|child| match child {
            Element::Option(option) => Some(option),
            _ => None,
        })
        .position(|option| option.selected)
        .unwrap_or(0);
    let kind = NodeKind::Select(SelectData {
        name: s.name.clone(),
        label: s.label.clone(),
        selected_index,
    });
    let node = insert_node(cx, NodeBuilder::new(kind).focusable(true));
    for child in &s.children {
        if let Element::Option(_) = child
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
        }
    }
    node
}

fn build_button(cx: &mut BuildCtx, b: &ast::ButtonElement) -> NodeId {
    let action = button_action(b);
    let kind = NodeKind::Button(ButtonData {
        label: b.label.clone(),
        key: b.key,
        action: b.action,
        target: b.target.clone(),
        states: b.states.clone(),
        to: b.to.clone(),
        href: b.href.clone(),
        transition: b.transition,
        transition_duration_ms: b.transition_duration_ms,
    });
    insert_node(
        cx,
        NodeBuilder::new(kind)
            .focusable(true)
            .hit_target(Some(action)),
    )
}

fn button_action(b: &ast::ButtonElement) -> Action {
    use ast::ButtonAction as BA;
    match b.action {
        BA::Navigate => Action::Navigate {
            href: b.href.clone().unwrap_or_default(),
            transition: b.transition,
            transition_duration_ms: b.transition_duration_ms,
            defer_animation: None,
        },
        BA::Toggle => Action::TogglePanel {
            panel: b.target.clone().unwrap_or_default(),
            states: b.states.clone().unwrap_or_default(),
        },
        BA::Set => Action::SetPanelState {
            panel: b.target.clone().unwrap_or_default(),
            state: b.to.clone().unwrap_or_default(),
        },
        BA::Submit => Action::Submit {
            form: b.target.clone().unwrap_or_default(),
        },
    }
}

fn build_animation(cx: &mut BuildCtx, a: &ast::AnimateElement) -> NodeId {
    // Frame-based animations hold their frames as children. WASM
    // animations have no frame children.
    let mut frame_ids = Vec::new();
    let frame_bound = a
        .children
        .iter()
        .filter(|child| matches!(child, Element::Frame(_)))
        .count();
    if frame_ids.try_reserve_exact(frame_bound).is_err() {
        *cx.relation_failed = true;
    }
    let placeholder = NodeKind::Animation(AnimationData {
        fps: a.fps as u16,
        autoplay: a.autoplay,
        loop_behavior: a.loop_behavior,
        loop_: !matches!(a.loop_behavior, ast::LoopBehavior::None),
        background: a.background,
        src: a.src.clone(),
        frames: Vec::new(),
        delay_ms: a.delay_ms,
        after: a.after.clone(),
        x: a.x,
        y: a.y,
        w: a.w,
        h: a.h,
    });
    let aml_id = try_clone_aml_id(cx, Some(a.id.as_str()));
    let node = insert_node(cx, NodeBuilder::new(placeholder).aml_id(aml_id));

    for child in &a.children {
        if let Element::Frame(_) = child {
            if let Some(child_id) = build_element(cx, child) {
                attach_child(cx, node, child_id);
                frame_ids.push(child_id);
            }
        } else if classify(child) == Category::NodeBearing {
            // Non-frame node-bearing children (content for WASM animations
            // to read via `get_content_cell`) attach to the Animation node
            // but don't appear in `AnimationData.frames`.
            if let Some(child_id) = build_element(cx, child) {
                attach_child(cx, node, child_id);
            }
        }
    }

    // Fix up the Animation's frames list now that we have the IDs.
    if let Some(node_mut) = cx.nodes.get_mut(node)
        && let NodeKind::Animation(data) = &mut node_mut.kind
    {
        data.frames = frame_ids;
    }
    node
}

fn build_live(cx: &mut BuildCtx, l: &ast::LiveElement) -> NodeId {
    let kind = NodeKind::LiveRegion(LiveData {
        endpoint: l.endpoint.clone(),
        height: l.height,
        scroll: l.scroll,
        buffer: l.buffer,
        delta: l.delta,
    });
    let aml_id = try_clone_aml_id(cx, Some(l.id.as_str()));
    let node = insert_node(cx, NodeBuilder::new(kind).aml_id(aml_id));
    for child in &l.children {
        if classify(child) == Category::NodeBearing
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
        }
    }
    node
}

fn build_panel(cx: &mut BuildCtx, p: &PanelElement) -> NodeId {
    // Build state children first, then record their ids in the panel's kind.
    let mut state_ids = Vec::new();
    let state_bound = p
        .children
        .iter()
        .filter(|child| matches!(child, Element::State(_)))
        .count();
    if state_ids.try_reserve_exact(state_bound).is_err() {
        *cx.relation_failed = true;
    }
    for child in &p.children {
        if let Element::State(_) = child
            && let Some(child_id) = build_element(cx, child)
        {
            state_ids.push(child_id);
        }
    }
    // Pick initial_state's node (fallback to first) as `active`.
    let active = state_ids
        .iter()
        .copied()
        .find(|&id| {
            cx.nodes
                .get(id)
                .and_then(|n| {
                    if let NodeKind::Flow(fd) = n.kind()
                        && fd.source == FlowSource::State
                    {
                        return n.aml_id();
                    }
                    None
                })
                .is_some_and(|name| name == p.initial_state.as_str())
        })
        .or_else(|| state_ids.first().copied())
        .unwrap_or(NodeId::default());

    let kind = NodeKind::Panel {
        states: state_ids.clone(),
        active,
        initial_state: p.initial_state.clone(),
    };
    let aml_id = try_clone_aml_id(cx, Some(p.id.as_str()));
    let node = insert_node(cx, NodeBuilder::new(kind).aml_id(aml_id));
    for state_id in state_ids {
        attach_child(cx, node, state_id);
    }
    node
}

fn build_state(cx: &mut BuildCtx, s: &ast::StateElement) -> NodeId {
    let flow = FlowData {
        source: FlowSource::State,
        state_name: Some(s.name.clone()),
        state_transition: s.transition.as_deref().and_then(ast::parse_transition_kind),
        state_transition_duration_ms: s.duration_ms,
        ..FlowData::default()
    };
    // aml_id stores the state's `name` so the Panel builder can match
    // `initial_state` by walking children.
    let aml_id = try_clone_aml_id(cx, Some(s.name.as_str()));
    let node = insert_node(cx, NodeBuilder::new(NodeKind::Flow(flow)).aml_id(aml_id));
    for child in &s.children {
        if classify(child) == Category::NodeBearing
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
        }
    }
    node
}

fn build_details(cx: &mut BuildCtx, d: &ast::DetailsElement) -> NodeId {
    let flow = FlowData {
        source: FlowSource::Details,
        details_open: d.open,
        details_summary: if d.summary_children.is_empty() {
            Some(d.summary.clone())
        } else {
            None
        },
        details_summary_count: 0, // set below once summary children are built
        ..FlowData::default()
    };
    let node = insert_node(cx, NodeBuilder::new(NodeKind::Flow(flow)));

    // Summary children first (if any), then body children (if open).
    // `details_summary_count` records how many of this node's scene
    // children belong to the summary line so layout can split them.
    let mut summary_count: u16 = 0;
    for child in &d.summary_children {
        if classify(child) == Category::NodeBearing
            && let Some(child_id) = build_element(cx, child)
        {
            attach_child(cx, node, child_id);
            summary_count = summary_count.saturating_add(1);
        }
    }
    if d.open {
        for child in &d.children {
            if classify(child) == Category::NodeBearing
                && let Some(child_id) = build_element(cx, child)
            {
                attach_child(cx, node, child_id);
            }
        }
    }
    // Patch the summary_count after child build.
    if let Some(n) = cx.nodes.get_mut(node)
        && let NodeKind::Flow(ref mut fd) = n.kind
    {
        fd.details_summary_count = summary_count;
    }
    node
}

fn border_tag(border: ast::BorderStyle) -> BorderStyleTag {
    match border {
        ast::BorderStyle::None => BorderStyleTag::None,
        ast::BorderStyle::Single => BorderStyleTag::Single,
        ast::BorderStyle::Double => BorderStyleTag::Double,
        ast::BorderStyle::Rounded => BorderStyleTag::Rounded,
        ast::BorderStyle::Heavy => BorderStyleTag::Heavy,
        ast::BorderStyle::Ascii => BorderStyleTag::Ascii,
    }
}

fn color_name(c: Option<&Color>) -> Option<Color> {
    c.cloned()
}

// Silence unused-warning for `Dimension` — used as a type annotation only
// in `AnimationData`/`LiveData`.
#[allow(dead_code)]
const _D: Option<Dimension> = None;
