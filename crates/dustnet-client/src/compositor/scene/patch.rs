//! `Patch`: the currency of structural scene change.
//!
//! Every observable scene mutation — a panel state flip, a tween's
//! transform update, a focus change — expresses itself as a `Patch`
//! applied by `PatchApplier`. Per-cell buffer writes are *not* patches;
//! those are handled by kind-gated `Scene::*_buffer_mut` accessors and
//! signalled via `Invalidation.composite`. See `docs/internals/compositor.md`
//! section "Scene state vs. subsystem state" for the two-channel model.
//!
//! ## Authority
//!
//! `PatchApplier` lives **inside** the scene module so it can write
//! `pub(super)` fields on `Node` and `Scene` directly. External callers
//! produce `Patch` values and hand them to `apply`; they cannot bypass
//! the gate because the fields aren't visible from outside this module.
//! This is the compile-time enforcement of Invariant 2a
//! ("structural scene changes go through patches") from compositor.md.

use crate::compositor::layout::Rect;
use crate::compositor::layout::engine::Placement;
use crate::resource::ResourceCategory;

use super::node::{Node, NodeId, NodeKind, Transform};
use super::tree::{Scene, ScrollState};

/// A declarative mutation applied to the scene.
///
/// See `docs/internals/compositor.md` "Patch" for the full vocabulary. `Patch` is
/// an `enum` rather than a trait because the set is bounded: input
/// dispatch, animation advancement, protocol live-updates, and trigger
/// firing are the only sources that produce patches, and the list of
/// scene changes they can request is finite and doesn't grow over time.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Patch {
    // ─── Structural ─────────────────────────────────────────────
    /// Insert `node` as a child of `parent`, before `before` (or
    /// appended if `None`). The builder allocates the new `NodeId`
    /// when `apply` runs.
    InsertNode {
        parent: NodeId,
        before: Option<NodeId>,
        node: NodeTemplate,
    },
    /// Remove `node` and its subtree.
    RemoveNode { node: NodeId },
    /// Replace a parent's children wholesale.
    ReplaceChildren {
        parent: NodeId,
        children: Vec<NodeId>,
    },

    // ─── Properties ─────────────────────────────────────────────
    /// Write a node's `placement`. Derived from layout; normally
    /// produced by the layout pass rather than external code.
    SetPlacement { node: NodeId, placement: Placement },
    /// Transform offset applied at composite time. Used by tweens.
    SetTransform { node: NodeId, transform: Transform },
    /// Hide/show a node. Hidden nodes do not composite but remain in
    /// the tree; their bbox stops contributing to parent placement.
    SetVisible { node: NodeId, visible: bool },
    /// Change stacking order among siblings.
    SetZIndex { node: NodeId, z_index: i16 },

    // ─── Panel / state ──────────────────────────────────────────
    /// Flip a panel to a specific state.
    SetPanelActive { panel: NodeId, active: NodeId },

    /// Toggle the open/closed state of a `[details]` node. Flips
    /// `FlowData.details_open` on `NodeKind::Flow { source: Details }`.
    /// No-op if the node isn't a Details flow.
    ToggleDetails { node: NodeId },

    /// Replace the current value of an input control. The subsequent
    /// layout pass redraws the node's owned buffer from this value.
    SetInputValue { node: NodeId, value: String },

    /// Select an option by its child-option index.
    SetSelectIndex { node: NodeId, index: usize },

    // ─── Focus / scroll ─────────────────────────────────────────
    /// Move focus, or clear it with `None`.
    SetFocus { node: Option<NodeId> },
    /// Set the viewport scroll offset.
    SetScroll { offset: u16 },
}

/// Inline template for a node being inserted via `Patch::InsertNode`.
/// Distinct from `Node` because `NodeId` is allocated at apply-time.
#[derive(Debug, Clone)]
pub struct NodeTemplate {
    pub kind: NodeKind,
    pub aml_id: Option<String>,
    pub focusable: bool,
    pub hit_target: Option<super::node::Action>,
}

/// Applies patches to a scene. Stateless — the "struct" exists as a
/// namespace for the `apply` method in case per-apply context (e.g.
/// telemetry, transactional rollback) lands later.
pub struct PatchApplier;

