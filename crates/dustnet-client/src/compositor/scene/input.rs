//! Hit-testing and focus traversal over the scene graph.
//!
//! Scene-native input operations that the render-loop's click/key
//! handlers use to translate coordinates into `NodeId`s and to walk
//! focusable siblings via Tab/Shift-Tab.
//!
//! Uses `Node.placement.rect` as the hit-test coordinate space (see
//! compositor.md "Placement") — it already carries buffer-absolute
//! coordinates after layout runs, so no separate screen-space cache
//! is needed.

use super::{Node, NodeId, NodeKind, Scene};

impl Scene {
    /// Return the deepest node whose `placement.rect` contains the
    /// point `(x, y)`. `None` if no node covers the point.
    ///
    /// "Deepest" = most distant from the root in the tree. Walks
    /// top-down and picks the most-nested match. Ties broken by
    /// insertion order: later children win (they stack on top).
    ///
    /// Hit-testing uses `placement.rect`, which carries
    /// buffer-absolute coordinates after `hydrate_scene_buffers`.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<NodeId> {
        self.hit_test_in_subtree(self.root, x, y)
    }

    /// Hit-test within a subtree rooted at `id`. Used internally and
    /// exposed for future compositor-integrated hit routing (e.g. a
    /// modal dialog intercepts clicks within its subtree).
    pub fn hit_test_in_subtree(&self, id: NodeId, x: u16, y: u16) -> Option<NodeId> {
        let node = self.get(id)?;
        // Overlay nodes are system-synthesized and never capture input;
        // a page-transition overlay must not swallow keystrokes that
        // should reach the underlying focus target. Skip the overlay
        // subtree entirely — overlays have no children today, but the
        // early return keeps future overlay kinds (debug, capture)
        // non-interactive by default.
        if matches!(node.kind(), NodeKind::Overlay(_)) {
            return None;
        }
        // Check children first — deepest wins. Iterate in reverse so
        // a later sibling (stacked on top) is preferred on overlap.
        for &child in node.children().iter().rev() {
            if let Some(hit) = self.hit_test_in_subtree(child, x, y) {
                return Some(hit);
            }
        }
        // No child hit; does this node cover the point?
        if contains(node, x, y) { Some(id) } else { None }
    }

    /// Walk tree-order and return the id of every focusable node. Used
    /// by focus traversal and by tests that want to reason about Tab
    /// order.
    pub fn focusable_tree_order(&self) -> Vec<NodeId> {
        self.iter_tree_order()
            .filter(|n| n.focusable())
            .map(|n| n.id())
            .collect()
    }

    /// Next focusable node after `current` in tree order, wrapping to
    /// the first focusable if `current` is `None` or at the end.
    pub fn focus_next(&self, current: Option<NodeId>) -> Option<NodeId> {
        let order = self.focusable_tree_order();
        // `first` subsumes the emptiness check: there is no wrap target if
        // there is no first element.
        let &first = order.first()?;
        let Some(id) = current else {
            return Some(first);
        };
        match order.iter().position(|&f| f == id) {
            Some(index) => order.get((index + 1) % order.len()).copied(),
            None => Some(first),
        }
    }

    /// Previous focusable in tree order. Wraps to the last when
    /// `current` is `None` or at position 0.
    pub fn focus_prev(&self, current: Option<NodeId>) -> Option<NodeId> {
        let order = self.focusable_tree_order();
        let &last = order.last()?;
        let Some(id) = current else { return Some(last) };
        match order.iter().position(|&f| f == id) {
            Some(0) | None => Some(last),
            Some(index) => order.get(index - 1).copied(),
        }
    }
}

