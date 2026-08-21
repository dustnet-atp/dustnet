//! Fixture tests: every representative AML document round-trips through
//! `build_scene` and passes the parity assertion, proving the scene
//! builder handles every element-combination that appears in the
//! the test fixtures.

use crate::compositor::scene::{
    build, parity::assert_scene_parity, tree::validate_scene_structure,
};
use crate::parser::parse;
use crate::resource::{MAX_REMOTE_MEMORY, ResourceCategory, ResourceGovernor};
use crate::scanner::Scanner;

fn parse_file(path: &str) -> crate::parser::ast::Document {
    let src = std::fs::read_to_string(crate::repository_root().join(path)).unwrap_or_else(|e| {
        panic!("failed to read {path}: {e}");
    });
    let mut scanner = Scanner::new(src.as_bytes()).unwrap_or_else(|e| {
        panic!("scan failed for {path}: {e:?}");
    });
    let tokens = scanner.scan_all().unwrap_or_else(|e| {
        panic!("scan_all failed for {path}: {e:?}");
    });
    let result = parse(tokens);
    result
        .document
        .unwrap_or_else(|| panic!("parse failed for {path}: {:?}", result.diagnostics))
}

fn check_fixture(path: &str) {
    let doc = parse_file(path);
    let scene = build::from_document(&doc);
    validate_scene_structure(&scene)
        .unwrap_or_else(|e| panic!("scene structure invariant failed for {path}: {e}"));
    assert_scene_parity(&doc, &scene);
    // Dump is deterministic and non-empty — serves as a smoke assertion
    // that every kind has a dump formatter.
    let dump = scene.debug_dump();
    assert!(
        !dump.is_empty(),
        "scene dump should not be empty for {path}"
    );
}

#[test]
fn hello_aml() {
    check_fixture("tests/fixtures/aml/hello.aml");
}

#[test]
fn kitchen_sink_aml() {
    check_fixture("tests/fixtures/aml/kitchen-sink.aml");
}

#[test]
fn panels_aml() {
    check_fixture("tests/fixtures/aml/panels.aml");
}

#[test]
fn animation_aml() {
    check_fixture("tests/fixtures/aml/animation.aml");
}

#[test]
fn retained_scene_strings_are_nonzero_and_stable() {
    let doc = parse_file("tests/fixtures/aml/kitchen-sink.aml");
    let scene = build::from_document(&doc);
    let retained = scene.retained_string_capacity();
    assert!(retained > 0);
    assert_eq!(retained, scene.retained_string_capacity());
}

#[test]
fn feed_aml() {
    check_fixture("tests/fixtures/aml/feed.aml");
}

#[test]
fn splash_aml() {
    check_fixture("tests/fixtures/aml/splash.aml");
}

#[test]
fn matrix_aml() {
    check_fixture("tests/fixtures/aml/matrix.aml");
}

#[test]
fn transitions_demo_aml() {
    check_fixture("tests/fixtures/aml/transitions-demo.aml");
}

#[test]
fn events_aml() {
    check_fixture("tests/fixtures/aml/events.aml");
}

#[test]
fn event_binding_topology_is_fixed_and_overflow_fails_closed() {
    let doc = parse_file("tests/fixtures/aml/events.aml");
    let scene = build::from_document(&doc);
    assert!(!scene.event_bindings.is_empty());
    assert_eq!(
        scene.event_bindings.capacity(),
        crate::compositor::scene::events::MAX_EVENT_BINDINGS,
    );

    let bindings = (0..=crate::compositor::scene::events::MAX_EVENT_BINDINGS)
        .map(|index| format!(r#"[on event="page-load" do="animate" target="target-{index}" /]"#))
        .collect::<String>();
    let source = format!(r#"[page mode="document"]{bindings}[/page]"#);
    let mut scanner = Scanner::new(source.as_bytes()).unwrap();
    let tokens = scanner.scan_all().unwrap();
    let document = parse(tokens).document.unwrap();
    let rejected = build::from_document(&document);
    assert!(rejected.resource_limit_exceeded());
    assert!(rejected.event_bindings.is_empty());
}

#[test]
fn components_aml() {
    check_fixture("tests/fixtures/aml/components.aml");
}

#[test]
fn panels_with_components_aml() {
    check_fixture("tests/fixtures/aml/panels-with-components.aml");
}

#[test]
fn aggregate_scene_budget_rejects_before_allocation() {
    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let mut scene = build::from_document(&doc);
    let ids: Vec<_> = scene
        .iter_tree_order()
        .filter(|node| !matches!(node.kind(), crate::compositor::scene::NodeKind::Root))
        .take(3)
        .map(|node| node.id())
        .collect();
    assert_eq!(ids.len(), 3);

    scene.allocate_buffer(ids[0], 512, 1024);
    scene.allocate_buffer(ids[1], 512, 1024);
    assert_eq!(
        scene.buffer_cell_count(),
        crate::compositor::scene::tree::MAX_SCENE_CELLS
    );
    scene.allocate_buffer(ids[2], 1, 1);
    assert!(scene.resource_limit_exceeded());
    assert!(scene.buffer_of(ids[2]).is_none());
}

#[test]
fn scene_layout_invalidation_is_preadmitted_and_released() {
    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let governor = ResourceGovernor::new();
    let mut scene = build::from_document_governed(&doc, &governor);
    let ids: Vec<_> = scene.iter_tree_order().map(|node| node.id()).collect();
    let capacity = scene.invalidation.layout.capacity();
    let retained = scene.invalidation.layout.retained_bytes();
    let relation_retained = scene.node_relation_admission_bytes();
    let node_retained = scene.node_topology_admission_bytes();

    assert!(capacity >= ids.len());
    assert_eq!(retained, capacity * std::mem::size_of_val(&ids[0]));
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        retained + relation_retained + node_retained,
    );

    for id in &ids {
        assert!(scene.invalidation.layout.insert(*id));
    }
    assert_eq!(scene.invalidation.layout.len(), ids.len());
    assert_eq!(scene.invalidation.layout.capacity(), capacity);
    assert!(scene.invalidation.layout.insert(ids[0]));
    assert_eq!(scene.invalidation.layout.len(), ids.len());

    scene.invalidation.layout.clear();
    for id in ids.iter().rev() {
        assert!(scene.invalidation.layout.insert(*id));
    }
    assert_eq!(scene.invalidation.layout.capacity(), capacity);
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        retained + relation_retained + node_retained,
    );

    drop(scene);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
}

#[test]
fn scene_node_relations_account_actual_capacities_and_release() {
    let src = "[page][box][text]a[/text][link id=next href=/next]b[/link][/box][panel id=p initial=one][state name=one][text]c[/text][/state][/panel][/page]";
    let mut scanner = Scanner::new(src.as_bytes()).unwrap();
    let tokens = scanner.scan_all().unwrap();
    let doc = parse(tokens).document.unwrap();
    let governor = ResourceGovernor::new();
    let scene = build::from_document_governed(&doc, &governor);

    assert!(!scene.resource_limit_exceeded());
    let (index_len, index_capacity, child_len, child_capacity) =
        scene.node_relation_topology_stats();
    assert_eq!(index_len, 3);
    assert_eq!(child_len + 1, scene.node_count());
    assert!(index_capacity >= index_len);
    assert!(child_capacity > child_len);
    let expected =
        (index_capacity + child_capacity) * std::mem::size_of::<crate::compositor::scene::NodeId>();
    assert_eq!(scene.node_relation_capacity_bytes(), Some(expected));
    assert_eq!(scene.node_relation_admission_bytes(), expected);
    let node_expected = scene.node_topology_capacity_bytes().unwrap();
    assert_eq!(scene.node_topology_admission_bytes(), node_expected);
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        expected + scene.layout_invalidation_admission_bytes() + node_expected,
    );

    drop(scene);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
}

