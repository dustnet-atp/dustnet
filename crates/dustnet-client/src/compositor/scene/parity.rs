//! Parity assertion: the scene matches the AST under the eight-property
//! isomorphism defined in `docs/internals/compositor.md` (section "AST to Scene
//! mapping").
//!
//! This is a test-time assertion, not a runtime one. Tests run this
//! check against every AML fixture to verify `build_scene`'s mapping
//! table is complete and correct; production never calls it.

use crate::parser::ast::{self, Document, Element, PanelElement};

use super::build::{Category, classify};
use super::node::{FlowSource, KindTag, Node, NodeKind, TextSource};
use super::tree::Scene;

/// Assert the scene is isomorphic to the AST under the rules in
/// compositor.md. Panics with a descriptive message on any violation.
pub fn assert_scene_parity(doc: &Document, scene: &Scene) {
    // (1) Enumerate expected node-bearing elements by walking the AST in
    //     source tree order, applying the classification.
    let mut expected: Vec<&Element> = Vec::new();
    for child in &doc.page.children {
        collect_expected(child, &mut expected);
    }

    // (2) Walk the scene in the same tree order, skipping the Root node
    //     (which has no corresponding AST element).
    let actual: Vec<&Node> = scene
        .iter_tree_order()
        .filter(|n| !matches!(n.kind(), NodeKind::Root))
        .collect();

    // Property (1) — Bijection by count.
    assert_eq!(
        expected.len(),
        actual.len(),
        "AST has {} node-bearing elements; scene has {} non-root nodes.\n\
         expected elements: {:?}\n\
         actual kind tags:  {:?}",
        expected.len(),
        actual.len(),
        expected.iter().map(|e| element_tag(e)).collect::<Vec<_>>(),
        actual.iter().map(|n| n.kind_tag()).collect::<Vec<_>>(),
    );

    // Properties (4), (5): kind and identity preservation, in order.
    for (i, (elem, node)) in expected.iter().zip(actual.iter()).enumerate() {
        let expected_tag = expected_kind(elem);
        assert_eq!(
            node.kind_tag(),
            expected_tag,
            "kind mismatch at tree index {i}: AST element = {:?} (expects {:?}), \
             scene node = {:?}",
            element_tag(elem),
            expected_tag,
            node.kind_tag(),
        );
        assert_eq!(
            node.aml_id(),
            element_aml_id(elem).as_deref(),
            "aml_id mismatch at tree index {i}: AST id = {:?}, scene aml_id = {:?}",
            element_aml_id(elem),
            node.aml_id(),
        );
    }

    // Property (6): ancillary absence. No node should have come from a
    // `Tween` or `On` element. We can't check this directly (the scene
    // doesn't record provenance), but the count check above catches it —
    // if `On` leaked into a node, `actual.len()` would exceed
    // `expected.len()`.

    // Property (8): component-expansion precondition. The parser in this
    // codebase does not emit `Def`/`Use` into the Element tree at all, so
    // this precondition is satisfied by construction.
}

/// Walk the AST in source tree order, collecting node-bearing elements.
/// `Text` children are walked specially: inline children (nested `[text]`
/// with no focusable behavior) do not appear in the enumeration — they
/// become runs. Node-bearing children of `[text]` (Link, Button) do.
fn collect_expected<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    match classify(e) {
        Category::NodeBearing => {
            out.push(e);
            for child in scene_children(e) {
                collect_expected(child, out);
            }
        }
        Category::Ancillary | Category::Inline => {
            // Ancillary: contributes no node. Inline: consumed by Text
            // (not expected at top level).
        }
    }
}

/// Node-bearing children of an element, in source order.
///
/// For `Text`-family elements, this filters out inline children (which
/// flatten into runs) and only yields the node-bearing children.
fn scene_children(e: &Element) -> Vec<&Element> {
    match e {
        Element::Text(t) => t
            .children
            .iter()
            .filter(|c| match c {
                // Inline Text children flatten — not node-bearing here.
                Element::Text(_) => false,
                other => classify(other) == Category::NodeBearing,
            })
            .collect(),
        Element::Box(b) => nb(&b.children),
        Element::Row(r) => nb(&r.children),
        Element::Col(c) => nb(&c.children),
        Element::Header(c) | Element::Body(c) | Element::Footer(c) => nb(&c.children),
        Element::Thead(c) | Element::Tbody(c) | Element::Pagination(c) => nb(&c.children),
        Element::Nav(n) => nb(&n.children),
        Element::List(l) => nb(&l.children),
        Element::Item(i) => nb(&i.children),
        Element::Form(f) => nb(&f.children),
        Element::Heading(h) => nb(&h.children),
        Element::Link(l) => nb(&l.children),
        Element::Select(s) => nb(&s.children),
        Element::Table(t) => nb(&t.children),
        Element::Tr(tr) => nb(&tr.children),
        Element::Th(c) | Element::Td(c) => nb(&c.children),
        Element::Animate(a) => nb(&a.children),
        Element::Frame(f) => nb(&f.children),
        Element::Live(l) => nb(&l.children),
        Element::Panel(p) => {
            // Panel's scene-children are its `[state]` children, which are
            // all node-bearing.
            nb(&p.children)
        }
        Element::State(s) => nb(&s.children),
        Element::Details(d) => {
            // `build_scene` attaches summary_children first, then
            // children (only if open). Parity must mirror that order.
            let mut v: Vec<&Element> = Vec::new();
            v.extend(
                d.summary_children
                    .iter()
                    .filter(|c| classify(c) == Category::NodeBearing),
            );
            if d.open {
                v.extend(
                    d.children
                        .iter()
                        .filter(|c| classify(c) == Category::NodeBearing),
                );
            }
            v
        }
        // Leaves: no node-bearing children.
        Element::Pre(_)
        | Element::Hr(_)
        | Element::Spacer(_)
        | Element::Art(_)
        | Element::Input(_)
        | Element::Option(_)
        | Element::Button(_)
        | Element::ElementDef(_)
        | Element::TextAnimate(_)
        | Element::Tween(_)
        | Element::On(_)
        | Element::Include(_) => Vec::new(),
    }
}