fn contains(node: &Node, x: u16, y: u16) -> bool {
    let r = node.placement().rect;
    r.contains_point(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::layout::Rect;
    use crate::compositor::layout::engine::Placement;
    use crate::compositor::scene::{self, NodeKind, Patch, PatchApplier};

    fn panel_scene() -> Scene {
        let doc = {
            let src = r#"[page mode=document]
                [panel id="p" state="a"]
                    [state name="a"][link href="atp://a"][text]A[/text][/link][/state]
                    [state name="b"][button action=submit]B[/button][/state]
                [/panel]
                [link href="atp://c"][text]C[/text][/link]
            [/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        scene::build::from_document(&doc)
    }

    /// Assign placements to nodes so hit_test has something to work with.
    /// `build_scene` leaves placements empty; layout would populate them,
    /// but the tests can do it directly via the patch channel.
    fn assign_rect(scene: &mut Scene, id: NodeId, r: Rect) {
        PatchApplier::apply(
            scene,
            Patch::SetPlacement {
                node: id,
                placement: Placement {
                    rect: r,
                    flow_advance: r.h,
                    bbox: r,
                },
            },
        );
    }

    #[test]
    fn hit_test_returns_none_for_empty_scene() {
        let scene = panel_scene();
        // No placements assigned — every rect is (0, 0, 0, 0) which
        // contains_point returns false. (Scene root has no placement
        // either, so hit_test returns None.)
        assert_eq!(scene.hit_test(5, 5), None);
    }

    #[test]
    fn hit_test_picks_deepest_containing_node() {
        let mut scene = panel_scene();
        // Give the root a covering rect and the panel a smaller one
        // inside. A point in both should return the panel (deeper).
        let root = scene.root();
        let panel = scene.find_by_aml_id("p").unwrap();
        assign_rect(&mut scene, root, Rect::new(0, 0, 80, 24));
        assign_rect(&mut scene, panel, Rect::new(2, 2, 10, 5));

        assert_eq!(scene.hit_test(5, 3), Some(panel), "point in panel → panel");
        assert_eq!(
            scene.hit_test(20, 15),
            Some(root),
            "point outside panel, in root"
        );
        assert_eq!(scene.hit_test(90, 30), None, "point outside everything");
    }

    #[test]
    fn focusable_tree_order_finds_all_focusables() {
        let scene = panel_scene();
        let order = scene.focusable_tree_order();
        // Two links + one button = 3 focusables. Order is document-
        // source order: link-A (in panel state a), button-B (in panel
        // state b), link-C (outside panel).
        assert_eq!(
            order.len(),
            3,
            "expected 3 focusables, got {}: {:?}",
            order.len(),
            order
        );
    }

    #[test]
    fn focus_next_wraps_to_first_from_last() {
        let scene = panel_scene();
        let order = scene.focusable_tree_order();
        assert!(order.len() >= 2);
        let last = *order.last().unwrap();
        assert_eq!(scene.focus_next(Some(last)), Some(order[0]));
    }

    #[test]
    fn focus_prev_wraps_to_last_from_first() {
        let scene = panel_scene();
        let order = scene.focusable_tree_order();
        assert!(order.len() >= 2);
        assert_eq!(scene.focus_prev(Some(order[0])), order.last().copied());
    }

    #[test]
    fn focus_next_from_none_is_first() {
        let scene = panel_scene();
        let order = scene.focusable_tree_order();
        assert_eq!(scene.focus_next(None), order.first().copied());
    }

    #[test]
    fn focus_prev_from_none_is_last() {
        let scene = panel_scene();
        let order = scene.focusable_tree_order();
        assert_eq!(scene.focus_prev(None), order.last().copied());
    }

    /// `hit_test` and `focus_*` combine: click on a focusable's rect
    /// returns its id, which a caller can feed into `Patch::SetFocus`
    /// to move focus to it.
    #[test]
    fn hit_test_on_link_returns_link_id() {
        let mut scene = panel_scene();
        let root = scene.root();
        let link_c = scene
            .iter_tree_order()
            .find(|n| {
                matches!(n.kind(), NodeKind::Link(_))
                    && n.hit_target().is_some()
                    && n.id()
                        != scene
                            .iter_tree_order()
                            .find(|m| matches!(m.kind(), NodeKind::Link(_)))
                            .unwrap()
                            .id()
            })
            .map(|n| n.id())
            .expect("should find second link");
        assign_rect(&mut scene, root, Rect::new(0, 0, 80, 24));
        assign_rect(&mut scene, link_c, Rect::new(0, 10, 5, 1));
        assert_eq!(scene.hit_test(2, 10), Some(link_c));
    }
}