#[test]
fn scene_aml_id_copy_rejection_discards_the_candidate_and_retries() {
    let src = "[page][element id=remote]value[/element][/page]";
    let mut scanner = Scanner::new(src.as_bytes()).unwrap();
    let tokens = scanner.scan_all().unwrap();
    let doc = parse(tokens).document.unwrap();
    let governor = ResourceGovernor::new();

    build::reject_next_aml_id_copy();
    let rejected = build::from_document_governed(&doc, &governor);
    assert!(rejected.resource_limit_exceeded());
    assert!(rejected.find_by_aml_id("remote").is_none());
    drop(rejected);
    assert_eq!(governor.total_used(), 0);

    let accepted = build::from_document_governed(&doc, &governor);
    assert!(!accepted.resource_limit_exceeded());
    assert!(accepted.find_by_aml_id("remote").is_some());
    drop(accepted);
    assert_eq!(governor.total_used(), 0);
}

#[test]
fn scene_node_topology_governor_and_allocator_rejection_discard_candidates() {
    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let governor = ResourceGovernor::new();

    super::tree::reject_next_node_arena_allocation();
    let blocker = governor
        .reserve(
            ResourceCategory::RemoteCollections,
            crate::resource::MAX_REMOTE_MEMORY,
        )
        .unwrap();
    let rejected = build::from_document_governed(&doc, &governor);
    assert!(rejected.resource_limit_exceeded());
    assert_eq!(rejected.node_count(), 0);
    assert_eq!(rejected.node_topology_admission_bytes(), 0);
    drop(rejected);
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        crate::resource::MAX_REMOTE_MEMORY,
    );
    drop(blocker);
    assert_eq!(governor.total_used(), 0);

    // Governor rejection precedes arena allocation, so the one-shot allocator
    // hook remains armed for the exact retry.
    let rejected = build::from_document_governed(&doc, &governor);
    assert!(rejected.resource_limit_exceeded());
    assert_eq!(rejected.node_count(), 0);
    assert_eq!(rejected.node_topology_admission_bytes(), 0);
    drop(rejected);
    assert_eq!(governor.total_used(), 0);

    let accepted = build::from_document_governed(&doc, &governor);
    assert!(!accepted.resource_limit_exceeded());
    assert!(accepted.node_count() > 0);
    assert_eq!(
        accepted.node_topology_capacity_bytes(),
        Some(accepted.node_topology_admission_bytes()),
    );
    drop(accepted);
    assert_eq!(governor.total_used(), 0);
}

#[test]
fn scene_layout_invalidation_pressure_rejects_candidate_without_leak() {
    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let governor = ResourceGovernor::new();
    let blocker = governor
        .reserve(ResourceCategory::RemoteCollections, MAX_REMOTE_MEMORY)
        .unwrap();

    let scene = build::from_document_governed(&doc, &governor);

    assert!(scene.resource_limit_exceeded());
    assert_eq!(scene.invalidation.layout.capacity(), 0);
    assert_eq!(scene.invalidation.layout.retained_bytes(), 0);
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        blocker.amount(),
    );
    drop(scene);
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        blocker.amount(),
    );
    drop(blocker);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
}