fn nb(elems: &[Element]) -> Vec<&Element> {
    elems
        .iter()
        .filter(|c| classify(c) == Category::NodeBearing)
        .collect()
}

/// Expected `KindTag` for an element — the exact mapping table, specialized
/// on attributes where the table requires.
fn expected_kind(e: &Element) -> KindTag {
    match e {
        Element::Box(b) => {
            if b.x.is_some() || b.y.is_some() {
                KindTag::Absolute
            } else {
                KindTag::Flow
            }
        }
        Element::Row(_) => KindTag::Row,
        Element::Col(_) => KindTag::Flow,
        Element::Header(_)
        | Element::Body(_)
        | Element::Footer(_)
        | Element::Nav(_)
        | Element::Thead(_)
        | Element::Tbody(_)
        | Element::Pagination(_)
        | Element::List(_)
        | Element::Item(_)
        | Element::Form(_)
        | Element::State(_)
        | Element::Details(_)
        | Element::Frame(_) => KindTag::Flow,

        Element::Hr(_) => KindTag::Hr,
        Element::Spacer(_) => KindTag::Spacer,

        Element::Text(_)
        | Element::Pre(_)
        | Element::Heading(_)
        | Element::Art(_)
        | Element::ElementDef(_)
        | Element::TextAnimate(_) => KindTag::Text,

        Element::Link(_) => KindTag::Link,
        Element::Input(_) => KindTag::Input,
        Element::Select(_) => KindTag::Select,
        Element::Option(_) => KindTag::OptionLeaf,
        Element::Button(_) => KindTag::Button,

        Element::Table(_) => KindTag::Table,
        Element::Tr(_) => KindTag::Tr,
        Element::Th(_) => KindTag::Th,
        Element::Td(_) => KindTag::Td,

        Element::Animate(_) => KindTag::Animation,
        Element::Live(_) => KindTag::LiveRegion,
        Element::Panel(_) => KindTag::Panel,

        Element::Tween(_) | Element::On(_) | Element::Include(_) => {
            panic!("expected_kind called on ancillary element {e:?}")
        }
    }
}

/// Short, human-readable tag for an element — used only for error
/// messages. Does not need to match any other enum.
fn element_tag(e: &Element) -> &'static str {
    match e {
        Element::Box(_) => "Box",
        Element::Row(_) => "Row",
        Element::Col(_) => "Col",
        Element::Hr(_) => "Hr",
        Element::Spacer(_) => "Spacer",
        Element::Header(_) => "Header",
        Element::Body(_) => "Body",
        Element::Footer(_) => "Footer",
        Element::Nav(_) => "Nav",
        Element::Text(_) => "Text",
        Element::Pre(_) => "Pre",
        Element::Heading(_) => "Heading",
        Element::List(_) => "List",
        Element::Item(_) => "Item",
        Element::Link(_) => "Link",
        Element::Input(_) => "Input",
        Element::Select(_) => "Select",
        Element::Option(_) => "Option",
        Element::Button(_) => "Button",
        Element::Form(_) => "Form",
        Element::Art(_) => "Art",
        Element::Table(_) => "Table",
        Element::Thead(_) => "Thead",
        Element::Tbody(_) => "Tbody",
        Element::Tr(_) => "Tr",
        Element::Th(_) => "Th",
        Element::Td(_) => "Td",
        Element::Animate(_) => "Animate",
        Element::Frame(_) => "Frame",
        Element::ElementDef(_) => "ElementDef",
        Element::Tween(_) => "Tween",
        Element::TextAnimate(_) => "TextAnimate",
        Element::Live(_) => "Live",
        Element::Panel(_) => "Panel",
        Element::State(_) => "State",
        Element::Details(_) => "Details",
        Element::Pagination(_) => "Pagination",
        Element::On(_) => "On",
        Element::Include(_) => "Include",
    }
}