impl PatchApplier {
    /// Apply one patch. Mutates `scene` (structural and property state)
    /// and populates `scene.invalidation` with the regions that need
    /// re-layout or re-composition.
    ///
    /// Arrival order within a tick is significant; inter-tick order
    /// between patches produced in parallel is undefined (Invariant 7).
    pub fn apply(scene: &mut Scene, patch: Patch) {
        match patch {
            Patch::InsertNode {
                parent,
                before,
                node,
            } => {
                apply_insert(scene, parent, before, node);
            }
            Patch::RemoveNode { node } => {
                apply_remove(scene, node);
            }
            Patch::ReplaceChildren { parent, children } => {
                apply_replace_children(scene, parent, children);
            }
            Patch::SetPlacement { node, placement } => {
                apply_set_placement(scene, node, placement);
            }
            Patch::SetTransform { node, transform } => {
                apply_set_transform(scene, node, transform);
            }
            Patch::SetVisible { node, visible } => {
                apply_set_visible(scene, node, visible);
            }
            Patch::SetZIndex { node, z_index } => {
                apply_set_z_index(scene, node, z_index);
            }
            Patch::SetPanelActive { panel, active } => {
                apply_set_panel_active(scene, panel, active);
            }
            Patch::ToggleDetails { node } => {
                apply_toggle_details(scene, node);
            }
            Patch::SetInputValue { node, value } => {
                apply_set_input_value(scene, node, value);
            }
            Patch::SetSelectIndex { node, index } => {
                apply_set_select_index(scene, node, index);
            }
            Patch::SetFocus { node } => {
                apply_set_focus(scene, node);
            }
            Patch::SetScroll { offset } => {
                apply_set_scroll(scene, offset);
            }
        }
    }

    /// Apply a whole batch in arrival order.
    pub fn apply_all(scene: &mut Scene, patches: impl IntoIterator<Item = Patch>) {
        for patch in patches {
            Self::apply(scene, patch);
        }
    }
}

fn apply_set_input_value(scene: &mut Scene, node: NodeId, value: String) {
    let Some(target) = scene.nodes.get_mut(node) else {
        return;
    };
    if let NodeKind::Input(data) = &mut target.kind {
        data.value = Some(value);
        let rect = target.placement.rect;
        mark_layout(scene, node);
        scene.invalidation.mark_composite(rect);
    }
}

fn apply_set_select_index(scene: &mut Scene, node: NodeId, index: usize) {
    let Some(target) = scene.nodes.get_mut(node) else {
        return;
    };
    if let NodeKind::Select(data) = &mut target.kind {
        data.selected_index = index;
        let rect = target.placement.rect;
        mark_layout(scene, node);
        scene.invalidation.mark_composite(rect);
    }
}

// ─── Handlers ───────────────────────────────────────────────────

fn mark_layout(scene: &mut Scene, node: NodeId) {
    if !scene.invalidation.layout.insert(node) {
        scene.resource_error = true;
    }
}

fn apply_insert(scene: &mut Scene, parent: NodeId, before: Option<NodeId>, tpl: NodeTemplate) {
    if !scene.nodes.has_spare_capacity()
        && !scene.has_governed_nodes()
        && scene.nodes.try_reserve(1).is_err()
    {
        scene.resource_error = true;
        return;
    }
    if !scene.nodes.has_spare_capacity()
        || scene.has_governed_node_relations()
            && (scene
                .nodes
                .get(parent)
                .is_none_or(|node| node.children.len() == node.children.capacity())
                || tpl.aml_id.is_some()
                    && scene.aml_id_index.len() == scene.aml_id_index.capacity())
    {
        scene.resource_error = true;
        return;
    }
    let Some(new_id) = scene.nodes.insert_with_key(|id| Node {
        id,
        kind: tpl.kind,
        parent: Some(parent),
        children: Vec::new(),
        placement: Placement::empty_at(0, 0),
        focusable_screen_rect: None,
        buffer: None,
        z_index: 0,
        transform: Transform::IDENTITY,
        visible: true,
        focusable: tpl.focusable,
        hit_target: tpl.hit_target,
        aml_id: tpl.aml_id,
    }) else {
        scene.resource_error = true;
        return;
    };
    if scene
        .nodes
        .get(new_id)
        .is_some_and(|node| node.aml_id.is_some())
    {
        scene.aml_id_index.push(new_id);
    }
    if let Some(parent_node) = scene.nodes.get_mut(parent) {
        if let Some(before_id) = before {
            if let Some(idx) = parent_node.children.iter().position(|&c| c == before_id) {
                parent_node.children.insert(idx, new_id);
            } else {
                parent_node.children.push(new_id);
            }
        } else {
            parent_node.children.push(new_id);
        }
    }
    mark_layout(scene, parent);
    if let Some(rect) = subtree_bbox(scene, parent) {
        scene.invalidation.mark_composite(rect);
    }
}