#[test]
fn relayout_journal_accounts_actual_capacities_and_releases() {
    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let governor = ResourceGovernor::new();
    let mut scene = build::from_document_governed(&doc, &governor);
    let id = scene
        .iter_tree_order()
        .find(|node| node.id() != scene.root())
        .unwrap()
        .id();
    assert!(scene.invalidation.layout.insert(id));
    let dirty = crate::compositor::layout::Rect::new(1, 2, 3, 4);
    scene.invalidation.mark_composite(dirty);
    let baseline = governor.used(ResourceCategory::RemoteCollections);

    assert!(scene.begin_relayout_transaction());
    let retained = scene.relayout_journal_retained_bytes().unwrap();
    assert_eq!(Some(retained), scene.relayout_journal_capacity_bytes(),);
    assert!(retained > 0);
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        baseline + retained,
    );
    scene.commit_relayout_transaction();
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), baseline,);

    assert!(scene.begin_relayout_transaction());
    let rollback_retained = scene.relayout_journal_retained_bytes().unwrap();
    scene.invalidation.clear();
    scene.rollback_relayout_transaction();
    assert!(scene.invalidation.layout.contains(&id));
    assert_eq!(scene.invalidation.composite.bounding_box(), Some(dirty));
    assert_eq!(scene.invalidation.present.bounding_box(), Some(dirty));
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), baseline,);
    assert!(rollback_retained > 0);

    drop(scene);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
}

#[test]
fn relayout_journal_pressure_and_stale_ids_preserve_exact_state() {
    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let governor = ResourceGovernor::new();
    let mut scene = build::from_document_governed(&doc, &governor);
    let id = scene
        .iter_tree_order()
        .find(|node| node.id() != scene.root())
        .unwrap()
        .id();
    assert!(scene.invalidation.layout.insert(id));
    let before_invalidation = scene.invalidation.clone();
    let before_error = scene.resource_limit_exceeded();
    let baseline = governor.used(ResourceCategory::RemoteCollections);
    let blocker = governor
        .reserve(
            ResourceCategory::RemoteCollections,
            MAX_REMOTE_MEMORY - governor.total_used(),
        )
        .unwrap();
    let pressured = governor.used(ResourceCategory::RemoteCollections);

    assert!(!scene.begin_relayout_transaction());
    assert_eq!(scene.relayout_journal_retained_bytes(), None);
    assert_eq!(scene.invalidation.layout, before_invalidation.layout);
    assert_eq!(
        scene.invalidation.composite.bounding_box(),
        before_invalidation.composite.bounding_box(),
    );
    assert_eq!(
        scene.invalidation.present.bounding_box(),
        before_invalidation.present.bounding_box(),
    );
    assert_eq!(scene.resource_limit_exceeded(), before_error);
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        pressured,
    );
    drop(blocker);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), baseline,);

    let stale = scene
        .iter_tree_order()
        .find(|node| node.id() != scene.root())
        .unwrap()
        .id();
    scene.nodes.remove(stale).unwrap();
    assert!(scene.begin_relayout_transaction());
    let buffer_state = scene.relayout_journal_buffer_state().unwrap();
    let retained = scene.relayout_journal_retained_bytes().unwrap();
    assert!(scene.stage_buffer_for_relayout(stale));
    assert_eq!(scene.relayout_journal_buffer_state(), Some(buffer_state));
    assert_eq!(scene.relayout_journal_retained_bytes(), Some(retained));
    scene.rollback_relayout_transaction();
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), baseline,);
}

// ─── Per-node buffer infrastructure ───────────────────────────

mod buffer_infrastructure {
    use crate::color::ColorSupport;
    use crate::compositor::layout::cell::{Cell, CellBuffer, CellStyle};
    use crate::compositor::layout::engine::layout_scene;
    use crate::compositor::layout::text::WidthConfig;
    use crate::compositor::scene::{self, NodeKind};
    use crate::parser::parse;
    use crate::resource::{ResourceCategory, ResourceGovernor};
    use crate::scanner::Scanner;

    fn parse_aml(src: &str) -> crate::parser::ast::Document {
        let mut scanner = Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        parse(tokens).document.expect("parse failed")
    }