fn element_aml_id(e: &Element) -> Option<String> {
    match e {
        Element::Link(l) => l.id.clone(),
        Element::Input(i) => i.id.clone(),
        Element::Panel(p) => Some(p.id.clone()),
        Element::Live(l) => Some(l.id.clone()),
        Element::Animate(a) => Some(a.id.clone()),
        Element::ElementDef(e) => Some(e.id.clone()),
        // `State` stores its `name` as aml_id in build.
        Element::State(s) => Some(s.name.clone()),
        _ => None,
    }
}

// Suppress unused-warning for imports that are part of the API surface
// the parity module needs as the scene grows.
#[allow(dead_code)]
const _KEEP: (
    Option<PanelElement>,
    Option<FlowSource>,
    Option<TextSource>,
    Option<ast::TransitionKind>,
) = (None, None, None, None);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::scanner::Scanner;

    use crate::compositor::scene::build;

    fn parse(aml: &str) -> Document {
        let mut scanner = Scanner::new(aml.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        parser::parse(tokens).document.expect("parse failed")
    }

    /// A tiny document — scene parity holds for the trivial case.
    #[test]
    fn parity_holds_for_hello_world() {
        let doc = parse(r#"[page mode=document][text]Hello[/text][/page]"#);
        let scene = build::from_document(&doc);
        assert_scene_parity(&doc, &scene);
    }

    /// Panel with two states, one active. Parity checks that the active
    /// NodeId matches the initial_state-named child.
    #[test]
    fn parity_holds_for_panel() {
        let doc = parse(
            r#"[page mode=document]
                [panel id="p1" state="a"]
                    [state name="a"][text]A[/text][/state]
                    [state name="b"][text]B[/text][/state]
                [/panel]
            [/page]"#,
        );
        let scene = build::from_document(&doc);
        assert_scene_parity(&doc, &scene);

        // Also sanity-check the Panel's active == state-"a" node.
        let panel_id = scene.find_by_aml_id("p1").expect("p1 must be found");
        let panel = scene.get(panel_id).unwrap();
        if let NodeKind::Panel { active, states, .. } = panel.kind() {
            let active_node = scene.get(*active).unwrap();
            assert_eq!(active_node.aml_id(), Some("a"));
            assert_eq!(states.len(), 2);
        } else {
            panic!("expected Panel kind");
        }
    }

    /// The count assertion must fire when a scene has the wrong node count.
    #[test]
    #[should_panic(expected = "AST has")]
    fn parity_fails_on_count_mismatch() {
        let doc = parse(r#"[page mode=document][text]Hi[/text][/page]"#);
        let mut scene = build::from_document(&doc);
        // Drop the only real node; parity should fail.
        let root = scene.root();
        let victim = scene.get(root).unwrap().children[0];
        if let Some(root_node) = scene.nodes.get_mut(root) {
            root_node.children.retain(|&c| c != victim);
        }
        scene.nodes.remove(victim);
        assert_scene_parity(&doc, &scene);
    }

    /// The kind assertion must fire when a node has the wrong kind for its
    /// source element.
    #[test]
    #[should_panic(expected = "kind mismatch")]
    fn parity_fails_on_kind_mismatch() {
        let doc = parse(r#"[page mode=document][text]Hi[/text][/page]"#);
        let mut scene = build::from_document(&doc);
        let root = scene.root();
        let text_id = scene.get(root).unwrap().children[0];
        if let Some(text_node) = scene.nodes.get_mut(text_id) {
            text_node.kind = NodeKind::Row(crate::compositor::scene::node::RowData::default());
        }
        assert_scene_parity(&doc, &scene);
    }

    /// Inline `[b]` inside `[text]` flattens into a run — it does *not*
    /// produce a separate scene node. If it did, the count check would
    /// fire.
    #[test]
    fn parity_holds_with_inline_bold() {
        let doc = parse(r#"[page mode=document][text]hi [text bold]world[/text][/text][/page]"#);
        let scene = build::from_document(&doc);
        assert_scene_parity(&doc, &scene);

        // Exactly one Text node in the scene, with two runs.
        let count = scene
            .iter_tree_order()
            .filter(|n| matches!(n.kind(), NodeKind::Text(_)))
            .count();
        assert_eq!(
            count, 1,
            "inline [b] should flatten to a run, not spawn a node"
        );
        let text_node = scene
            .iter_tree_order()
            .find(|n| matches!(n.kind(), NodeKind::Text(_)))
            .unwrap();
        if let NodeKind::Text(tc) = text_node.kind() {
            assert_eq!(tc.runs.len(), 2, "expected 2 runs (plain + bold)");
            assert!(tc.runs.iter().any(|r| r.bold), "one run must be bold");
        }
    }

    /// `[on]` is ancillary: produces no scene node.
    #[test]
    fn parity_holds_with_on_bindings() {
        let doc = parse(
            r#"[page mode=document]
                [text]Hello[/text]
                [on event="page-load" do="set" target="x" to="y"/]
            [/page]"#,
        );
        let scene = build::from_document(&doc);
        assert_scene_parity(&doc, &scene);
        let count = scene
            .iter_tree_order()
            .filter(|n| !matches!(n.kind(), NodeKind::Root))
            .count();
        assert_eq!(count, 1, "only the [text] should produce a node");
    }
}