fn apply_remove(scene: &mut Scene, node: NodeId) {
    let Some(victim) = scene.nodes.get(node) else {
        return;
    };
    let parent = victim.parent;

    // Collect all descendants via breadth-first walk. The scene size is a
    // conservative bound that can be admitted before the remotely shaped
    // traversal allocation. Failure leaves the tree untouched.
    let capacity = scene.nodes.len();
    let requested_bytes = match capacity.checked_mul(std::mem::size_of::<NodeId>()) {
        Some(bytes) => bytes,
        None => {
            scene.resource_error = true;
            return;
        }
    };
    let mut traversal_lease = match scene.governor.as_ref() {
        Some(governor) => {
            match governor.reserve(ResourceCategory::RemoteCollections, requested_bytes) {
                Ok(lease) => Some(lease),
                Err(_) => {
                    scene.resource_error = true;
                    return;
                }
            }
        }
        None => None,
    };
    let mut to_remove = Vec::new();
    if to_remove.try_reserve_exact(capacity).is_err() {
        scene.resource_error = true;
        return;
    }
    let retained_bytes = match to_remove
        .capacity()
        .checked_mul(std::mem::size_of::<NodeId>())
    {
        Some(bytes) => bytes,
        None => {
            scene.resource_error = true;
            return;
        }
    };
    if let Some(lease) = traversal_lease.as_mut()
        && lease
            .try_resize_with_cost(retained_bytes, retained_bytes)
            .is_err()
    {
        scene.resource_error = true;
        return;
    }
    to_remove.push(node);
    // Invalidate the subtree's rect only after traversal admission succeeds.
    if let Some(rect) = subtree_bbox(scene, node) {
        scene.invalidation.mark_composite(rect);
    }
    let mut i = 0;
    while let Some(&id) = to_remove.get(i) {
        if let Some(n) = scene.nodes.get(id) {
            to_remove.extend_from_slice(&n.children);
        }
        i += 1;
    }
    for id in to_remove {
        let removes_winning_id = scene
            .nodes
            .get(id)
            .and_then(|node| node.aml_id())
            .is_some_and(|aml_id| {
                scene.aml_id_index.iter().rev().copied().find(|indexed| {
                    scene
                        .nodes
                        .get(*indexed)
                        .is_some_and(|node| node.aml_id() == Some(aml_id))
                }) == Some(id)
            });
        if removes_winning_id
            && let nodes = &scene.nodes
            && let Some(aml_id) = nodes.get(id).and_then(|node| node.aml_id())
        {
            scene.aml_id_index.retain(|indexed| {
                nodes
                    .get(*indexed)
                    .is_none_or(|node| node.aml_id() != Some(aml_id))
            });
        } else {
            scene.aml_id_index.retain(|indexed| *indexed != id);
        }
        scene.nodes.remove(id);
    }
    if let Some(parent_id) = parent {
        if let Some(parent_node) = scene.nodes.get_mut(parent_id) {
            parent_node.children.retain(|&c| c != node);
        }
        mark_layout(scene, parent_id);
    }
    if !scene.reconcile_node_relation_lease() {
        scene.resource_error = true;
    }
}