    fn hydrate(doc: &crate::parser::ast::Document, w: u16, h: u16) -> scene::Scene {
        let mut s = scene::build::from_document(doc);
        let lo = layout_scene(
            &mut s,
            w,
            h,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        for p in &lo.placed {
            if let Some(id) = s.find_by_aml_id(&p.id) {
                match &p.kind {
                    crate::compositor::layout::engine::PlacedKind::Panel
                    | crate::compositor::layout::engine::PlacedKind::Animation { .. }
                    | crate::compositor::layout::engine::PlacedKind::Live { .. } => {
                        if !p.rect.is_empty() {
                            s.allocate_buffer(id, p.rect.w, p.rect.h);
                        }
                    }
                }
            }
        }
        s
    }

    #[test]
    fn governed_scene_buffers_retain_exact_individual_leases() {
        let doc = parse_aml(
            r#"[page mode=document]
                [text id="title"]Hello[/text]
                [live id="clock" endpoint="/clock"][text]Waiting[/text][/live]
            [/page]"#,
        );
        let governor = ResourceGovernor::new();
        let mut scene = scene::build::from_document_governed(&doc, &governor);

        let layout = layout_scene(
            &mut scene,
            40,
            8,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        for placed in &layout.placed {
            if let Some(id) = scene.find_by_aml_id(&placed.id)
                && matches!(
                    &placed.kind,
                    crate::compositor::layout::engine::PlacedKind::Live { .. }
                )
                && !placed.rect.is_empty()
            {
                scene.ensure_buffer(id, placed.rect.w, placed.rect.h);
            }
        }

        assert!(scene.shares_budget_with(&governor));
        assert!(scene.buffer_cell_count() > 0);
        assert_eq!(
            governor.used(ResourceCategory::SceneCells),
            scene.buffer_cell_count(),
        );
        drop(scene);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
    }

    /// Scene hydration: a panel, an animation, and a live region each
    /// get their own `CellBuffer` sized to their post-layout rect. The
    /// kind-gated accessors return `Some` only for the matching kind.
    #[test]
    fn hydrate_allocates_buffer_per_kind() {
        let doc = parse_aml(
            r#"[page mode=document]
                [panel id="p" state="a"]
                    [state name="a"][text]Panel[/text][/state]
                [/panel]
                [animate id="anim" fps=10][frame][text]frame1[/text][/frame][/animate]
                [live id="clock" endpoint="/clock"][text]--:--[/text][/live]
            [/page]"#,
        );
        let mut s = hydrate(&doc, 40, 20);

        let panel_id = s.find_by_aml_id("p").unwrap();
        let anim_id = s.find_by_aml_id("anim").unwrap();
        let live_id = s.find_by_aml_id("clock").unwrap();

        // Each node has a buffer.
        assert!(
            s.buffer_of(panel_id).is_some(),
            "panel buffer not allocated"
        );
        assert!(
            s.buffer_of(anim_id).is_some(),
            "animation buffer not allocated"
        );
        assert!(s.buffer_of(live_id).is_some(), "live buffer not allocated");

        // Kind-gated accessors respect the node's kind.
        assert!(
            s.panel_buffer_mut(panel_id).is_some(),
            "panel_buffer_mut for Panel"
        );
        assert!(
            s.panel_buffer_mut(anim_id).is_none(),
            "panel_buffer_mut on Animation is None"
        );
        assert!(
            s.wasm_buffer_mut(anim_id).is_some(),
            "wasm_buffer_mut for Animation"
        );
        assert!(
            s.wasm_buffer_mut(panel_id).is_none(),
            "wasm_buffer_mut on Panel is None"
        );
        assert!(
            s.live_buffer_mut(live_id).is_some(),
            "live_buffer_mut for Live"
        );
        assert!(
            s.live_buffer_mut(anim_id).is_none(),
            "live_buffer_mut on Animation is None"
        );
    }

    /// `Root` and structural `Flow` sources (Header, Body, Frame, State, …)
    /// stay `buffer: None`: they don't paint chrome themselves and, in
    /// Strategy D terms, have nothing to composite. `Flow { source: Box }`
    /// (and `Absolute`) DO get a buffer after the Phase 2 pivot of
    /// `layout_box_node` — that's where bg/border/title cells live. This
    /// test pins the structural-only set down so a future pivot widening
    /// forces a deliberate update.
    #[test]
    fn non_bufferable_kinds_stay_empty() {
        use crate::compositor::scene::FlowSource;
        let doc = parse_aml(
            r#"[page mode=document]
                [text]Hello[/text]
                [box w=5 h=2 border=single][/box]
            [/page]"#,
        );
        let s = hydrate(&doc, 20, 10);
        for n in s.iter_tree_order() {
            let is_structural = match n.kind() {
                NodeKind::Root | NodeKind::Row(_) => true,
                NodeKind::Flow(data) => !matches!(data.source, FlowSource::Box),
                _ => false,
            };
            if !is_structural {
                continue;
            }
            assert!(
                n.buffer().is_none(),
                "structural container has a buffer: {:?}",
                n.kind_tag(),
            );
        }
    }

    /// The new capability: two independent animations at different
    /// z-indices composite correctly through their *scene-owned* buffers.
    /// Each animation writes into its own layer; the compositor stacks
    /// them. No inter-layer knowledge is required.
    ///
    /// Guards the "stacked animations render correctly" behavior —
    /// two animations at different z-indices composite through their
    /// scene-owned buffers without an explicit blend step.
    #[test]
    fn stacked_animations_via_scene_buffers_composite() {
        let doc = parse_aml(
            r#"[page mode=document]
                [animate id="bg" fps=10][frame][text]..........[/text][/frame][/animate]
                [animate id="fg" fps=10][frame][text]FFFFF[/text][/frame][/animate]
            [/page]"#,
        );
        let mut s = hydrate(&doc, 20, 5);

        // Simulate each animation writing into its scene-owned buffer.
        let bg_id = s.find_by_aml_id("bg").unwrap();
        let fg_id = s.find_by_aml_id("fg").unwrap();

        // Background: dense "." across its buffer.
        let bg_w;
        let bg_h;
        {
            let bg = s.wasm_buffer_mut(bg_id).expect("bg buffer missing");
            bg_w = bg.width;
            bg_h = bg.height;
            for y in 0..bg.height {
                for x in 0..bg.width {
                    bg.put_char(x, y, '.', &CellStyle::default());
                }
            }
        }

        // Foreground: "FFFFF" on row 0, rest absent (transparent).
        {
            let fg = s.wasm_buffer_mut(fg_id).expect("fg buffer missing");
            for x in 0..5.min(fg.width) {
                fg.put_char(x, 0, 'F', &CellStyle::default());
            }
        }

        // Composite the two scene-owned buffers manually via the same
        // `blit` primitive the scene walk uses. Verifies the
        // "reveal-through-gaps" property independent of the animation
        // runtime's wiring (which is covered by the parity harness).
        let mut out = CellBuffer::new(bg_w, bg_h);
        // Paint bg first (lower z).
        {
            let src = s.buffer_of(bg_id).unwrap();
            for y in 0..src.height.min(out.height) {
                for x in 0..src.width.min(out.width) {
                    if let Some(cell) = src.get(x, y)
                        && !cell.is_transparent()
                    {
                        out.set(x, y, cell.clone());
                    }
                }
            }
        }
        // Paint fg on top.
        {
            let src = s.buffer_of(fg_id).unwrap();
            for y in 0..src.height.min(out.height) {
                for x in 0..src.width.min(out.width) {
                    if let Some(cell) = src.get(x, y)
                        && !cell.is_transparent()
                    {
                        out.set(x, y, cell.clone());
                    }
                }
            }
        }

        // Row 0: foreground 'F's win for x in 0..5; background shows through
        // only where the foreground left cells absent. In this fixture, the
        // fg layer is the same size as bg, so cells past x=5 on the fg layer
        // are absent (transparent), and the bg's '.'s should show through.
        // This is the "reveal through gaps" property.
        let row0: String = (0..bg_w)
            .map(|x| out.get(x, 0).map(|c| c.ch).unwrap_or(' '))
            .collect();
        assert!(
            row0.starts_with("FFFFF"),
            "foreground should win: row0 = {row0:?}"
        );
        assert!(
            row0.trim_end_matches('.')
                .trim_end()
                .trim_end_matches('F')
                .is_empty(),
            "background '.' should reveal past the foreground: row0 = {row0:?}",
        );

        // Row 1+: only background is present; should be all '.'s.
        let row1: String = (0..bg_w)
            .map(|x| out.get(x, 1).map(|c| c.ch).unwrap_or(' '))
            .collect();
        assert_eq!(
            row1.trim_end_matches('.').trim_end(),
            "",
            "row 1 should be all '.': {row1:?}",
        );

        // Cells outside both buffers (if any) would fall back to base.
        let _ = Cell::empty(); // silences an otherwise-unused import helper
    }

    /// Widened `layout_buffer_mut` (per the per-node-buffer migration
    /// Phase 0) returns `Some` for every layout-owned kind once a
    /// buffer has been allocated, and `None` for kinds owned by
    /// other subsystems. Layout does not currently allocate buffers
    /// on these kinds in production — the accessor is widened ahead
    /// of the pivot so that scene surface is ready.
    ///
    /// The fixture exercises every layout-owned kind at least once
    /// (Flow, Row, Absolute, Text, Hr, Spacer, Link, Button, Input,
    /// Select, OptionLeaf, Table, Tr, Th, Td) alongside non-layout
    /// kinds (Panel, Animation, LiveRegion) whose buffers must stay
    /// outside the layout accessor's reach.
    #[test]
    fn layout_buffer_mut_covers_every_layout_owned_kind() {
        use crate::compositor::scene::node::{KindTag, NodeId};

        let doc = parse_aml(
            r#"[page mode=document]
                [box w=10 h=2][/box]
                [box x=1 y=1 w=5 h=2][/box]
                [row][col][text]hello[/text][/col][/row]
                [hr]
                [spacer lines=2]
                [text]plain [link href="atp://example.com/x"]L[/link] end[/text]
                [form action="atp://example.com/submit"]
                  [input name="q"]
                  [select name="s"][option value="a"]A[/option][/select]
                  [button action="submit" target="f"]go[/button]
                [/form]
                [table]
                  [thead][tr][th]H[/th][/tr][/thead]
                  [tbody][tr][td]c[/td][/tr][/tbody]
                [/table]
                [panel id="p" state="a"]
                    [state name="a"][text]P[/text][/state]
                [/panel]
                [animate id="a" fps=10][frame][text]f[/text][/frame][/animate]
                [live id="l" endpoint="atp://example.com/l"][/live]
            [/page]"#,
        );
        let mut s = scene::build::from_document(&doc);

        let find_by_tag = |s: &scene::Scene, target: KindTag| -> NodeId {
            s.iter_tree_order()
                .find(|n| n.kind_tag() == target)
                .unwrap_or_else(|| panic!("fixture missing a node of kind {:?}", target))
                .id()
        };

        let layout_owned_tags: [KindTag; 15] = [
            KindTag::Flow,
            KindTag::Row,
            KindTag::Absolute,
            KindTag::Text,
            KindTag::Hr,
            KindTag::Spacer,
            KindTag::Link,
            KindTag::Button,
            KindTag::Input,
            KindTag::Select,
            KindTag::OptionLeaf,
            KindTag::Table,
            KindTag::Tr,
            KindTag::Th,
            KindTag::Td,
        ];

        for &tag in &layout_owned_tags {
            let id = find_by_tag(&s, tag);
            s.allocate_buffer(id, 1, 1);
            assert!(
                s.layout_buffer_mut(id).is_some(),
                "layout_buffer_mut must return Some for allocated {:?} node",
                tag,
            );
            assert!(s.wasm_buffer_mut(id).is_none());
            assert!(s.live_buffer_mut(id).is_none());
            assert!(s.panel_buffer_mut(id).is_none());
        }

        // Non-layout kinds: `layout_buffer_mut` never reaches them,
        // even when a buffer is allocated. And the subsystem-owning
        // accessor does see it.
        for tag in [KindTag::Panel, KindTag::Animation, KindTag::LiveRegion] {
            let id = find_by_tag(&s, tag);
            s.allocate_buffer(id, 1, 1);
            assert!(
                s.layout_buffer_mut(id).is_none(),
                "layout_buffer_mut must not reach {:?} buffers",
                tag,
            );
        }
        let panel_id = find_by_tag(&s, KindTag::Panel);
        assert!(s.panel_buffer_mut(panel_id).is_some());
        let anim_id = find_by_tag(&s, KindTag::Animation);
        assert!(s.wasm_buffer_mut(anim_id).is_some());
        let live_id = find_by_tag(&s, KindTag::LiveRegion);
        assert!(s.live_buffer_mut(live_id).is_some());
    }
}

// ─── Patch integration ────────────────────────────────────────

mod patch_integration {
    use crate::compositor::scene::{self, NodeKind, Patch, PatchApplier};
    use crate::parser::parse;
    use crate::scanner::Scanner;

    fn parse_aml(src: &str) -> crate::parser::ast::Document {
        let mut scanner = Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        parse(tokens).document.expect("parse failed")
    }

    /// End-to-end: a scene built from a panel-containing document,
    /// then mutated by `Patch::SetPanelActive`, reflects the new
    /// active state without rebuilding the scene. Demonstrates the
    /// patch channel over a production-shaped scene graph.
    #[test]
    fn patch_applies_to_scene_built_from_document() {
        let doc = parse_aml(
            r#"[page mode=document]
                [panel id="tabs" state="one"]
                    [state name="one"][text]One[/text][/state]
                    [state name="two"][text]Two[/text][/state]
                    [state name="three"][text]Three[/text][/state]
                [/panel]
            [/page]"#,
        );
        let mut s = scene::build::from_document(&doc);

        let panel_id = s.find_by_aml_id("tabs").unwrap();
        // Resolve "three" by walking states.
        let three_id = {
            let panel = s.get(panel_id).unwrap();
            if let NodeKind::Panel { states, .. } = panel.kind() {
                *states
                    .iter()
                    .find(|&&id| s.get(id).and_then(|n| n.aml_id()) == Some("three"))
                    .unwrap()
            } else {
                panic!();
            }
        };

        PatchApplier::apply(
            &mut s,
            Patch::SetPanelActive {
                panel: panel_id,
                active: three_id,
            },
        );

        // Scene reflects the flip; invalidation populated.
        if let NodeKind::Panel { active, .. } = s.get(panel_id).unwrap().kind() {
            assert_eq!(*active, three_id);
        }
        assert!(s.invalidation.layout.contains(&panel_id));
    }
}

// ─── Phase 1: NodeKind::Overlay ──────────────────────────────────
//
// Overlay is a system-synthesized kind (page transitions, future debug
// overlays). These tests verify its invariants hold without needing
// any consumer wired up yet: build::from_document never produces one,
// hit-test ignores it, and insert_overlay/remove_overlay round-trip
// through the scene.

#[cfg(test)]
mod overlay_tests {
    use crate::compositor::layout::Rect;
    use crate::compositor::scene::{self, NodeKind, OverlaySource};
    use crate::resource::{ResourceCategory, ResourceGovernor};

    fn simple_scene() -> scene::Scene {
        let src = r#"[page mode=document][text]hello[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        scene::build::from_document(&doc)
    }

    #[test]
    fn build_from_document_never_produces_overlay() {
        // Parity: AML never mentions an "overlay" — every NodeKind::Overlay
        // must come from `insert_overlay`, never from the AST. Scan every
        // node of every fixture AML for an Overlay; there should be zero.
        let fixtures = [
            "tests/fixtures/aml/hello.aml",
            "tests/fixtures/aml/kitchen-sink.aml",
        ];
        for path in fixtures {
            let src = std::fs::read_to_string(crate::repository_root().join(path)).unwrap();
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            let doc = crate::parser::parse(tokens).document.unwrap();
            let s = scene::build::from_document(&doc);
            let overlay_count = s
                .iter_tree_order()
                .filter(|n| matches!(n.kind(), NodeKind::Overlay(_)))
                .count();
            assert_eq!(overlay_count, 0, "{path} produced an Overlay from AST");
        }
    }

    #[test]
    fn insert_overlay_allocates_buffer_and_attaches_to_root() {
        let mut s = simple_scene();
        let before_root_children = s.get(s.root()).unwrap().children().len();

        let id = s.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 40, 10),
        );

        let node = s.get(id).expect("overlay exists");
        assert!(matches!(node.kind(), NodeKind::Overlay(_)));
        assert_eq!(node.z_index(), i16::MAX);
        let buf = node.buffer().expect("overlay buffer allocated");
        assert_eq!(buf.width, 40);
        assert_eq!(buf.height, 10);
        assert_eq!(node.placement().rect, Rect::new(0, 0, 40, 10));
        assert_eq!(node.parent(), Some(s.root()));

        // Root gained exactly one new child.
        let after_root_children = s.get(s.root()).unwrap().children().len();
        assert_eq!(after_root_children, before_root_children + 1);
    }