fn apply_replace_children(scene: &mut Scene, parent: NodeId, children: Vec<NodeId>) {
    // Reparent the new children; detach any existing ones that aren't in the
    // new list. Children dropped this way are NOT removed from the slotmap
    // (the caller decides whether to remove with a separate RemoveNode);
    // this mirrors the spec that ReplaceChildren is a structural re-link,
    // not a destruction.
    let governed = scene.has_governed_node_relations();
    if governed
        && scene
            .nodes
            .get(parent)
            .is_none_or(|node| children.len() > node.children.capacity())
    {
        scene.resource_error = true;
        return;
    }
    for &child_id in &children {
        if let Some(child) = scene.nodes.get_mut(child_id) {
            child.parent = Some(parent);
        }
    }
    if let Some(parent_node) = scene.nodes.get_mut(parent) {
        if governed {
            parent_node.children.clear();
            parent_node.children.extend_from_slice(&children);
        } else {
            parent_node.children = children;
        }
    }
    mark_layout(scene, parent);
    if let Some(rect) = subtree_bbox(scene, parent) {
        scene.invalidation.mark_composite(rect);
    }
}

fn apply_set_placement(scene: &mut Scene, node: NodeId, placement: Placement) {
    let old_bbox = scene.nodes.get(node).map(|n| n.placement.bbox);
    if let Some(n) = scene.nodes.get_mut(node) {
        n.placement = placement;
    }
    // Placement change can cascade into siblings (flow advance
    // changes), so mark the subtree for re-layout.
    // `layout_pass_invalidated` consumes this.
    mark_layout(scene, node);
    if let Some(r) = old_bbox {
        scene.invalidation.mark_composite(r);
    }
    scene.invalidation.mark_composite(placement.bbox);
}

fn apply_set_transform(scene: &mut Scene, node: NodeId, transform: Transform) {
    let old_rect = scene.nodes.get(node).map(effective_rect);
    if let Some(n) = scene.nodes.get_mut(node) {
        n.transform = transform;
    }
    // Transform doesn't change layout — just re-compose at old + new rects.
    if let Some(r) = old_rect {
        scene.invalidation.mark_composite(r);
    }
    if let Some(n) = scene.nodes.get(node) {
        scene.invalidation.mark_composite(effective_rect(n));
    }
}

fn apply_set_visible(scene: &mut Scene, node: NodeId, visible: bool) {
    let rect = scene.nodes.get(node).map(|n| n.placement.bbox);
    if let Some(n) = scene.nodes.get_mut(node) {
        n.visible = visible;
    }
    if let Some(r) = rect {
        scene.invalidation.mark_composite(r);
    }
}

fn apply_set_z_index(scene: &mut Scene, node: NodeId, z_index: i16) {
    let rect = scene.nodes.get(node).map(|n| n.placement.bbox);
    if let Some(n) = scene.nodes.get_mut(node) {
        n.z_index = z_index;
    }
    if let Some(r) = rect {
        scene.invalidation.mark_composite(r);
    }
}

fn apply_set_panel_active(scene: &mut Scene, panel: NodeId, active: NodeId) {
    // Preconditions: panel is a Panel; active is in panel.states.
    let valid_active = scene
        .nodes
        .get(panel)
        .and_then(|n| {
            if let NodeKind::Panel { states, .. } = &n.kind {
                Some(states.contains(&active))
            } else {
                None
            }
        })
        .unwrap_or(false);
    if !valid_active {
        // Silent no-op: the caller asked for an invalid state flip. We
        // don't panic — patches may arrive against a slightly-stale
        // NodeId set (especially once structural patches come online),
        // and the render loop treats them as best-effort.
        return;
    }

    let panel_rect = scene.nodes.get(panel).map(|n| n.placement.bbox);
    // Also capture the incoming state's bbox — the panel's old rect
    // may be empty (hidden state has no children, placement bbox is
    // zero-area), which `DirtyRegions::add` silently drops. Marking
    // the new state's rect as well guarantees a non-empty composite
    // invalidation stamp even when flipping from a hidden state.
    let new_state_rect = scene.nodes.get(active).map(|n| n.placement.bbox);
    if let Some(n) = scene.nodes.get_mut(panel)
        && let NodeKind::Panel { active: a, .. } = &mut n.kind
    {
        *a = active;
    }
    // Layout invalidation: the panel's subtree needs re-layout because the
    // active state's content has changed. Composite invalidation: the
    // panel's whole rect needs repainting.
    mark_layout(scene, panel);
    if let Some(r) = panel_rect {
        scene.invalidation.mark_composite(r);
    }
    if let Some(r) = new_state_rect {
        scene.invalidation.mark_composite(r);
    }
}