    #[test]
    fn insert_overlay_marks_composite_invalidation() {
        let mut s = simple_scene();
        s.invalidation.clear();
        assert!(s.invalidation.composite.is_empty());

        s.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 40, 10),
        );

        assert!(
            !s.invalidation.composite.is_empty(),
            "insert_overlay must seed composite invalidation so the compositor cache is dropped",
        );
    }

    #[test]
    fn hit_test_skips_overlay() {
        // Even though the overlay covers every cell of the viewport,
        // hit_test must walk past it and report None — keyboard/click
        // events during a transition have to reach the underlying scene.
        let mut s = simple_scene();

        // Give the root a placement so hit_test has something to walk.
        // (build_scene leaves placements empty until layout runs.)
        s.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 40, 10),
        );

        // No other node has a non-empty placement, so the only candidate
        // covering (5, 5) is the overlay — and hit_test must refuse it.
        let hit = s.hit_test(5, 5);
        assert!(
            hit.is_none(),
            "overlay is at (5, 5) but hit_test returned {hit:?}; overlays must not capture input",
        );
    }

    #[test]
    fn remove_overlay_detaches_and_reinvalidates() {
        let mut s = simple_scene();
        let id = s.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 40, 10),
        );
        s.invalidation.clear();

        s.remove_overlay(id);

        assert!(s.get(id).is_none(), "overlay node removed");
        let root_children = s.get(s.root()).unwrap().children();
        assert!(
            !root_children.contains(&id),
            "overlay detached from root child list",
        );
        assert!(
            !s.invalidation.composite.is_empty(),
            "remove_overlay marks composite so the cache is dropped",
        );
    }

    #[test]
    fn remove_overlay_refuses_non_overlay_node() {
        // Safety guard: remove_overlay is for compositor-owned synthesized
        // nodes only. Calling it on an AML-backed node is a no-op.
        let mut s = simple_scene();
        let text_id = s
            .iter_tree_order()
            .find(|n| matches!(n.kind(), NodeKind::Text(_)))
            .expect("fixture has a Text node")
            .id();

        s.remove_overlay(text_id);

        assert!(
            s.get(text_id).is_some(),
            "remove_overlay on Text node must be a no-op",
        );
    }

    #[test]
    fn overlay_buffer_mut_is_kind_gated() {
        // The kind-gated accessor pattern: overlay_buffer_mut returns
        // Some only for Overlay nodes, None for everything else.
        let mut s = simple_scene();
        let overlay_id = s.insert_overlay(0, OverlaySource::PageTransition, Rect::new(0, 0, 10, 5));
        let text_id = s
            .iter_tree_order()
            .find(|n| matches!(n.kind(), NodeKind::Text(_)))
            .map(|n| n.id())
            .expect("fixture has a Text node");

        assert!(
            s.overlay_buffer_mut(overlay_id).is_some(),
            "overlay_buffer_mut on Overlay node must return Some",
        );
        assert!(
            s.overlay_buffer_mut(text_id).is_none(),
            "overlay_buffer_mut on non-Overlay node must return None",
        );
        assert!(
            s.panel_buffer_mut(overlay_id).is_none(),
            "panel_buffer_mut on Overlay node must return None",
        );
    }

    #[test]
    fn governed_overlay_owns_and_releases_exact_scene_cell_lease() {
        let src = r#"[page mode=document][text]hello[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let governor = ResourceGovernor::new();
        let mut scene = scene::build::from_document_governed(&doc, &governor);

        let id = scene.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 20, 4),
        );
        assert_eq!(governor.used(ResourceCategory::SceneCells), 80);

        scene.remove_overlay(id);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
    }

    #[test]
    fn dormant_page_transition_slot_transfers_lease_and_reuses_topology() {
        let src = r#"[page mode=document][text]hello[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let governor = ResourceGovernor::new();
        let mut scene = scene::build::from_document_governed(&doc, &governor);
        let layout_invalidation_bytes = scene.layout_invalidation_admission_bytes();
        let node_capacity = scene.nodes.capacity();
        let node_topology_bytes = scene.node_topology_admission_bytes();
        assert_eq!(
            Some(node_topology_bytes),
            scene.node_topology_capacity_bytes(),
        );
        assert!(scene.prepare_page_transition_overlay());
        let topology_bytes = governor.used(ResourceCategory::RemoteCollections);
        assert_eq!(
            topology_bytes,
            layout_invalidation_bytes + scene.node_relation_admission_bytes() + node_topology_bytes,
        );
        assert_eq!(scene.nodes.capacity(), node_capacity);
        assert_eq!(scene.node_topology_admission_bytes(), node_topology_bytes);
        let cells = 20 * 4;
        let lease = governor
            .reserve_with_cost(
                ResourceCategory::SceneCells,
                cells,
                cells * std::mem::size_of::<crate::compositor::layout::cell::Cell>(),
            )
            .unwrap();

        let id = scene.page_transition_overlay_slot().unwrap();
        let node_count = scene.node_count();
        let root_children = scene.get(scene.root()).unwrap().children().len();
        let buffer =
            crate::compositor::layout::cell::CellBuffer::try_new_opaque_with_lease(20, 4, lease)
                .unwrap();
        assert_eq!(
            scene.activate_page_transition_overlay(Rect::new(0, 0, 20, 4), buffer),
            Some(id),
        );
        assert_eq!(governor.used(ResourceCategory::SceneCells), cells);

        scene.remove_page_transition_overlay(id);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
        assert_eq!(scene.node_count(), node_count);
        assert_eq!(
            scene.get(scene.root()).unwrap().children().len(),
            root_children
        );
        assert_eq!(scene.page_transition_overlay_slot(), Some(id));
        assert!(scene.page_transition_overlay().is_none());
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            topology_bytes,
            "dormant topology capacity remains charged for the scene lifetime",
        );

        let second_lease = governor
            .reserve_with_cost(
                ResourceCategory::SceneCells,
                cells,
                cells * std::mem::size_of::<crate::compositor::layout::cell::Cell>(),
            )
            .unwrap();
        let second = crate::compositor::layout::cell::CellBuffer::try_new_opaque_with_lease(
            20,
            4,
            second_lease,
        )
        .unwrap();
        assert_eq!(
            scene.activate_page_transition_overlay(Rect::new(0, 0, 20, 4), second),
            Some(id),
            "the same stable slot is reused without topology growth",
        );
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            topology_bytes,
        );
        assert_eq!(scene.nodes.capacity(), node_capacity);
        assert_eq!(scene.node_topology_admission_bytes(), node_topology_bytes);
        drop(scene);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
    }
}

/// The run vectors in a built scene must be reserved exactly, not grown.
///
/// `count_flattened_runs` predicts what `flatten_inline_text` will produce so
/// the vector can be `try_reserve_exact`ed before any of it is built. The two
/// are separate functions walking the same recursive shape, so they can drift
/// apart silently — a prediction that is too low reallocates during the fill,
/// which is exactly the unbounded growth the reservation exists to prevent.
/// Capacity equal to length is the observable consequence of them agreeing.
#[test]
fn text_run_vectors_are_reserved_exactly_not_grown() {
    let source = r#"[page]
        [text]outer[text bold]nested[text italic]deeper[/text][/text][/text]
        [text]plain[/text]
        [text]with a [link href="/x"]link[/link] inline and [text dim]more[/text][/text]
        [text][/text]
    [/page]"#;
    let mut scanner = Scanner::new(source.as_bytes()).expect("scan");
    let tokens = scanner.scan_all().expect("scan_all");
    let doc = parse(tokens).document.expect("parse");
    let scene = build::from_document(&doc);

    let mut inspected = 0usize;
    for node in scene.iter_tree_order() {
        if let crate::compositor::scene::node::NodeKind::Text(content) = node.kind() {
            assert_eq!(
                content.runs.capacity(),
                content.runs.len(),
                "run vector grew past its reservation: {:?}",
                content.runs
            );
            inspected += 1;
        }
    }
    assert!(
        inspected >= 4,
        "expected several text nodes, saw {inspected}"
    );
}