fn apply_toggle_details(scene: &mut Scene, node: NodeId) {
    // Flip `FlowData.details_open`. Only meaningful for Details-flavored
    // Flow nodes; other kinds are silently ignored.
    let Some(n) = scene.nodes.get_mut(node) else {
        return;
    };
    if let NodeKind::Flow(data) = &mut n.kind
        && matches!(data.source, super::node::FlowSource::Details)
    {
        data.details_open = !data.details_open;
        let rect = n.placement.bbox;
        mark_layout(scene, node);
        scene.invalidation.mark_composite(rect);
    }
}

fn apply_set_focus(scene: &mut Scene, node: Option<NodeId>) {
    // Invalidate focus-outline regions for both old and new focus.
    let old_rect = scene
        .focus
        .and_then(|f| scene.nodes.get(f))
        .map(|n| n.placement.bbox);
    let new_rect = node
        .and_then(|f| scene.nodes.get(f))
        .map(|n| n.placement.bbox);
    scene.focus = node;
    if let Some(r) = old_rect {
        scene.invalidation.mark_composite(r);
    }
    if let Some(r) = new_rect {
        scene.invalidation.mark_composite(r);
    }
}

fn apply_set_scroll(scene: &mut Scene, offset: u16) {
    scene.scroll = ScrollState { offset };
    // The whole viewport needs re-composition on scroll. Use the scene
    // root's bbox as an upper bound; composite coalescing will refine.
    if let Some(r) = scene.nodes.get(scene.root).map(|n| n.placement.bbox) {
        scene.invalidation.mark_composite(r);
    }
}

// ─── Helpers ────────────────────────────────────────────────────

fn subtree_bbox(scene: &Scene, root: NodeId) -> Option<Rect> {
    scene.nodes.get(root).map(|n| n.placement.bbox)
}

/// Effective composition rect: placement.bbox shifted by the node's
/// transform. Used for transform-change invalidation.
fn effective_rect(node: &Node) -> Rect {
    if node.transform.is_identity() {
        node.placement.bbox
    } else {
        node.placement
            .bbox
            .translate(node.transform.dx as i32, node.transform.dy as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::scene::build;
    use crate::parser::parse;
    use crate::resource::{MAX_REMOTE_MEMORY, ResourceGovernor};
    use crate::scanner::Scanner;

    fn parse_aml(src: &str) -> crate::parser::ast::Document {
        let mut scanner = Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        parse(tokens).document.expect("parse failed")
    }

    fn panel_scene() -> Scene {
        let doc = parse_aml(
            r#"[page mode=document]
                [panel id="p" state="a"]
                    [state name="a"][text]A[/text][/state]
                    [state name="b"][text]B[/text][/state]
                [/panel]
            [/page]"#,
        );
        build::from_document(&doc)
    }

    fn assign_placement(scene: &mut Scene, id: NodeId, r: Rect) {
        scene.update_placement(
            id,
            Placement {
                rect: r,
                flow_advance: r.h,
                bbox: r,
            },
        );
    }

    #[test]
    fn set_panel_active_flips_active_and_invalidates() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        // Give the panel a non-trivial rect so composite invalidation is observable.
        assign_placement(&mut scene, panel_id, Rect::new(0, 0, 20, 5));

        let b_state_id = {
            let panel = scene.get(panel_id).unwrap();
            if let NodeKind::Panel { states, .. } = panel.kind() {
                *states
                    .iter()
                    .find(|&&s| scene.get(s).and_then(|n| n.aml_id()) == Some("b"))
                    .unwrap()
            } else {
                panic!();
            }
        };

        PatchApplier::apply(
            &mut scene,
            Patch::SetPanelActive {
                panel: panel_id,
                active: b_state_id,
            },
        );

        // Active flipped.
        let panel = scene.get(panel_id).unwrap();
        if let NodeKind::Panel { active, .. } = panel.kind() {
            assert_eq!(*active, b_state_id);
        } else {
            panic!();
        }
        // Invalidation populated on both channels.
        assert!(scene.invalidation.layout.contains(&panel_id));
        assert!(!scene.invalidation.composite.is_empty());
    }

    #[test]
    fn set_panel_active_rejects_unknown_state() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        let original_active =
            if let NodeKind::Panel { active, .. } = scene.get(panel_id).unwrap().kind() {
                *active
            } else {
                panic!();
            };
        // Use the panel's own id as a bogus active (not in `states`).
        PatchApplier::apply(
            &mut scene,
            Patch::SetPanelActive {
                panel: panel_id,
                active: panel_id,
            },
        );
        // Unchanged — invalid target is a silent no-op.
        if let NodeKind::Panel { active, .. } = scene.get(panel_id).unwrap().kind() {
            assert_eq!(*active, original_active);
        }
        // Nothing invalidated either.
        assert!(scene.invalidation.layout.is_empty());
    }

    #[test]
    fn set_transform_invalidates_old_and_new_composite() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        assign_placement(&mut scene, panel_id, Rect::new(5, 5, 10, 3));

        PatchApplier::apply(
            &mut scene,
            Patch::SetTransform {
                node: panel_id,
                transform: Transform { dx: 3, dy: 0 },
            },
        );

        // Transform updated.
        assert_eq!(
            scene.get(panel_id).unwrap().transform(),
            Transform { dx: 3, dy: 0 }
        );
        // No layout invalidation (transform is pure compose).
        assert!(!scene.invalidation.layout.contains(&panel_id));
        // Two composite rects (old + new), merged via DirtyRegions.
        let bbox = scene.invalidation.composite.bounding_box().unwrap();
        // Union of (5,5,10,3) and its +3x translate (8,5,10,3) is (5,5,13,3).
        assert_eq!(bbox, Rect::new(5, 5, 13, 3));
    }

    #[test]
    fn set_visible_invalidates_composite_only() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        assign_placement(&mut scene, panel_id, Rect::new(0, 0, 10, 3));

        PatchApplier::apply(
            &mut scene,
            Patch::SetVisible {
                node: panel_id,
                visible: false,
            },
        );
        assert!(!scene.get(panel_id).unwrap().visible());
        assert!(!scene.invalidation.composite.is_empty());
        assert!(scene.invalidation.layout.is_empty());
    }

    #[test]
    fn set_z_index_mutates_and_invalidates() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        assign_placement(&mut scene, panel_id, Rect::new(0, 0, 5, 2));

        PatchApplier::apply(
            &mut scene,
            Patch::SetZIndex {
                node: panel_id,
                z_index: 42,
            },
        );
        assert_eq!(scene.get(panel_id).unwrap().z_index(), 42);
        assert!(!scene.invalidation.composite.is_empty());
    }

    #[test]
    fn set_focus_updates_scene_and_invalidates_both_regions() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        assign_placement(&mut scene, panel_id, Rect::new(0, 0, 5, 2));

        PatchApplier::apply(
            &mut scene,
            Patch::SetFocus {
                node: Some(panel_id),
            },
        );
        assert_eq!(scene.focus(), Some(panel_id));
        assert!(!scene.invalidation.composite.is_empty());

        // Clear focus — old rect invalidated again.
        scene.invalidation.clear();
        PatchApplier::apply(&mut scene, Patch::SetFocus { node: None });
        assert_eq!(scene.focus(), None);
        assert!(!scene.invalidation.composite.is_empty());
    }

    #[test]
    fn set_scroll_writes_offset_and_invalidates_root() {
        let mut scene = panel_scene();
        let root = scene.root();
        assign_placement(&mut scene, root, Rect::new(0, 0, 80, 24));

        PatchApplier::apply(&mut scene, Patch::SetScroll { offset: 12 });
        assert_eq!(scene.scroll().offset, 12);
        assert!(!scene.invalidation.composite.is_empty());
    }

    #[test]
    fn set_placement_cascades_layout_and_composite() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        assign_placement(&mut scene, panel_id, Rect::new(0, 0, 10, 3));

        PatchApplier::apply(
            &mut scene,
            Patch::SetPlacement {
                node: panel_id,
                placement: Placement {
                    rect: Rect::new(5, 5, 20, 4),
                    flow_advance: 4,
                    bbox: Rect::new(5, 5, 20, 4),
                },
            },
        );
        assert_eq!(
            scene.get(panel_id).unwrap().placement().rect,
            Rect::new(5, 5, 20, 4)
        );
        assert!(scene.invalidation.layout.contains(&panel_id));
        // Two composite rects (old + new).
        let bbox = scene.invalidation.composite.bounding_box().unwrap();
        assert_eq!(bbox, Rect::new(0, 0, 25, 9));
    }

    #[test]
    fn insert_and_remove_round_trip() {
        let mut scene = panel_scene();
        let root = scene.root();
        let before = scene.node_count();

        PatchApplier::apply(
            &mut scene,
            Patch::InsertNode {
                parent: root,
                before: None,
                node: NodeTemplate {
                    kind: NodeKind::Row(crate::compositor::scene::node::RowData::default()),
                    aml_id: Some("injected".into()),
                    focusable: false,
                    hit_target: None,
                },
            },
        );
        let injected = scene.find_by_aml_id("injected").unwrap();
        assert_eq!(scene.node_count(), before + 1);
        assert!(scene.invalidation.layout.contains(&root));

        scene.invalidation.clear();
        PatchApplier::apply(&mut scene, Patch::RemoveNode { node: injected });
        assert_eq!(scene.node_count(), before);
        assert!(scene.find_by_aml_id("injected").is_none());
        assert!(scene.invalidation.layout.contains(&root));
    }

    #[test]
    fn governed_relation_overflow_rejects_before_structural_mutation() {
        let doc = parse_aml("[page][text]x[/text][/page]");
        let governor = ResourceGovernor::new();
        let mut scene = build::from_document_governed(&doc, &governor);
        let root = scene.root();
        let mut suffix = 0usize;

        while scene.get(root).unwrap().children.len() < scene.get(root).unwrap().children.capacity()
            && scene.aml_id_index.len() < scene.aml_id_index.capacity()
        {
            PatchApplier::apply(
                &mut scene,
                Patch::InsertNode {
                    parent: root,
                    before: None,
                    node: NodeTemplate {
                        kind: NodeKind::Row(crate::compositor::scene::node::RowData::default()),
                        aml_id: Some(format!("extra-{suffix}")),
                        focusable: false,
                        hit_target: None,
                    },
                },
            );
            suffix += 1;
        }

        scene.invalidation.clear();
        let nodes = scene.node_count();
        let children = scene.get(root).unwrap().children.clone();
        let index = scene.aml_id_index.clone();
        let used = governor.used(ResourceCategory::RemoteCollections);
        PatchApplier::apply(
            &mut scene,
            Patch::InsertNode {
                parent: root,
                before: None,
                node: NodeTemplate {
                    kind: NodeKind::Row(crate::compositor::scene::node::RowData::default()),
                    aml_id: Some("overflow".into()),
                    focusable: false,
                    hit_target: None,
                },
            },
        );

        assert_eq!(scene.node_count(), nodes);
        assert_eq!(scene.get(root).unwrap().children, children);
        assert_eq!(scene.aml_id_index, index);
        assert!(scene.invalidation.layout.is_empty());
        assert!(scene.invalidation.composite.is_empty());
        assert!(scene.resource_limit_exceeded());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), used);

        let oversized = vec![children[0]; scene.get(root).unwrap().children.capacity() + 1];
        PatchApplier::apply(
            &mut scene,
            Patch::ReplaceChildren {
                parent: root,
                children: oversized,
            },
        );
        assert_eq!(scene.get(root).unwrap().children, children);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), used);
    }

    #[test]
    fn duplicate_aml_id_removal_preserves_last_writer_tombstone_semantics() {
        let mut scene = panel_scene();
        let root = scene.root();
        let insert = |scene: &mut Scene| {
            PatchApplier::apply(
                scene,
                Patch::InsertNode {
                    parent: root,
                    before: None,
                    node: NodeTemplate {
                        kind: NodeKind::Row(crate::compositor::scene::node::RowData::default()),
                        aml_id: Some("duplicate".into()),
                        focusable: false,
                        hit_target: None,
                    },
                },
            );
            scene.find_by_aml_id("duplicate").unwrap()
        };

        let first = insert(&mut scene);
        let second = insert(&mut scene);
        assert_ne!(first, second);
        assert_eq!(scene.find_by_aml_id("duplicate"), Some(second));

        PatchApplier::apply(&mut scene, Patch::RemoveNode { node: first });
        assert_eq!(scene.find_by_aml_id("duplicate"), Some(second));

        PatchApplier::apply(&mut scene, Patch::RemoveNode { node: second });
        assert_eq!(scene.find_by_aml_id("duplicate"), None);
    }

    #[test]
    fn remove_preadmission_failure_preserves_scene_and_accounting() {
        let doc = parse_aml(
            r#"[page mode=document]
                [panel id="p" state="a"]
                    [state name="a"][box][text]remote subtree[/text][/box][/state]
                [/panel]
            [/page]"#,
        );
        let governor = ResourceGovernor::new();
        let mut scene = build::from_document_governed(&doc, &governor);
        let panel = scene.find_by_aml_id("p").unwrap();
        scene.invalidation.clear();
        let requested = scene.node_count() * std::mem::size_of::<NodeId>();
        let baseline = governor.used(ResourceCategory::RemoteCollections);
        let relation_before = scene.node_relation_admission_bytes();
        let available = MAX_REMOTE_MEMORY - baseline;
        assert!(requested > 0 && available >= requested);
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                available - requested + 1,
            )
            .unwrap();
        let before = governor.used(ResourceCategory::RemoteCollections);

        PatchApplier::apply(&mut scene, Patch::RemoveNode { node: panel });

        assert!(scene.get(panel).is_some());
        assert!(scene.invalidation.layout.is_empty());
        assert!(scene.invalidation.composite.is_empty());
        assert!(scene.resource_limit_exceeded());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);

        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), baseline);

        PatchApplier::apply(&mut scene, Patch::RemoveNode { node: panel });
        assert!(scene.get(panel).is_none());
        let expected = baseline - relation_before + scene.node_relation_admission_bytes();
        assert!(expected < baseline);
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            expected,
            "successful traversal must release its temporary lease and removed child backing"
        );
    }

    #[test]
    fn replace_children_relinks_and_invalidates() {
        let mut scene = panel_scene();
        let root = scene.root();

        // Insert two new nodes, then replace-children to reorder them.
        PatchApplier::apply(
            &mut scene,
            Patch::InsertNode {
                parent: root,
                before: None,
                node: NodeTemplate {
                    kind: NodeKind::Row(crate::compositor::scene::node::RowData::default()),
                    aml_id: Some("r1".into()),
                    focusable: false,
                    hit_target: None,
                },
            },
        );
        PatchApplier::apply(
            &mut scene,
            Patch::InsertNode {
                parent: root,
                before: None,
                node: NodeTemplate {
                    kind: NodeKind::Row(crate::compositor::scene::node::RowData::default()),
                    aml_id: Some("r2".into()),
                    focusable: false,
                    hit_target: None,
                },
            },
        );
        let r1 = scene.find_by_aml_id("r1").unwrap();
        let r2 = scene.find_by_aml_id("r2").unwrap();

        scene.invalidation.clear();
        PatchApplier::apply(
            &mut scene,
            Patch::ReplaceChildren {
                parent: root,
                children: vec![r2, r1],
            },
        );

        let root_node = scene.get(root).unwrap();
        assert_eq!(root_node.children(), &[r2, r1]);
        assert!(scene.invalidation.layout.contains(&root));
    }

    #[test]
    fn apply_all_preserves_arrival_order() {
        let mut scene = panel_scene();
        let panel_id = scene.find_by_aml_id("p").unwrap();
        assign_placement(&mut scene, panel_id, Rect::new(0, 0, 5, 2));

        PatchApplier::apply_all(
            &mut scene,
            vec![
                Patch::SetZIndex {
                    node: panel_id,
                    z_index: 1,
                },
                Patch::SetZIndex {
                    node: panel_id,
                    z_index: 2,
                },
                Patch::SetZIndex {
                    node: panel_id,
                    z_index: 3,
                },
            ],
        );
        assert_eq!(scene.get(panel_id).unwrap().z_index(), 3);
    }
}