/// A refused relayout journal leaves the scene exactly as it was.
///
/// The journal admits five collections as one transaction so a later rollback
/// cannot itself allocate. Exhausting a real governor shows *a* refusal;
/// naming the site shows the refusal is atomic — no journal is installed, no
/// budget is held, and the very next attempt succeeds.
#[test]
fn relayout_journal_rejection_installs_nothing_and_recovers() {
    use crate::compositor::scene::tree::{SceneAllocationSite, SceneRejectionGuard};

    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let governor = ResourceGovernor::new();
    let mut scene = build::from_document_governed(&doc, &governor);
    let before = governor.used(ResourceCategory::RemoteCollections);

    let rejection = SceneRejectionGuard::at(SceneAllocationSite::RelayoutJournal);
    assert!(!scene.begin_relayout_transaction());
    assert_eq!(
        scene.relayout_journal_retained_bytes(),
        None,
        "a refused transaction must install no journal"
    );
    assert_eq!(
        governor.used(ResourceCategory::RemoteCollections),
        before,
        "a refused transaction must hold no budget"
    );
    drop(rejection);

    assert!(
        scene.begin_relayout_transaction(),
        "the transaction must succeed once the site is disarmed"
    );
    assert!(scene.relayout_journal_retained_bytes().is_some());
    scene.commit_relayout_transaction();
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);
}

/// Every scene allocation site refuses without leaving a partial scene, and
/// the same document builds cleanly once the site is disarmed.
///
/// The scene admits four things separately — node arena, relation topology,
/// invalidation, and each node's buffer — and a refusal in any one of them has
/// to mark the candidate rather than half-build it. Governor exhaustion
/// refuses whichever comes first; naming the site is what covers the rest.
#[test]
fn every_scene_allocation_site_refuses_without_a_partial_scene() {
    use crate::compositor::scene::tree::{SceneAllocationSite, SceneRejectionGuard};

    let doc = parse_file("tests/fixtures/aml/hello.aml");

    for site in [
        SceneAllocationSite::RelationTopology,
        SceneAllocationSite::Invalidation,
    ] {
        let governor = ResourceGovernor::new();
        let rejection = SceneRejectionGuard::at(site);
        let refused = build::from_document_governed(&doc, &governor);
        assert!(
            refused.resource_limit_exceeded(),
            "{site:?} did not mark the scene as a failed candidate"
        );
        drop(refused);
        assert_eq!(governor.total_used(), 0, "{site:?} leaked budget");
        drop(rejection);

        let accepted = build::from_document_governed(&doc, &governor);
        assert!(
            !accepted.resource_limit_exceeded(),
            "the scene must build once {site:?} is disarmed"
        );
        drop(accepted);
        assert_eq!(governor.total_used(), 0);
    }
}

/// A refused node buffer marks the scene and retains no cells.
///
/// `allocate_buffer` checkpoints the live buffer before attempting the
/// candidate, so that a refusal cannot leave layout painting into the previous
/// page's pixels. Naming the site is the only way to exercise that ordering:
/// exhausting the governor refuses the scene long before any buffer is
/// reached.
#[test]
fn refused_node_buffer_marks_the_scene_and_retains_no_cells() {
    use crate::compositor::scene::tree::{SceneAllocationSite, SceneRejectionGuard};

    let doc = parse_file("tests/fixtures/aml/hello.aml");
    let governor = ResourceGovernor::new();
    let mut scene = build::from_document_governed(&doc, &governor);
    let node = scene.root();
    let cells_before = governor.used(ResourceCategory::SceneCells);

    let rejection = SceneRejectionGuard::at(SceneAllocationSite::NodeBuffer);
    scene.allocate_buffer(node, 10, 2);
    assert!(scene.resource_limit_exceeded());
    assert_eq!(
        governor.used(ResourceCategory::SceneCells),
        cells_before,
        "a refused buffer must hold no cells"
    );
    drop(rejection);

    scene.allocate_buffer(node, 10, 2);
    assert!(
        governor.used(ResourceCategory::SceneCells) > cells_before,
        "the buffer must be admitted once the site is disarmed"
    );
    drop(scene);
    assert_eq!(governor.total_used(), 0);
}
