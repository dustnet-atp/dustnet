use super::*;
use crossterm::event::{KeyEventKind, KeyEventState};

fn make_key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn stale_prepared_page_is_never_installed_by_runtime() {
    let wcfg = WidthConfig::default();
    let mut active = layout_page(
        parse_aml("[page title=active][text]active[/text][/page]").unwrap(),
        40,
        12,
        ColorSupport::Truecolor,
        wcfg,
        None,
        None,
        None,
    )
    .await;
    let stale = layout_page(
        parse_aml("[page title=stale][text]stale[/text][/page]").unwrap(),
        40,
        12,
        ColorSupport::Truecolor,
        wcfg,
        None,
        None,
        None,
    )
    .await;
    let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
    let first_uri = AtpUri::parse("atp://127.0.0.1/first").unwrap();
    let first_origin = client.request_origin(&first_uri).unwrap();
    let mut lifecycle = ReducerPort::new(LifecycleModel::new(40, 12));
    let first_owner = dispatch_event(
        &mut lifecycle,
        LifecycleEvent::InitialNavigation {
            uri: first_uri,
            origin: first_origin,
        },
    )
    .into_iter()
    .find_map(|effect| match effect {
        LifecycleEffect::Connect { owner } => Some(owner),
        _ => None,
    })
    .unwrap();
    let second_uri = AtpUri::parse("atp://127.0.0.1/second").unwrap();
    let second_origin = client.request_origin(&second_uri).unwrap();
    dispatch_event(
        &mut lifecycle,
        LifecycleEvent::Navigate {
            uri: second_uri,
            origin: second_origin,
        },
    );

    let governor = active.governor.clone();
    let event_dispatcher = active
        .prepared_event_dispatcher
        .take()
        .expect("a prepared page must own an admitted event dispatcher");
    let compositor = Compositor::with_governor(40, active.buf.height, governor);
    let state = ViewportState::with_sticky(40, 12, active.buf.height, &active.sticky_buf);
    let mut runtime = TerminalRuntime {
        page: active,
        client: None,
        history: Vec::new(),
        compositor,
        state,
        needs_redraw: false,
        render_authorized: false,
        input_mode: InputMode {
            active: false,
            cursor_pos: 0,
            current_value: String::new(),
            current_node: None,
            maxlen: 0,
            password: false,
            field_col: 0,
            field_row: 0,
            field_is_sticky: false,
            wcfg,
        },
        event_dispatcher,
        deferred_navigation: None,
        deferred_proposal: None,
        resumed_navigation: PreparedSlot::default(),
        pending_tick: None,
        pending_tick_attempt: None,
        pending_page_transition: None,
        local_page_activated: false,
        last_local_page_aml_ptr: None,
        command_line: CommandLine::new(),
        help_visible: false,
        retry_after_trust: None,
        declined_trust: false,
        last_fetch_error: None,
        client_hud: ClientHud::new(),
        error_log: ErrorLog::new(),
        showing_overlay: false,
        region_buffers: RegionBuffers::new(),
        prepared_layout: None,
        prepared_wasm: PreparedSlot::default(),
        pending_updates: PreparedSlot::default(),
        wasm_resources: PreparedSlot::default(),
        fetched_pages: PreparedSlot::default(),
        parsed_pages: PreparedSlot::default(),
        prepared_navigation: PreparedSlot::default(),
        pending_history_artifact: None,
        activated_navigation: PreparedSlot::default(),
        pending_redirect_depth: None,
        color_support: ColorSupport::Truecolor,
        wcfg,
    };

    runtime.store_prepared_layout(&first_owner, PreparedLayout::Page(Box::new(stale)));
    super::super::dispatch_runtime_events(
        &mut runtime,
        &mut lifecycle,
        [LifecycleEvent::LayoutPrepared {
            owner: first_owner,
            content_height: 1,
        }],
    )
    .await
    .unwrap();
    assert_eq!(runtime.page.scene.title.as_deref(), Some("active"));
}

#[test]
fn governed_sticky_split_rolls_back_without_orphan_leases() {
    let governor = ResourceGovernor::new();
    let buffer =
        CellBuffer::try_new_governed(10, 10, &governor, ResourceCategory::CompositorCells).unwrap();
    let original_bytes = buffer.cell_count() * std::mem::size_of::<Cell>();
    let _pressure = governor
        .reserve(
            ResourceCategory::CompositorCells,
            crate::resource::MAX_REMOTE_MEMORY - original_bytes - 1,
        )
        .unwrap();
    let sticky = [crate::compositor::layout::engine::StickyRegion {
        position: crate::parser::ast::StickyPosition::Bottom,
        y: 6,
        h: 4,
    }];

    let (main, sticky) = split_sticky(buffer, &sticky, &mut [], Some(&governor));

    assert!(main.allocation_failed());
    assert_eq!((main.width, main.height), (10, 10));
    assert!(sticky.is_none());
    assert_eq!(
        governor.used(ResourceCategory::CompositorCells),
        crate::resource::MAX_REMOTE_MEMORY - 1
    );
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn exhausted_origin_still_builds_independent_client_error_page() {
    let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
    let hostile_governor = client.governor.clone();
    let _pressure = hostile_governor
        .reserve(
            ResourceCategory::RemoteCollections,
            crate::resource::MAX_REMOTE_MEMORY,
        )
        .unwrap();
    let doc = parse_aml("[page mode=document][text]hostile[/text][/page]").unwrap();

    let page = layout_page(
        doc,
        40,
        12,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        Some(&mut client),
        None,
        None,
    )
    .await;

    assert_eq!(page.scene.title.as_deref(), Some("Content blocked"));
    assert!(!page.buf.allocation_failed());
    assert!(!page.governor.shares_budget_with(&hostile_governor));
    assert!(page.governor.used(ResourceCategory::CompositorCells) > 0);
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn loaded_page_retains_and_releases_projection_collection_capacity() {
    let page = layout_page(
        parse_aml(
            r#"[page mode=document]
                [input name="query"]
                [panel id="p" state="a"]
                    [state name="a"][text]A[/text][/state]
                    [state name="b"][text]B[/text][/state]
                [/panel]
                [button action="toggle" target="p" states="a,b"]Toggle panel state[/button]
                [link id="documentation" href="/documentation"][text]Read docs[/text][/link]
                [live id="clock" endpoint="/endpoint-with-capacity"][/live]
                [box sticky=bottom][text]footer[/text][/box]
            [/page]"#,
        )
        .unwrap(),
        40,
        12,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        None,
        None,
        None,
    )
    .await;
    let governor = page.governor.clone();
    let focusable_payload = page
        .focusables
        .iter()
        .map(|focusable| focusable.retained_payload_capacity().unwrap())
        .sum::<usize>();
    let (focusable_count, focusable_bound) =
        crate::compositor::panels::focusable_storage_requirements(&page.scene).unwrap();
    assert_eq!(focusable_count, page.focusables.len());
    assert!(focusable_payload > 0);
    assert!(focusable_payload <= focusable_bound);
    let expected = page
        .focusables
        .capacity()
        .saturating_mul(std::mem::size_of::<
            crate::compositor::panels::FocusableElement,
        >())
        .saturating_add(
            page.placed
                .capacity()
                .saturating_mul(std::mem::size_of::<PlacedElement>()),
        )
        .saturating_add(
            page._sticky_regions
                .capacity()
                .saturating_mul(std::mem::size_of::<
                    crate::compositor::layout::engine::StickyRegion,
                >()),
        )
        .saturating_add(
            page.placed
                .iter()
                .map(PlacedElement::retained_string_capacity)
                .sum::<usize>(),
        )
        .saturating_add(focusable_payload);
    let expected = expected
        .saturating_add(page.anim_rt.retained_collection_capacity_bytes())
        .saturating_add(page.scene.layout_invalidation_admission_bytes())
        .saturating_add(page.scene.node_relation_admission_bytes())
        .saturating_add(page.scene.page_transition_topology_admission_bytes())
        .saturating_add(
            page.prepared_event_dispatcher
                .as_ref()
                .unwrap()
                .retained_collection_capacity_bytes(),
        );
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), expected);
    drop(page);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn projection_reservation_rejects_placed_payloads_before_population() {
    let page = layout_page(
        parse_aml(
            r#"[page mode=document]
                [panel id="panel-with-capacity" state="a"]
                    [state name="a"][text]A[/text][/state]
                [/panel]
                [live id="live-with-capacity" endpoint="/endpoint-with-capacity"][/live]
            [/page]"#,
        )
        .unwrap(),
        40,
        12,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        None,
        None,
        None,
    )
    .await;
    let capacity = page.scene.iter_tree_order().count();
    let item_bytes = std::mem::size_of::<crate::compositor::panels::FocusableElement>()
        + std::mem::size_of::<PlacedElement>()
        + std::mem::size_of::<crate::compositor::layout::engine::StickyRegion>();
    let structural_bytes = capacity * item_bytes;
    let string_bound = page.scene.placed_storage_requirements(true).unwrap().1;
    assert!(string_bound > 0);
    let governor = ResourceGovernor::new();
    let blocker = governor
        .reserve(
            ResourceCategory::RemoteCollections,
            crate::resource::MAX_REMOTE_MEMORY - structural_bytes,
        )
        .unwrap();
    let before = governor.used(ResourceCategory::RemoteCollections);

    assert!(reserve_projection_collections(&page.scene, Some(&governor)).is_none());
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);

    drop(blocker);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn projection_reservation_rejects_focusable_payloads_before_population() {
    let page = layout_page(
        parse_aml(
            r#"[page mode=document]
                [input id="search-field" name="query-with-capacity" placeholder="Search remotely supplied text"]
            [/page]"#,
        )
        .unwrap(),
        40,
        12,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        None,
        None,
        None,
    )
    .await;
    let capacity = page.scene.iter_tree_order().count();
    let structural_bytes = capacity
        * (std::mem::size_of::<crate::compositor::panels::FocusableElement>()
            + std::mem::size_of::<PlacedElement>()
            + std::mem::size_of::<crate::compositor::layout::engine::StickyRegion>());
    let placed_bound = page.scene.placed_storage_requirements(true).unwrap().1;
    let focusable_bound = crate::compositor::panels::focusable_storage_requirements(&page.scene)
        .unwrap()
        .1;
    assert!(focusable_bound > 0);
    let pre_payload_bytes = structural_bytes + placed_bound;
    let governor = ResourceGovernor::new();
    let blocker = governor
        .reserve(
            ResourceCategory::RemoteCollections,
            crate::resource::MAX_REMOTE_MEMORY - pre_payload_bytes,
        )
        .unwrap();
    let before = governor.used(ResourceCategory::RemoteCollections);

    assert!(reserve_projection_collections(&page.scene, Some(&governor)).is_none());
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);

    drop(blocker);
    assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
}

#[test]
fn remote_parse_reserves_its_transient_bound_before_scanning() {
    let governor = ResourceGovernor::new();
    let aml = "[page][text]bounded[/text][/page]";
    let transient = aml.len() * PARSE_TRANSIENT_MULTIPLIER;
    let pressure = governor
        .reserve(
            ResourceCategory::AstStrings,
            crate::resource::MAX_REMOTE_MEMORY - transient + 1,
        )
        .unwrap();

    assert!(matches!(
        parse_remote_aml(aml, &governor),
        Err(RemoteParseError::ResourceRejected)
    ));
    assert_eq!(
        governor.used(ResourceCategory::AstStrings),
        pressure.amount()
    );
}

mod dynamic_scene_rendering {
    use super::*;
    use crate::color::ColorSupport;
    use crate::compositor::animate::Animation;
    use crate::compositor::layout::cell::CellStyle;
    use crate::compositor::layout::text::WidthConfig;
    use crate::protocol::message::{UpdateFlags, UpdateMessage};

    async fn page_from(aml: &str) -> LoadedPage {
        let mut scanner = crate::scanner::Scanner::new(aml.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        layout_page(
            doc,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
        )
        .await
    }

    fn composite(page: &LoadedPage) -> CellBuffer {
        crate::compositor::composite::walk(
            &page.scene,
            &AnimationRuntime::empty(),
            40,
            page.buf
                .height
                .saturating_add(page.sticky_buf.as_ref().map_or(0, |buf| buf.height)),
        )
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn anonymous_controls_keep_exact_focus_and_authored_events() {
        let mut page = page_from(
            r#"[page mode=document]
                [link href="/next"]Next[/link]
                [form action="/submit"]
                    [button action=submit]Send[/button]
                    [input name="query" /]
                    [select name="choice"]
                        [option value="a"]A[/option]
                        [option value="b"]B[/option]
                    [/select]
                [/form]
                [details summary="More"][text]Body[/text][/details]
            [/page]"#,
        )
        .await;
        assert_eq!(page.focusables.len(), 5);
        assert!(
            page.focusables
                .iter()
                .all(|focusable| focusable.id.is_none())
        );

        let mut state = ViewportState::with_sticky(40, 12, page.buf.height, &page.sticky_buf);
        let global_bindings = [
            crate::compositor::scene::EventBinding {
                event: crate::parser::ast::EventKind::Focus,
                source: None,
                action: crate::parser::ast::ActionKind::Animate,
                target: "focus-indicator".into(),
                to: None,
                delay_ms: 0,
            },
            crate::compositor::scene::EventBinding {
                event: crate::parser::ast::EventKind::Blur,
                source: None,
                action: crate::parser::ast::ActionKind::Animate,
                target: "blur-indicator".into(),
                to: None,
                delay_ms: 0,
            },
        ];
        let dispatcher = EventDispatcher::new();

        for expected_index in 0..page.focusables.len() {
            advance_focus(&mut page, &mut state, true);
            let focused = page.scene.focus().expect("Tab must focus a control");
            assert_eq!(focused, page.focusables[expected_index].node_id);

            PatchApplier::apply(&mut page.scene, Patch::SetFocus { node: None });
            assert!(page.scene.focus().is_none());
            assert!(page.scene.get(focused).is_some());
            assert!(project_scene_focus(&mut page.scene, Some(focused)));
            assert_eq!(
                current_focus_index(&page.scene, &page.focusables),
                Some(expected_index),
                "the render highlight and Enter activation must resolve the exact node",
            );

            let event = authored_focus_action(&page.focusables[expected_index], true);
            let PresentationAction::Focus { source } = event else {
                unreachable!()
            };
            assert!(source.is_none());
            assert_eq!(
                dispatcher
                    .prepare_fire(
                        &global_bindings,
                        crate::parser::ast::EventKind::Focus,
                        source.as_deref(),
                        0,
                    )
                    .unwrap()
                    .len(),
                1,
                "a source-free authored focus binding must fire for anonymous controls",
            );
        }

        assert!(matches!(
            page.focusables[0].action,
            crate::compositor::panels::FocusAction::Navigate { .. }
        ));
        assert!(matches!(
            page.focusables[1].action,
            crate::compositor::panels::FocusAction::Submit { .. }
        ));
        assert!(matches!(
            page.focusables[2].action,
            crate::compositor::panels::FocusAction::EditInput { .. }
        ));
        assert!(matches!(
            page.focusables[3].action,
            crate::compositor::panels::FocusAction::EditSelect { .. }
        ));
        assert!(matches!(
            page.focusables[4].action,
            crate::compositor::panels::FocusAction::ToggleDetails { .. }
        ));
        let PresentationAction::Blur { source } = authored_focus_action(&page.focusables[0], false)
        else {
            unreachable!()
        };
        assert!(source.is_none());
        assert_eq!(
            dispatcher
                .prepare_fire(
                    &global_bindings,
                    crate::parser::ast::EventKind::Blur,
                    source.as_deref(),
                    0,
                )
                .unwrap()
                .len(),
            1,
            "a source-free authored blur binding must fire for anonymous controls",
        );
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn authored_focus_source_remains_the_exact_aml_id() {
        let page =
            page_from(r#"[page mode=document][link id="named" href="/next"]Next[/link][/page]"#)
                .await;
        assert!(matches!(
            authored_focus_action(&page.focusables[0], true),
            PresentationAction::Focus { source: Some(ref source) } if source == "named"
        ));
        assert!(matches!(
            authored_focus_action(&page.focusables[0], false),
            PresentationAction::Blur { source: Some(ref source) } if source == "named"
        ));
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn edited_input_reaches_scene_composite_and_handles_utf8() {
        let mut page = page_from(
            r#"[page mode=document][form action="/save"][input id="name" name="name" /][/form][/page]"#,
        )
        .await;
        let node = page.scene.find_by_aml_id("name").unwrap();
        let mut input = InputMode {
            active: true,
            cursor_pos: 0,
            current_value: String::new(),
            current_node: Some(node),
            maxlen: 20,
            password: false,
            field_col: 0,
            field_row: 0,
            field_is_sticky: false,
            wcfg: WidthConfig::default(),
        };
        let mut redraw = false;

        handle_input_key(make_key(KeyCode::Char('é')), &mut input, &mut redraw);
        handle_input_key(make_key(KeyCode::Char('🦀')), &mut input, &mut redraw);
        sync_input_value(
            &mut page.scene,
            input.current_node,
            input.current_value.clone(),
        );
        assert_eq!(input.current_value, "é🦀");
        assert_eq!(input.cursor_pos, 2);

        layout_pass_invalidated(
            &mut page.scene,
            &mut page.buf,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let rendered = crate::compositor::present::render_to_string(&composite(&page));
        assert!(
            rendered.contains("[é🦀"),
            "visible input did not update: {rendered:?}"
        );

        handle_input_key(make_key(KeyCode::Backspace), &mut input, &mut redraw);
        assert_eq!(input.current_value, "é");
        assert_eq!(input.cursor_pos, 1);
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "64 KiB grapheme boundary walk is covered by native tests"
    )]
    fn input_mode_allocation_rejection_preserves_exact_edit_state() {
        let mut input = InputMode {
            active: true,
            cursor_pos: 3,
            current_value: String::from("old"),
            current_node: None,
            maxlen: 20,
            password: false,
            field_col: 2,
            field_row: 3,
            field_is_sticky: false,
            wcfg: WidthConfig::default(),
        };
        let old_ptr = input.current_value.as_ptr();

        reject_next_input_value_allocation(InputValueAllocationSite::Activation);
        assert!(!input.try_activate(None, "remote", 30, true, (8, 9, true)));
        assert_eq!(input.current_value, "old");
        assert_eq!(input.current_value.as_ptr(), old_ptr);
        assert_eq!(input.cursor_pos, 3);
        assert_eq!((input.maxlen, input.password), (20, false));
        assert_eq!((input.field_col, input.field_row), (2, 3));
        assert!(!input.field_is_sticky);

        assert!(input.try_activate(None, "remote", 30, true, (8, 9, true)));
        assert_eq!(input.current_value, "remote");
        let remote_ptr = input.current_value.as_ptr();
        let mut redraw = false;
        reject_next_input_value_allocation(InputValueAllocationSite::Growth);
        handle_input_key(make_key(KeyCode::Char('!')), &mut input, &mut redraw);
        assert_eq!(input.current_value, "remote");
        assert_eq!(input.current_value.as_ptr(), remote_ptr);
        assert_eq!(input.cursor_pos, 6);
        assert!(!redraw);

        handle_input_key(make_key(KeyCode::Char('!')), &mut input, &mut redraw);
        assert_eq!(input.current_value, "remote!");
        assert!(redraw);

        assert!(input.try_activate(None, "a👩‍💻b", 2, false, (0, 0, false)));
        assert_eq!(input.current_value, "a👩‍💻");
        assert_eq!(input.cursor_pos, 2);

        let boundary = format!("{}👩‍💻", "x".repeat(MAX_INPUT_VALUE_BYTES - 1));
        assert!(input.try_activate(None, &boundary, 0, false, (0, 0, false)));
        assert_eq!(input.current_value.len(), MAX_INPUT_VALUE_BYTES - 1);
        assert!(input.current_value.ends_with('x'));

        let boundary_ptr = input.current_value.as_ptr();
        input.cursor_pos = unicode_segmentation::UnicodeSegmentation::graphemes(
            input.current_value.as_str(),
            true,
        )
        .count();
        redraw = false;
        handle_input_key(make_key(KeyCode::Char('é')), &mut input, &mut redraw);
        assert_eq!(input.current_value.len(), MAX_INPUT_VALUE_BYTES - 1);
        assert_eq!(input.current_value.as_ptr(), boundary_ptr);
        assert!(!redraw);

        assert!(input.try_activate(None, "a", 1, false, (0, 0, false)));
        redraw = false;
        handle_input_key(make_key(KeyCode::Char('\u{301}')), &mut input, &mut redraw);
        assert_eq!(input.current_value, "a\u{301}");
        assert_eq!(input.cursor_pos, 1);
        assert!(redraw);
        handle_input_key(make_key(KeyCode::Backspace), &mut input, &mut redraw);
        assert!(input.current_value.is_empty());
        assert_eq!(input.cursor_pos, 0);

        assert!(input.try_activate(None, "retry", 20, false, (0, 0, false)));
        let retry_ptr = input.current_value.as_ptr();
        redraw = false;
        reject_next_input_value_allocation(InputValueAllocationSite::Projection);
        handle_input_key(make_key(KeyCode::Char('!')), &mut input, &mut redraw);
        assert_eq!(input.current_value, "retry");
        assert_eq!(input.current_value.as_ptr(), retry_ptr);
        assert_eq!(input.cursor_pos, 5);
        assert!(!redraw);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn forms_own_their_controls_and_selects_are_submitted() {
        let mut page = page_from(
            r#"[page mode=document]
                [form action="/one"]
                  [input name="password" password value="secret" /]
                  [input name="tag" value="first" /]
                  [input name="tag" value="second" /]
                  [select name="choice"]
                    [option value="a"]A[/option]
                    [option value="b" selected]B[/option]
                  [/select]
                  [button action=submit]One[/button]
                [/form]
                [form action="/two"]
                  [input name="message" value="hello" /]
                  [button action=submit]Two[/button]
                [/form]
            [/page]"#,
        )
        .await;

        let forms: Vec<_> = page
            .scene
            .iter_tree_order()
            .filter(|node| {
                matches!(
                    node.kind(),
                    NodeKind::Flow(data)
                        if matches!(data.source, crate::compositor::scene::FlowSource::Form)
                )
            })
            .map(|node| node.id())
            .collect();
        assert_eq!(forms.len(), 2);
        assert_eq!(form_action(&page.scene, forms[0]).as_deref(), Some("/one"));
        assert_eq!(
            collect_form_values(&page.scene, forms[0]).unwrap(),
            vec![
                ("password".into(), "secret".into()),
                ("tag".into(), "first".into()),
                ("tag".into(), "second".into()),
                ("choice".into(), "b".into()),
            ]
        );
        assert_eq!(
            collect_form_values(&page.scene, forms[1]).unwrap(),
            vec![("message".into(), "hello".into())]
        );

        let select = page
            .scene
            .iter_subtree(forms[0])
            .find(|node| matches!(node.kind(), NodeKind::Select(_)))
            .unwrap()
            .id();
        advance_select(&mut page.scene, select);
        assert!(
            collect_form_values(&page.scene, forms[0])
                .unwrap()
                .contains(&("choice".into(), "a".into()))
        );

        let focusables = crate::compositor::panels::collect_focusables_from_scene(&page.scene);
        assert!(focusables.iter().any(|focusable| {
            matches!(
                focusable.action,
                crate::compositor::panels::FocusAction::EditSelect { form: Some(id) }
                    if id == forms[0]
            )
        }));
        let submit_forms: Vec<_> = focusables
            .iter()
            .filter_map(|focusable| match focusable.action {
                crate::compositor::panels::FocusAction::Submit { form, .. } => form,
                _ => None,
            })
            .collect();
        assert_eq!(submit_forms, forms);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn live_update_reaches_scene_composite() {
        let mut page = page_from(
            r#"[page mode=document]
                [box w=20 h=6 border=rounded bg=black padding=1]
                    [live id="feed" endpoint="/feed" height=2][text]waiting[/text][/live]
                [/box]
            [/page]"#,
        )
        .await;
        assert!(page.live_regions().any(|placed| placed.id == "feed"));
        let update = UpdateMessage {
            region: "feed".into(),
            content: "[text]UPDATED[/text]".into(),
            flags: UpdateFlags::default(),
        };
        let mut buffers = RegionBuffers::new();
        let region = SubscriptionRegionKey::from_placed_index(
            page.placed
                .iter()
                .position(|placed| placed.is_live() && placed.id == "feed")
                .unwrap(),
        )
        .unwrap();
        apply_live_update(
            &update,
            region,
            &page.placed,
            &mut page.scene,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            &mut buffers,
            &page.governor,
        );

        let live = page.scene.find_by_aml_id("feed").unwrap();
        let updated = page.scene.buffer_of(live).unwrap().get(0, 0).unwrap();
        assert_eq!(
            updated.style.bg,
            Some(crate::color::ResolvedColor::Rgb(0, 0, 0)),
            "live update should inherit the enclosing box background",
        );

        let rendered = crate::compositor::present::render_to_string(&composite(&page));
        assert!(
            rendered.contains("UPDATED"),
            "live pixels missing: {rendered:?}"
        );
        assert!(!page.scene.invalidation.composite.is_empty());
    }

    #[test]
    fn live_region_rows_are_exactly_leased_and_released() {
        let governor = ResourceGovernor::new();
        let mut retained = RegionBuffer::new(4, 2, 3, governor.clone());
        let mut mini = CellBuffer::new(4, 2);
        mini.put_char(0, 0, 'a', &CellStyle::default());
        mini.put_char(0, 1, 'b', &CellStyle::default());

        assert!(retained.replace(&mini));
        assert_eq!(
            governor.used(ResourceCategory::SceneCells),
            retained.rows.capacity()
        );
        assert_eq!(
            governor.total_used(),
            retained.rows.capacity() * std::mem::size_of::<Cell>()
        );

        assert!(retained.append(&mini));
        assert_eq!(retained.row_count, 3);
        assert_eq!(retained.rows.len(), 12);
        assert_eq!(
            governor.used(ResourceCategory::SceneCells),
            retained.rows.capacity()
        );
        assert_eq!(
            governor.total_used(),
            retained.rows.capacity() * std::mem::size_of::<Cell>()
        );

        drop(retained);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
        assert_eq!(governor.total_used(), 0);
    }

    #[test]
    fn live_region_pressure_preserves_the_previous_rows() {
        let governor = ResourceGovernor::new();
        let mut retained = RegionBuffer::new(4, 2, 3, governor.clone());
        let mut mini = CellBuffer::new(4, 2);
        mini.put_char(0, 0, 'a', &CellStyle::default());
        mini.put_char(0, 1, 'b', &CellStyle::default());
        assert!(retained.replace(&mini));

        let requested_replacement = 12;
        let old_cells = retained.rows.capacity();
        let pressure_cells = crate::resource::MAX_SCENE_CELLS
            .saturating_sub(old_cells)
            .saturating_sub(requested_replacement)
            .saturating_add(1);
        let _pressure = governor
            .reserve_with_cost(ResourceCategory::SceneCells, pressure_cells, 0)
            .unwrap();

        assert!(!retained.append(&mini));
        assert_eq!(retained.row_count, 2);
        assert_eq!(retained.rows.len(), 8);
        assert_eq!(retained.rows[0].ch, 'a');
        assert_eq!(retained.rows[4].ch, 'b');
        assert_eq!(
            governor.used(ResourceCategory::SceneCells),
            old_cells + pressure_cells,
            "failed admission must retain only the old rows and pressure lease",
        );
    }

    #[test]
    fn live_region_flat_rows_preserve_append_prepend_and_trim_order() {
        let governor = ResourceGovernor::new();
        let mut retained = RegionBuffer::new(2, 2, 3, governor);
        let mut first = CellBuffer::new(2, 2);
        first.put_char(0, 0, 'a', &CellStyle::default());
        first.put_char(0, 1, 'b', &CellStyle::default());
        let mut second = CellBuffer::new(2, 2);
        second.put_char(0, 0, 'c', &CellStyle::default());
        second.put_char(0, 1, 'd', &CellStyle::default());

        assert!(retained.replace(&first));
        assert!(retained.append(&second));
        assert_eq!(retained.row_count, 3);
        assert_eq!(retained.rows[0].ch, 'b');
        assert_eq!(retained.rows[2].ch, 'c');
        assert_eq!(retained.rows[4].ch, 'd');

        assert!(retained.rebuild(&second, RegionBufferUpdate::Prepend));
        assert_eq!(retained.row_count, 3);
        assert_eq!(retained.rows[0].ch, 'c');
        assert_eq!(retained.rows[2].ch, 'd');
        assert_eq!(retained.rows[4].ch, 'b');
    }

    #[test]
    fn fixed_region_table_rejects_capacity_and_failed_first_update_atomically() {
        let governor = ResourceGovernor::new();
        let mut mini = CellBuffer::new(1, 1);
        mini.put_char(0, 0, 'x', &CellStyle::default());
        let mut table = RegionBuffers::new();
        for index in 0..crate::client::MAX_ACTIVE_SUBSCRIPTIONS {
            assert!(table.update(
                SubscriptionRegionKey::from_placed_index(index).unwrap(),
                1,
                1,
                1,
                &governor,
                &mini,
                RegionBufferUpdate::Replace,
            ));
        }
        assert_eq!(table.len(), crate::client::MAX_ACTIVE_SUBSCRIPTIONS);
        assert!(
            !table.update(
                SubscriptionRegionKey::from_placed_index(crate::client::MAX_ACTIVE_SUBSCRIPTIONS)
                    .unwrap(),
                1,
                1,
                1,
                &governor,
                &mini,
                RegionBufferUpdate::Replace,
            )
        );
        assert_eq!(table.len(), crate::client::MAX_ACTIVE_SUBSCRIPTIONS);
        drop(table);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
        assert_eq!(governor.count(ResourceCategory::SceneCells), 0);

        let pressured = ResourceGovernor::new();
        let _pressure = pressured
            .reserve_with_cost(
                ResourceCategory::SceneCells,
                crate::resource::MAX_SCENE_CELLS,
                0,
            )
            .unwrap();
        let mut rejected = RegionBuffers::new();
        assert!(!rejected.update(
            SubscriptionRegionKey::from_placed_index(0).unwrap(),
            1,
            1,
            1,
            &pressured,
            &mini,
            RegionBufferUpdate::Replace,
        ));
        assert_eq!(rejected.len(), 0);
    }

    #[test]
    fn region_configuration_pressure_preserves_exact_entry_then_replaces_it() {
        let governor = ResourceGovernor::new();
        let key = SubscriptionRegionKey::from_placed_index(3).unwrap();
        let mut old = CellBuffer::new(2, 1);
        old.put_char(0, 0, 'o', &CellStyle::default());
        let mut replacement = CellBuffer::new(3, 1);
        replacement.put_char(0, 0, 'n', &CellStyle::default());
        let mut table = RegionBuffers::new();
        assert!(table.update(key, 2, 1, 1, &governor, &old, RegionBufferUpdate::Replace,));
        let old_capacity = table.get(key).unwrap().rows.capacity();
        let old_ptr = table.get(key).unwrap().rows.as_ptr();
        let requested = 3;
        let pressure_cells = crate::resource::MAX_SCENE_CELLS
            .saturating_sub(old_capacity)
            .saturating_sub(requested)
            .saturating_add(1);
        let pressure = governor
            .reserve_with_cost(ResourceCategory::SceneCells, pressure_cells, 0)
            .unwrap();

        assert!(!table.update(
            key,
            3,
            1,
            2,
            &governor,
            &replacement,
            RegionBufferUpdate::Replace,
        ));
        let retained = table.get(key).unwrap();
        assert_eq!(retained.width, 2);
        assert_eq!(retained.rows.as_ptr(), old_ptr);
        assert_eq!(retained.rows[0].ch, 'o');
        assert_eq!(
            governor.used(ResourceCategory::SceneCells),
            old_capacity + pressure_cells
        );

        drop(pressure);
        assert!(table.update(
            key,
            3,
            1,
            2,
            &governor,
            &replacement,
            RegionBufferUpdate::Replace,
        ));
        let retained = table.get(key).unwrap();
        assert_eq!(retained.width, 3);
        assert_eq!(retained.rows[0].ch, 'n');
        assert_eq!(
            governor.used(ResourceCategory::SceneCells),
            retained.rows.capacity()
        );
        drop(table);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
    }

    #[test]
    fn zero_width_live_region_retains_no_rows_or_lease() {
        let governor = ResourceGovernor::new();
        let mini = CellBuffer::new(1, 2);
        let mut retained = RegionBuffer::new(0, 2, u32::MAX, governor.clone());
        assert!(retained.replace(&mini));
        assert_eq!(retained.row_count, 0);
        assert!(retained.rows.is_empty());
        assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
        assert_eq!(governor.count(ResourceCategory::SceneCells), 0);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn sticky_pixels_are_extracted_from_scene_composite() {
        let mut page = page_from(
            r#"[page mode=document][text]main[/text][nav sticky=bottom][text]STICKY[/text][/nav][/page]"#,
        )
        .await;
        assert!(page.sticky_buf.is_some());
        let full = composite(&page);
        refresh_sticky_buffer(&mut page, &full);
        let sticky = page.sticky_buf.as_ref().unwrap();
        let rendered = crate::compositor::present::render_to_string(sticky);
        assert!(
            rendered.contains("STICKY"),
            "sticky pixels missing: {rendered:?}"
        );
        assert_eq!(
            page.buf.height, 1,
            "main document should stop before sticky region"
        );
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn background_animation_follows_scrolled_viewport() {
        let mut page = page_from(
            r#"[page mode=document]
                [animate id="bg" background=true fps=10]
                    [frame][text].[/text][/frame]
                [/animate]
                [spacer lines=30 /]
            [/page]"#,
        )
        .await;
        let bg = page.scene.find_by_aml_id("bg").unwrap();
        let buffer = page.scene.wasm_buffer_mut(bg).unwrap();
        for y in 0..buffer.height {
            for x in 0..buffer.width {
                buffer.put_char(x, y, '.', &CellStyle::default());
            }
        }

        let mut compositor = crate::compositor::composite::Compositor::new(40, page.buf.height);
        let top = compositor
            .composite_at(&page.scene, &page.anim_rt, 0)
            .unwrap();
        assert_eq!(top.get(0, 0).unwrap().ch, '.');
        assert_eq!(top.get(0, 15).unwrap().ch, ' ');

        let scrolled = compositor
            .composite_at(&page.scene, &page.anim_rt, 10)
            .unwrap();
        assert_eq!(scrolled.get(0, 10).unwrap().ch, '.');
        assert_eq!(scrolled.get(0, 15).unwrap().ch, '.');
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn page_transition_snapshots_do_not_reuse_the_previous_scene() {
        let old_page =
            page_from(r#"[page mode=document][text]OLD PAGE[/text][spacer lines=20 /][/page]"#)
                .await;
        let new_page =
            page_from(r#"[page mode=document][text]NEW PAGE[/text][spacer lines=20 /][/page]"#)
                .await;
        assert_eq!(old_page.buf.height, new_page.buf.height);

        let state = ViewportState::new(40, 12, old_page.buf.height);
        let governor = ResourceGovernor::new();
        let mut compositor = crate::compositor::composite::Compositor::with_governor(
            40,
            old_page.buf.height,
            governor.clone(),
        );
        let old = capture_viewport_snapshot(&old_page, &mut compositor, &state, &governor)
            .expect("transition snapshot budget should fit");
        let viewport_bytes = 40 * 12 * std::mem::size_of::<Cell>();
        let new_lease = new_page
            .governor
            .reserve(ResourceCategory::CompositorCells, viewport_bytes)
            .unwrap();
        let new = build_new_page_snapshot(&new_page, &mut compositor, &state, new_lease)
            .expect("new transition snapshot budget should fit");

        let old_text = crate::compositor::present::render_to_string(&old.old_snapshot);
        let new_text = crate::compositor::present::render_to_string(&new);
        assert!(old_text.contains("OLD PAGE"));
        assert!(new_text.contains("NEW PAGE"));
        assert!(!new_text.contains("OLD PAGE"));
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn page_transition_reserves_source_snapshot_before_capture() {
        let page = page_from(r#"[page mode=document][text]OLD PAGE[/text][/page]"#).await;
        let state = ViewportState::new(40, 12, page.buf.height);
        let governor = ResourceGovernor::new();
        let viewport_bytes = 40 * 12 * std::mem::size_of::<Cell>();
        let required = viewport_bytes;
        let _pressure = governor
            .reserve(
                ResourceCategory::CompositorCells,
                crate::resource::MAX_REMOTE_MEMORY - required + 1,
            )
            .unwrap();
        let mut compositor = crate::compositor::composite::Compositor::with_governor(
            40,
            page.buf.height,
            governor.clone(),
        );

        assert!(
            capture_viewport_snapshot(&page, &mut compositor, &state, &governor).is_none(),
            "transition admission must fail before capturing the old page",
        );
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            crate::resource::MAX_REMOTE_MEMORY - required + 1,
            "failed pre-admission must not retain a partial snapshot lease",
        );
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn new_scene_invalidation_drops_equal_sized_page_cache() {
        let old_page = page_from(r#"[page mode=document][text]OLD PAGE[/text][/page]"#).await;
        let mut new_page = page_from(r#"[page mode=document][text]NEW PAGE[/text][/page]"#).await;
        let mut compositor = Compositor::new(40, old_page.buf.height);

        let old_frame = compositor
            .composite(&old_page.scene, &old_page.anim_rt)
            .unwrap();
        assert_eq!(old_frame.get(0, 0).unwrap().ch, 'O');

        // A newly built equal-sized scene can legitimately have no pending
        // dirty regions, so scene identity—not dimensions—must reset caches.
        new_page.scene.invalidation.clear();
        invalidate_compositor_for_new_scene(&mut compositor);
        let new_frame = compositor
            .composite(&new_page.scene, &new_page.anim_rt)
            .unwrap();

        assert_eq!(new_frame.get(0, 0).unwrap().ch, 'N');
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn panel_relayout_preserves_unchanged_background_animation() {
        let mut page = page_from(
            r#"[page mode=screen rows=24]
                [animate id="rain" background=true fps=10 loop=true]
                    [frame][text].[/text][/frame]
                [/animate]
                [panel id="directory" state="hidden"]
                    [state name="hidden"]
                        [box y=5 w=20 h=5 border=none padding=0 align=center][/box]
                    [/state]
                    [state name="visible" transition="draw-down" duration=1600ms]
                        [box y=5 w=20 h=5 bg=black align=center][text]Directory[/text][/box]
                    [/state]
                [/panel]
            [/page]"#,
        )
        .await;
        let rain = page.scene.find_by_aml_id("rain").unwrap();
        page.scene
            .wasm_buffer_mut(rain)
            .unwrap()
            .put_char(0, 0, 'R', &CellStyle::default());
        let adapter_before = (&*page.anim_rt.animations[0]) as *const dyn Animation as *const ();

        let old_panel = capture_panel_transition_source(&page, "directory");
        assert_eq!(old_panel.as_ref().unwrap().0, Rect::new(10, 5, 20, 5));
        assert!(
            old_panel
                .as_ref()
                .unwrap()
                .1
                .get(0, 4)
                .unwrap()
                .is_transparent()
        );
        assert!(apply_panel_patch(&mut page.scene, "directory", "visible"));
        let mut state = ViewportState::new(40, 12, page.buf.height);
        relayout_panels_for(
            &mut page,
            &mut state,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some("directory"),
            old_panel,
            None,
        )
        .await;

        let adapter_after = (&*page.anim_rt.animations[0]) as *const dyn Animation as *const ();
        assert_eq!(adapter_before, adapter_after);
        assert_eq!(
            page.scene.buffer_of(rain).unwrap().get(0, 0).unwrap().ch,
            'R'
        );
        assert_eq!(page.anim_rt.transition_animations.len(), 1);

        let directory = page.scene.find_by_aml_id("directory").unwrap();
        let rect = page.scene.get(directory).unwrap().placement().rect;
        assert_eq!(rect, Rect::new(10, 5, 20, 5));
        let first_frame = crate::compositor::composite::walk(
            &page.scene,
            &page.anim_rt,
            page.buf.width,
            page.buf.height,
        );
        assert!(first_frame.get(rect.x, rect.y).unwrap().is_transparent());

        for _ in 0..6 {
            page.anim_rt.tick(
                &mut page.scene,
                std::time::Instant::now(),
                0,
                state.viewport_height(),
            );
        }
        let partial_panel = page.scene.buffer_of(directory).unwrap();
        assert!(partial_panel.get(0, 0).unwrap().is_transparent());
        assert!(!partial_panel.get(rect.w / 2, 0).unwrap().is_transparent());
        assert!(partial_panel.get(rect.w - 1, 0).unwrap().is_transparent());
        let bottom_cell = partial_panel.get(0, rect.h - 1).unwrap();
        assert!(bottom_cell.is_transparent(), "bottom cell: {bottom_cell:?}");

        for _ in 0..19 {
            page.anim_rt.tick(
                &mut page.scene,
                std::time::Instant::now(),
                0,
                state.viewport_height(),
            );
        }
        let vertical_panel = page.scene.buffer_of(directory).unwrap();
        assert!(!vertical_panel.get(rect.w - 1, 0).unwrap().is_transparent());
        assert!(!vertical_panel.get(0, 1).unwrap().is_transparent());
        assert!(vertical_panel.get(0, rect.h - 1).unwrap().is_transparent());
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn panel_relayout_rebuilds_changed_animation_topology_after_layout() {
        let mut page = page_from(
            r#"[page mode=screen rows=24]
                [panel id="stage" state="a"]
                    [state name="a"]
                        [animate id="old" fps=10]
                            [frame][text]old[/text][/frame]
                        [/animate]
                    [/state]
                    [state name="b"]
                        [animate id="new" fps=10]
                            [frame][text]new[/text][/frame]
                        [/animate]
                    [/state]
                [/panel]
            [/page]"#,
        )
        .await;
        assert!(
            page.anim_rt
                .animations
                .iter()
                .any(|animation| animation.id() == "old")
        );
        assert!(
            !page
                .anim_rt
                .animations
                .iter()
                .any(|animation| animation.id() == "new")
        );

        assert!(apply_panel_patch(&mut page.scene, "stage", "b"));
        let mut state = ViewportState::new(40, 12, page.buf.height);
        assert!(
            relayout_panels_for(
                &mut page,
                &mut state,
                ColorSupport::Truecolor,
                WidthConfig::default(),
                Some("stage"),
                None,
                None,
            )
            .await
        );

        assert!(
            !page
                .anim_rt
                .animations
                .iter()
                .any(|animation| animation.id() == "old")
        );
        assert!(
            page.anim_rt
                .animations
                .iter()
                .any(|animation| animation.id() == "new")
        );
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn completed_panel_transition_stays_opaque_over_background_animation() {
        let mut page = page_from(
            r#"[page mode=screen cols=40 rows=12]
                [animate id="bg" background=true fps=10 loop=true]
                    [frame][text]XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX[/text][/frame]
                [/animate]
                [panel id="hero" state="hidden"]
                    [state name="hidden"]
                        [box x=5 y=1 w=30 h=8 border=none padding=0][/box]
                    [/state]
                    [state name="visible" transition="draw-down" duration=900ms]
                        [box x=5 y=1 w=30 h=8 border=double bg=black padding=1 align=center]
                            [text]NETWORK[/text]
                        [/box]
                    [/state]
                [/panel]
            [/page]"#,
        )
        .await;
        let background = page.scene.find_by_aml_id("bg").unwrap();
        let background_buf = page.scene.wasm_buffer_mut(background).unwrap();
        for y in 0..background_buf.height {
            for x in 0..background_buf.width {
                background_buf.put_char(x, y, 'X', &CellStyle::default());
            }
        }
        let mut state = ViewportState::new(40, 14, page.buf.height);
        let old_panel = capture_panel_transition_source(&page, "hero");
        assert!(apply_panel_patch(&mut page.scene, "hero", "visible"));
        relayout_panels_for(
            &mut page,
            &mut state,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some("hero"),
            old_panel,
            None,
        )
        .await;

        for _ in 0..30 {
            page.anim_rt.tick(
                &mut page.scene,
                std::time::Instant::now(),
                0,
                state.viewport_height(),
            );
        }

        let frame = crate::compositor::composite::walk(
            &page.scene,
            &page.anim_rt,
            page.buf.width,
            page.buf.height,
        );
        assert_eq!(frame.get(5, 1).unwrap().ch, '╔');
        assert_eq!(frame.get(6, 2).unwrap().ch, ' ');
        assert_eq!(frame.get(7, 2).unwrap().ch, ' ');
        assert_eq!(frame.get(0, 1).unwrap().ch, 'X');
    }
}

fn make_key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

// ─── ViewportState basics ──────────────────────────────────
// viewport_height = term_h - 2 (status bar + command line)

#[test]
fn new_state() {
    let state = ViewportState::new(80, 24, 100);
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.term_w, 80);
    assert_eq!(state.term_h, 24);
    assert_eq!(state.content_height, 100);
    assert_eq!(state.viewport_height(), 22);
    assert_eq!(state.max_scroll(), 78);
    assert!(state.scrollable());
}

#[test]
fn non_scrollable_when_content_fits() {
    let state = ViewportState::new(80, 24, 10);
    assert!(!state.scrollable());
    assert_eq!(state.max_scroll(), 0);
}

#[test]
fn content_exactly_fills_viewport() {
    let state = ViewportState::new(80, 24, 22); // viewport is 22
    assert!(!state.scrollable());
    assert_eq!(state.max_scroll(), 0);
}

#[test]
fn content_one_more_than_viewport() {
    let state = ViewportState::new(80, 24, 23); // viewport 22, content 23
    assert!(state.scrollable());
    assert_eq!(state.max_scroll(), 1);
}

// ─── Scroll down ─────────────────────────────────────────

#[test]
fn scroll_down_basic() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(state.scroll_down(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 1);
}

#[test]
fn scroll_down_at_bottom() {
    let mut state = ViewportState::new(80, 24, 23); // max_scroll = 1
    state.scroll_offset = 1;
    assert_eq!(state.scroll_down(), ViewerAction::None);
    assert_eq!(state.scroll_offset, 1);
}

#[test]
fn scroll_down_not_scrollable() {
    let mut state = ViewportState::new(80, 24, 10);
    assert_eq!(state.scroll_down(), ViewerAction::None);
    assert_eq!(state.scroll_offset, 0);
}

// ─── Scroll up ───────────────────────────────────────────

#[test]
fn scroll_up_basic() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 5;
    assert_eq!(state.scroll_up(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 4);
}

#[test]
fn scroll_up_at_top() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(state.scroll_up(), ViewerAction::None);
    assert_eq!(state.scroll_offset, 0);
}

// ─── Page down / up ──────────────────────────────────────

#[test]
fn page_down() {
    let mut state = ViewportState::new(80, 24, 100); // viewport 22
    assert_eq!(state.page_down(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 22);
}

#[test]
fn page_down_clamps_to_max() {
    let mut state = ViewportState::new(80, 24, 100); // max_scroll = 78
    state.scroll_offset = 70;
    assert_eq!(state.page_down(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 78);
}

#[test]
fn page_down_at_bottom() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 78;
    assert_eq!(state.page_down(), ViewerAction::None);
}

#[test]
fn page_up() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 50;
    assert_eq!(state.page_up(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 28);
}

#[test]
fn page_up_clamps_to_zero() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 10;
    assert_eq!(state.page_up(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn page_up_at_top() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(state.page_up(), ViewerAction::None);
}

// ─── Home / End ──────────────────────────────────────────

#[test]
fn scroll_home() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 50;
    assert_eq!(state.scroll_home(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn scroll_home_already_at_top() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(state.scroll_home(), ViewerAction::None);
}

#[test]
fn scroll_end() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(state.scroll_end(), ViewerAction::Redraw);
    assert_eq!(state.scroll_offset, 78);
}

#[test]
fn scroll_end_already_at_bottom() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 78;
    assert_eq!(state.scroll_end(), ViewerAction::None);
}

// ─── Resize ──────────────────────────────────────────────

#[test]
fn resize_updates_dimensions() {
    let mut state = ViewportState::new(80, 24, 100);
    state.handle_resize(120, 40, 80);
    assert_eq!(state.term_w, 120);
    assert_eq!(state.term_h, 40);
    assert_eq!(state.content_height, 80);
    assert_eq!(state.viewport_height(), 38);
}

#[test]
fn resize_clamps_scroll_offset() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 78; // at bottom for 24-row terminal

    // Resize to much taller terminal: max_scroll shrinks
    state.handle_resize(80, 80, 100);
    // viewport=78, max_scroll=22
    assert_eq!(state.scroll_offset, 22);
}

#[test]
fn resize_to_taller_than_content() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 50;

    // Terminal is now taller than content
    state.handle_resize(80, 200, 100);
    assert_eq!(state.max_scroll(), 0);
    assert_eq!(state.scroll_offset, 0);
    assert!(!state.scrollable());
}

#[test]
fn resize_narrower_relayouts_taller_content() {
    let mut state = ViewportState::new(80, 24, 50);
    state.scroll_offset = 0;

    // Narrower terminal -> text wraps more -> more content lines
    state.handle_resize(40, 24, 80);
    assert_eq!(state.content_height, 80);
    assert_eq!(state.max_scroll(), 58);
    assert!(state.scrollable());
}

#[test]
fn resize_preserves_scroll_when_possible() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 30;

    // Slight resize -- scroll should stay the same
    state.handle_resize(85, 24, 100);
    assert_eq!(state.scroll_offset, 30);
}

#[test]
fn reducer_coalesces_noop_resize() {
    let mut model = ViewportState::new(80, 24, 100);
    assert!(
        model
            .transition(ViewportEvent::Resize {
                width: 80,
                height: 24,
            })
            .is_empty()
    );
    assert_eq!(
        model.transition(ViewportEvent::Resize {
            width: 100,
            height: 30,
        }),
        vec![ViewportEffect::Relayout {
            width: 100,
            height: 30,
        }]
    );
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn resize_projection_pressure_rolls_back_and_retries_exact_dimensions() {
    let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
    let uri = AtpUri::parse("atp://127.0.0.1/resize").unwrap();
    let origin = client.request_origin(&uri).unwrap();
    let scope = crate::viewer::PageScope {
        origin,
        generation: 4,
    };
    let page = layout_page(
        parse_aml(
            "[page mode=document title=Resize]\n\
             [text]a remotely governed line that wraps during resize[/text]\n\
             [link id=\"next\" href=\"/next\"][text]next[/text][/link]\n\
             [/page]",
        )
        .unwrap(),
        80,
        24,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        Some(&mut client),
        Some(uri),
        None,
    )
    .await;
    let governor = client.governor.clone();
    let mut runtime = TerminalRuntime::new(
        page,
        Some(client),
        Vec::new(),
        80,
        24,
        ColorSupport::Truecolor,
        WidthConfig::default(),
    );
    runtime.needs_redraw = false;
    runtime.render_authorized = false;
    let mut model = LifecycleModel::new(80, 24);
    model.scope = Some(scope.clone());
    let mut lifecycle = ReducerPort::new(model);
    let effect = dispatch_event(
        &mut lifecycle,
        LifecycleEvent::Resize {
            width: 38,
            height: 17,
        },
    )
    .into_iter()
    .next()
    .unwrap();
    let owner = match &effect {
        LifecycleEffect::PrepareResizeProjection { owner, .. } => owner.clone(),
        _ => panic!("expected resize projection"),
    };
    let old_state = runtime.state.clone();
    let old_buffer = (runtime.page.buf.width, runtime.page.buf.height);
    let old_focusables = runtime
        .page
        .focusables
        .iter()
        .map(|focusable| {
            (
                focusable.node_id,
                focusable.col,
                focusable.row,
                focusable.width,
            )
        })
        .collect::<Vec<_>>();
    let old_placed = runtime
        .page
        .placed
        .iter()
        .map(|placed| (placed.id.clone(), placed.rect))
        .collect::<Vec<_>>();
    let remaining = crate::resource::MAX_REMOTE_MEMORY - governor.total_used();
    let pressure = governor
        .reserve(ResourceCategory::RemoteCollections, remaining)
        .unwrap();

    let rejected = runtime.execute(effect, &lifecycle).await.unwrap();
    assert!(matches!(
        rejected.as_slice(),
        [LifecycleEvent::PresentationFailed {
            retry: Some(PressureRetry::ResizeProjection {
                owner: retry_owner,
                width: 38,
                height: 17,
            }),
            ..
        }] if retry_owner == &owner
    ));
    assert_eq!(runtime.state.term_w, old_state.term_w);
    assert_eq!(runtime.state.term_h, old_state.term_h);
    assert_eq!(runtime.state.scroll_offset, old_state.scroll_offset);
    assert_eq!(
        (runtime.page.buf.width, runtime.page.buf.height),
        old_buffer
    );
    assert_eq!(
        runtime
            .page
            .focusables
            .iter()
            .map(|focusable| (
                focusable.node_id,
                focusable.col,
                focusable.row,
                focusable.width
            ))
            .collect::<Vec<_>>(),
        old_focusables
    );
    assert_eq!(
        runtime
            .page
            .placed
            .iter()
            .map(|placed| (placed.id.clone(), placed.rect))
            .collect::<Vec<_>>(),
        old_placed
    );
    assert!(!runtime.needs_redraw);
    assert!(!runtime.render_authorized);
    assert!(runtime.prepared_layout.is_none());

    drop(pressure);
    let prepared = runtime
        .execute(
            LifecycleEffect::PrepareResizeProjection {
                owner: owner.clone(),
                width: 38,
                height: 17,
            },
            &lifecycle,
        )
        .await
        .unwrap();
    assert!(matches!(
        prepared.as_slice(),
        [LifecycleEvent::ResizeProjectionPrepared {
            owner: prepared_owner,
            ..
        }] if prepared_owner == &owner
    ));
    assert_eq!(runtime.state.term_w, 38);
    assert_eq!(runtime.state.term_h, 17);
    assert_eq!(runtime.page.buf.width, 38);
    assert!(runtime.prepared_layout(&owner).is_some());
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn adapter_resize_pressure_rolls_back_projection_until_exact_retry() {
    struct ResizeGate {
        reject: std::rc::Rc<std::cell::Cell<bool>>,
        commits: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl crate::compositor::animate::Animation for ResizeGate {
        fn id(&self) -> &str {
            "resize-gate"
        }
        fn advance(
            &mut self,
            _ctx: &mut crate::compositor::animate::AdvanceCtx,
        ) -> crate::compositor::animate::AdvanceResult {
            crate::compositor::animate::AdvanceResult::none()
        }
        fn finished(&self) -> bool {
            false
        }
        fn state(&self) -> crate::compositor::animate::AnimState {
            crate::compositor::animate::AnimState::Running
        }
        fn prepare_resize(
            &self,
            _scene: &crate::compositor::scene::Scene,
        ) -> Result<
            Option<crate::compositor::animate::AnimationResizeCandidate>,
            crate::compositor::animate::AnimationResizeRejected,
        > {
            if self.reject.get() {
                return Err(crate::compositor::animate::AnimationResizeRejected);
            }
            Ok(Some(crate::compositor::animate::AnimationResizeCandidate {
                width: 1,
                height: 1,
                swap_buffer: CellBuffer::new(1, 1),
                output_buffer: CellBuffer::new(1, 1),
            }))
        }
        fn commit_resize(
            &mut self,
            _scene: &mut crate::compositor::scene::Scene,
            _candidate: crate::compositor::animate::AnimationResizeCandidate,
        ) {
            self.commits.set(self.commits.get() + 1);
        }
    }

    let mut page = layout_page(
        parse_aml("[page mode=document][text]resize gate[/text][/page]").unwrap(),
        80,
        24,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        None,
        None,
        None,
    )
    .await;
    let reject = std::rc::Rc::new(std::cell::Cell::new(true));
    let commits = std::rc::Rc::new(std::cell::Cell::new(0));
    page.anim_rt = AnimationRuntime::new(vec![Box::new(ResizeGate {
        reject: reject.clone(),
        commits: commits.clone(),
    })]);
    let mut runtime = TerminalRuntime::new(
        page,
        None,
        Vec::new(),
        80,
        24,
        ColorSupport::Truecolor,
        WidthConfig::default(),
    );
    let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
    let uri = AtpUri::parse("atp://127.0.0.1/resize-gate").unwrap();
    let scope = crate::viewer::PageScope {
        origin: client.request_origin(&uri).unwrap(),
        generation: 7,
    };
    let mut model = LifecycleModel::new(80, 24);
    model.scope = Some(scope);
    let mut lifecycle = ReducerPort::new(model);
    let effect = dispatch_event(
        &mut lifecycle,
        LifecycleEvent::Resize {
            width: 42,
            height: 18,
        },
    )
    .into_iter()
    .next()
    .unwrap();
    let owner = match &effect {
        LifecycleEffect::PrepareResizeProjection { owner, .. } => owner.clone(),
        _ => unreachable!(),
    };

    let failed = runtime.execute(effect, &lifecycle).await.unwrap();
    assert!(matches!(
        failed.as_slice(),
        [LifecycleEvent::PresentationFailed {
            retry: Some(PressureRetry::ResizeProjection {
                owner: retry_owner,
                width: 42,
                height: 18,
            }),
            ..
        }] if retry_owner == &owner
    ));
    assert_eq!((runtime.state.term_w, runtime.state.term_h), (80, 24));
    assert_eq!(runtime.page.buf.width, 80);
    assert_eq!(commits.get(), 0);
    assert!(runtime.prepared_layout.is_none());

    reject.set(false);
    let prepared = runtime
        .execute(
            LifecycleEffect::PrepareResizeProjection {
                owner: owner.clone(),
                width: 42,
                height: 18,
            },
            &lifecycle,
        )
        .await
        .unwrap();
    assert!(matches!(
        prepared.as_slice(),
        [LifecycleEvent::ResizeProjectionPrepared { owner: ready, .. }] if ready == &owner
    ));
    assert_eq!((runtime.state.term_w, runtime.state.term_h), (42, 18));
    assert_eq!(commits.get(), 1);
}

// ─── Key handling ────────────────────────────────────────

#[test]
fn key_q_quits_as_documented() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('q'))),
        ViewerAction::Quit
    );
}

#[test]
fn key_esc_clears_focus() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Esc)),
        ViewerAction::ClearFocus
    );
}

#[test]
fn key_ctrl_c_quits() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ViewerAction::Quit
    );
}

#[test]
fn key_j_scrolls_down() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('j'))),
        ViewerAction::Redraw
    );
    assert_eq!(state.scroll_offset, 1);
}

#[test]
fn key_k_scrolls_up() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 5;
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('k'))),
        ViewerAction::Redraw
    );
    assert_eq!(state.scroll_offset, 4);
}

#[test]
fn key_down_scrolls_down() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Down)),
        ViewerAction::Redraw
    );
    assert_eq!(state.scroll_offset, 1);
}

#[test]
fn key_space_pages_down() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char(' '))),
        ViewerAction::Redraw
    );
    assert_eq!(state.scroll_offset, 22);
}

#[test]
fn key_g_goes_home() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 50;
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('g'))),
        ViewerAction::Redraw
    );
    assert_eq!(state.scroll_offset, 0);
}

#[test]
#[allow(non_snake_case)]
fn key_G_goes_end() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('G'))),
        ViewerAction::Redraw
    );
    assert_eq!(state.scroll_offset, 78);
}

#[test]
fn unknown_key_does_nothing() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('x'))),
        ViewerAction::None
    );
    assert_eq!(state.scroll_offset, 0);
}

// ─── Back / Forward ─────────────────────────────────────

#[test]
fn key_left_goes_back() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Left)),
        ViewerAction::GoBack
    );
}

#[test]
fn key_h_goes_back() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('h'))),
        ViewerAction::GoBack
    );
}

#[test]
fn key_right_goes_forward() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Right)),
        ViewerAction::GoForward
    );
}

#[test]
fn key_l_goes_forward() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('l'))),
        ViewerAction::GoForward
    );
}

// ─── Command line keys ──────────────────────────────────

#[test]
fn key_colon_enters_command_mode() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char(':'))),
        ViewerAction::EnterCommandMode
    );
}

#[test]
fn key_o_enters_command_mode_open() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('o'))),
        ViewerAction::EnterCommandModeOpen
    );
}

#[test]
fn key_r_reloads_page() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('r'))),
        ViewerAction::Reload
    );
}

#[test]
fn key_question_mark_shows_help() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('?'))),
        ViewerAction::ShowHelp
    );
}

#[test]
fn key_backtick_opens_client_hud() {
    let mut state = ViewportState::new(80, 24, 100);
    assert_eq!(
        state.handle_key(make_key(KeyCode::Char('`'))),
        ViewerAction::ShowHud
    );
}

#[test]
fn client_hud_selects_and_opens_history_entries() {
    let mut hud = ClientHud::new();
    hud.toggle(2, 4);
    assert!(hud.is_active());
    assert_eq!(hud.history_selected, 2);

    assert_eq!(
        hud.handle_key(KeyCode::Up, 4, 0, 0),
        ClientHudAction::Redraw
    );
    assert_eq!(hud.history_selected, 1);
    assert_eq!(
        hud.handle_key(KeyCode::Enter, 4, 0, 0),
        ClientHudAction::OpenHistory(1)
    );
    assert!(!hud.target_open);
}

#[test]
fn client_hud_navigation_stops_at_ends() {
    let mut hud = ClientHud::new();
    hud.toggle(0, 3);
    hud.handle_key(KeyCode::Up, 3, 0, 0);
    assert_eq!(hud.history_selected, 0);
    hud.handle_key(KeyCode::End, 3, 0, 0);
    assert_eq!(hud.history_selected, 2);
    hud.handle_key(KeyCode::Down, 3, 0, 0);
    assert_eq!(hud.history_selected, 2);
    hud.handle_key(KeyCode::Home, 3, 0, 0);
    assert_eq!(hud.history_selected, 0);
}

#[test]
fn client_hud_renders_history_tab_titles_uris_and_current_marker() {
    let state = ViewportState::new(80, 24, 1);
    let uri = AtpUri::parse("atp://example.com/about").unwrap();
    let history = vec![HistoryEntry {
        id: 1,
        _retained_bytes: 0,
        _budget_lease: None,
        title: "About Example".into(),
        transition: None,
        transition_duration_ms: 0,
    }];
    let logical_history = vec![crate::viewer::HistoryEntry {
        id: 1,
        scope: crate::viewer::PageScope {
            origin: crate::protocol::origin::Origin::from_uri(
                &uri,
                crate::protocol::origin::TransportSecurity::VerifiedTls,
            )
            .unwrap(),
            generation: 1,
        },
        uri,
        retained_aml: String::from(""),
    }];
    let mut hud = ClientHud::new();
    hud.progress = 1.0;
    hud.target_open = true;
    let errors = ErrorLog::new();
    let mut output = Vec::new();
    write_client_hud(
        &mut output,
        &state,
        &hud,
        &errors,
        &history,
        &logical_history,
        0,
        &[],
    )
    .unwrap();
    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("[HISTORY]"));
    assert!(rendered.contains("ERRORS (0)"));
    assert!(rendered.contains("About Example"));
    assert!(rendered.contains("atp://example.com/about"));
    assert!(rendered.contains('◆'));
}

#[test]
fn error_log_groups_sanitizes_and_counts_runtime_failures() {
    let mut errors = ErrorLog::new();

    assert_eq!(
        errors.record("WASM memory limit\nexceeded"),
        (Some(0), true)
    );
    assert_eq!(
        errors.record("WASM memory limit\nexceeded"),
        (Some(0), false)
    );
    assert_eq!(errors.total_count(), 2);
    assert_eq!(errors.entries.len(), 1);
    assert_eq!(errors.entries[0].message, "WASM memory limit exceeded");
    assert_eq!(errors.entries[0].count, 2);

    errors.clear();
    assert_eq!(errors.total_count(), 0);
    assert!(errors.entries.is_empty());
    assert_eq!(errors.record("later failure"), (Some(0), false));
}

#[test]
fn error_log_messages_are_sanitized_and_bounded_fallibly() {
    let mut errors = ErrorLog::new();
    let hostile = format!("\x1b[31m{}\nignored", "界".repeat(1_000));
    assert_eq!(errors.record(&hostile), (Some(0), true));

    let message = &errors.entries[0].message;
    assert!(!message.contains('\x1b'));
    assert!(message.chars().count() <= MAX_ERROR_MESSAGE_CHARS);
    assert!(unicode_width::UnicodeWidthStr::width(message.as_str()) <= MAX_ERROR_MESSAGE_WIDTH);
    assert!(message.len() <= MAX_ERROR_MESSAGE_CHARS * 4);

    let exact_grapheme_boundary = format!("{}👩‍💻x", "界".repeat(255));
    assert_eq!(errors.record(&exact_grapheme_boundary), (Some(1), false));
    let message = &errors.entries[1].message;
    assert!(message.ends_with("👩‍💻"));
    assert!(!message.ends_with('x'));
    assert_eq!(unicode_width::UnicodeWidthStr::width(message.as_str()), 512);
}

#[test]
fn error_log_bounds_unique_session_entries() {
    let mut errors = ErrorLog::new();
    for index in 0..65 {
        errors.record(&format!("failure {index}"));
    }

    assert_eq!(errors.entries.len(), 64);
    assert_eq!(errors.entries.capacity(), MAX_ERROR_ENTRIES);
    assert_eq!(errors.entries.first().unwrap().message, "failure 0");
    assert_eq!(errors.entries.last().unwrap().message, "failure 63");
    assert_eq!(errors.total_count(), 65);
    assert_eq!(errors.omitted_count(), 1);

    assert_eq!(errors.record("failure 0"), (Some(0), false));
    assert_eq!(errors.entries.len(), 64);
    assert_eq!(errors.entries[0].count, 2);
    assert_eq!(errors.omitted_count(), 1);
}

#[test]
fn first_runtime_error_opens_errors_tab_only_once_per_session() {
    let mut errors = ErrorLog::new();
    let mut hud = ClientHud::new();

    assert!(record_runtime_notice(&mut errors, &mut hud, "first"));
    assert!(hud.target_open);
    assert_eq!(hud.tab, ClientHudTab::Errors);

    hud.close();
    assert!(!record_runtime_notice(&mut errors, &mut hud, "second"));
    assert!(!hud.target_open);

    errors.clear();
    assert!(!record_runtime_notice(&mut errors, &mut hud, "after clear"));
    assert!(!hud.target_open);
}

#[test]
fn client_hud_switches_tabs_and_clears_errors() {
    let mut hud = ClientHud::new();
    hud.toggle(0, 1);
    assert_eq!(hud.tab, ClientHudTab::History);

    assert_eq!(
        hud.handle_key(KeyCode::Tab, 1, 2, 0),
        ClientHudAction::Redraw
    );
    assert_eq!(hud.tab, ClientHudTab::Errors);
    hud.handle_key(KeyCode::End, 1, 2, 0);
    assert_eq!(hud.error_selected, 1);
    assert_eq!(
        hud.handle_key(KeyCode::Char('c'), 1, 2, 0),
        ClientHudAction::ClearErrors
    );
}

#[test]
fn client_hud_renders_grouped_error_counts() {
    let state = ViewportState::new(80, 24, 1);
    let mut errors = ErrorLog::new();
    errors.record("effect 'rain' stopped: WASM memory limit exceeded");
    errors.record("effect 'rain' stopped: WASM memory limit exceeded");
    let mut hud = ClientHud::new();
    hud.progress = 1.0;
    hud.target_open = true;
    hud.open_errors(Some(0));

    let mut output = Vec::new();
    write_client_hud(&mut output, &state, &hud, &errors, &[], &[], 0, &[]).unwrap();
    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("[ERRORS (2)]"));
    assert!(rendered.contains("×2"));
    assert!(rendered.contains("WASM memory limit exceeded"));
    assert!(rendered.contains("c clear"));
}

/// The Sessions tab shows what was restored, and never the token.
///
/// "restored 1 remembered session" is printed at startup from a count of lines
/// in a file, before any connection exists -- it cannot say which origin, or
/// whether a server still honours it. This tab is where that becomes answerable,
/// so it has to name the origin and show the expiry, and it must not show the
/// credential: a HUD is something people open while sharing a screen.
#[test]
fn client_hud_sessions_tab_names_origins_without_tokens() {
    let state = ViewportState::new(90, 12, 12);
    let mut hud = ClientHud::new();
    hud.progress = 1.0;
    hud.target_open = true;
    hud.tab = ClientHudTab::Sessions;
    let errors = ErrorLog::new();
    let sessions = [
        SessionRow {
            origin: "news.dustnet.io:1986".into(),
            security: "verified".into(),
            scope: "/".into(),
            expires_in: Some(46_862),
            persistent: true,
        },
        SessionRow {
            origin: "hub.dustnet.io:1987".into(),
            security: "pinned".into(),
            scope: "/submit".into(),
            expires_in: Some(-5),
            persistent: false,
        },
    ];
    let mut output = Vec::new();
    write_client_hud(&mut output, &state, &hud, &errors, &[], &[], 0, &sessions).unwrap();
    let rendered = String::from_utf8(output).unwrap();

    assert!(rendered.contains("[SESSIONS (2)]"), "tab must be selected");
    assert!(rendered.contains("news.dustnet.io:1986"));
    assert!(rendered.contains("hub.dustnet.io:1987"));
    assert!(rendered.contains("verified"));
    assert!(rendered.contains("/submit"), "scope must be shown");
    assert!(
        rendered.contains("in 13h"),
        "expiry as a duration, not an epoch"
    );
    assert!(
        rendered.contains("EXPIRED"),
        "a lapsed session is exactly what explains being logged out"
    );
    assert!(
        rendered.contains("remembered"),
        "persistence must be visible"
    );
}

/// An empty store says so, rather than looking like a broken tab.
#[test]
fn client_hud_sessions_tab_is_explicit_when_empty() {
    let state = ViewportState::new(90, 12, 12);
    let mut hud = ClientHud::new();
    hud.progress = 1.0;
    hud.target_open = true;
    hud.tab = ClientHudTab::Sessions;
    let errors = ErrorLog::new();
    let mut output = Vec::new();
    write_client_hud(&mut output, &state, &hud, &errors, &[], &[], 0, &[]).unwrap();
    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("No sessions"));
}

/// Tab cycles through all three, and BackTab goes the other way.
#[test]
fn client_hud_tab_cycles_three_tabs_both_ways() {
    let mut hud = ClientHud::new();
    assert_eq!(hud.tab, ClientHudTab::History);
    hud.handle_key(KeyCode::Tab, 0, 0, 0);
    assert_eq!(hud.tab, ClientHudTab::Errors);
    hud.handle_key(KeyCode::Tab, 0, 0, 0);
    assert_eq!(hud.tab, ClientHudTab::Sessions);
    hud.handle_key(KeyCode::Tab, 0, 0, 0);
    assert_eq!(hud.tab, ClientHudTab::History, "Tab wraps round");
    hud.handle_key(KeyCode::BackTab, 0, 0, 0);
    assert_eq!(hud.tab, ClientHudTab::Sessions, "BackTab goes backwards");
}

#[test]
fn client_hud_frame_is_committed_as_one_synchronized_update() {
    let mut output = Vec::new();
    write_synchronized_update(&mut output, b"page diff then opaque HUD").unwrap();

    assert_eq!(
        output, b"\x1b[?2026hpage diff then opaque HUD\x1b[?2026l",
        "the terminal must not see the underlying page and HUD as separate frames"
    );
}

#[test]
fn terminal_hud_frame_allocation_rejection_is_unpublished() {
    let mut terminal = Vec::new();
    let mut candidate = FallibleFrame::default();
    reject_next_terminal_frame_allocation();

    assert!(candidate.write_all(b"partial candidate").is_err());
    assert!(candidate.as_slice().is_empty());
    assert!(terminal.is_empty());

    candidate.write_all(b"complete candidate").unwrap();
    write_synchronized_update(&mut terminal, candidate.as_slice()).unwrap();
    assert_eq!(terminal, b"\x1b[?2026hcomplete candidate\x1b[?2026l");
}

#[test]
fn terminal_hud_text_rejection_restores_the_presented_baseline() {
    let width = 20;
    let height = 6;
    let buffer = SharedFrame::try_new(CellBuffer::new(width, 1)).unwrap();
    let mut compositor = Compositor::new(width, 1);
    let state = ViewportState::new(width, height, 1);
    let input_mode = InputMode {
        active: false,
        cursor_pos: 0,
        current_value: String::new(),
        current_node: None,
        maxlen: 0,
        password: false,
        field_col: 0,
        field_row: 0,
        field_is_sticky: false,
        wcfg: WidthConfig::default(),
    };
    let mut hud = ClientHud::new();
    hud.progress = 1.0;
    hud.target_open = true;
    let errors = ErrorLog::new();
    let config = ClientConfig::default();
    let mut terminal = Vec::new();

    reject_next_terminal_text_allocation();
    assert!(
        draw_viewer_frame(
            &mut terminal,
            &mut compositor,
            &buffer,
            &state,
            &[],
            None,
            "atp://example.com/",
            Some(dustnet_core::protocol::origin::TransportSecurity::VerifiedTls),
            &config,
            ColorSupport::Truecolor,
            "",
            true,
            &input_mode,
            &CommandLine::new(),
            &None,
            false,
            &hud,
            &errors,
            &[],
            &[],
            0,
            0,
            &[],
        )
        .is_err()
    );
    assert!(terminal.is_empty());

    draw_viewer_frame(
        &mut terminal,
        &mut compositor,
        &buffer,
        &state,
        &[],
        None,
        "atp://example.com/",
        Some(dustnet_core::protocol::origin::TransportSecurity::VerifiedTls),
        &config,
        ColorSupport::Truecolor,
        "",
        true,
        &input_mode,
        &CommandLine::new(),
        &None,
        false,
        &hud,
        &errors,
        &[],
        &[],
        0,
        0,
        &[],
    )
    .unwrap();
    assert!(terminal.starts_with(b"\x1b[?2026h"));
    assert!(terminal.windows(4).any(|window| window == b"\x1b[2J"));
}

#[test]
fn deferred_navigation_waits_until_final_frame_is_presented() {
    let uri = AtpUri::parse("atp://example.com/deferred").unwrap();
    let mut pending = DeferredNavigation {
        scope: crate::viewer::PageScope {
            origin: crate::protocol::origin::Origin::from_uri(
                &uri,
                crate::protocol::origin::TransportSecurity::VerifiedTls,
            )
            .unwrap(),
            generation: 7,
        },
        request_id: 42,
        wait_for: "exit".into(),
        action: crate::compositor::panels::FocusAction::None,
        ready: false,
        final_frame_presented: false,
    };

    assert!(!pending.can_resume(Some(&pending.scope)));
    pending.mark_animation_finished("exit");
    assert!(!pending.can_resume(Some(&pending.scope)));
    pending.mark_final_frame_presented();
    assert!(pending.can_resume(Some(&pending.scope)));
    let mut stale = pending.scope.clone();
    stale.generation += 1;
    assert!(!pending.can_resume(Some(&stale)));
}

#[test]
fn proposed_address_resolves_relative_link() {
    assert_eq!(
        resolve_proposed_address("atp://example.com/docs/start", "../about?tab=team").unwrap(),
        "atp://example.com/about?tab=team"
    );
}

#[test]
fn proposed_address_preserves_local_link() {
    assert_eq!(
        resolve_proposed_address("/tmp/index.aml", "next.aml").unwrap(),
        "next.aml"
    );
}

#[test]
fn proposed_address_preserves_invalid_links_and_canonicalizes_absolute_links() {
    assert_eq!(
        resolve_proposed_address("atp://example.com/docs/start", "../bad#fragment").unwrap(),
        "../bad#fragment"
    );
    assert_eq!(
        resolve_proposed_address("atp://example.com/docs/start", "atp://EXAMPLE.COM/a/../b")
            .unwrap(),
        "atp://example.com/b"
    );
}

#[test]
fn reload_clear_erases_terminal_before_full_redraw() {
    let mut out = Vec::new();
    let mut compositor = Compositor::new(80, 24);

    clear_terminal_for_full_redraw(&mut out, &mut compositor).unwrap();

    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains("\x1b[2J"));
    assert!(rendered.contains("\x1b[1;1H"));
}

#[test]
fn status_focus_includes_proposed_address() {
    let state = ViewportState::new(80, 24, 1);
    let vars = build_status_vars(
        &state,
        5,
        Some(0),
        Some("atp://example.com/next"),
        "atp://example.com/",
        Some(dustnet_core::protocol::origin::TransportSecurity::VerifiedTls),
        "",
        "",
        0,
    )
    .unwrap();

    assert_eq!(vars.focus, "[1/5] atp://example.com/next");
}

// ─── Resize + scroll interaction ─────────────────────────

#[test]
fn scroll_to_bottom_resize_smaller_scroll_back() {
    let mut state = ViewportState::new(80, 24, 100);

    // Scroll to bottom
    state.scroll_end();
    assert_eq!(state.scroll_offset, 78);

    // Resize terminal shorter -> more max_scroll
    state.handle_resize(80, 10, 100);
    // viewport=8, max_scroll=92
    assert_eq!(state.scroll_offset, 78); // still valid

    // Resize terminal very tall -> less max_scroll
    state.handle_resize(80, 50, 100);
    // viewport=48, max_scroll=52
    assert_eq!(state.max_scroll(), 52);
    assert_eq!(state.scroll_offset, 52); // clamped down
}

#[test]
fn scroll_down_after_resize_to_non_scrollable() {
    let mut state = ViewportState::new(80, 24, 100);
    state.scroll_offset = 50;

    // Resize so tall content fits
    state.handle_resize(80, 200, 100);
    assert_eq!(state.scroll_offset, 0);
    assert!(!state.scrollable());

    // Scroll should do nothing now
    assert_eq!(state.scroll_down(), ViewerAction::None);
    assert_eq!(state.page_down(), ViewerAction::None);
}

// ─── URL Encoding ──────────────────────────────────────

#[test]
fn url_encode_simple() {
    assert_eq!(super::url_encode("hello").unwrap(), "hello");
    assert_eq!(super::url_encode("hello world").unwrap(), "hello+world");
    assert_eq!(super::url_encode("a&b=c").unwrap(), "a%26b%3Dc");
}

#[test]
fn url_encode_form_values() {
    let values = vec![
        ("name".into(), "Alice".into()),
        ("msg".into(), "Hello World".into()),
    ];
    let encoded = super::url_encode_form(&values).unwrap();
    assert!(encoded.contains("name=Alice"));
    assert!(encoded.contains("msg=Hello+World"));
    assert!(encoded.contains('&'));
}

// ─── Command line ───────────────────────────────────────

#[test]
fn command_line_new_is_idle() {
    let cl = CommandLine::new();
    assert_eq!(cl.mode, CommandLineMode::Idle);
    assert!(cl.buffer.is_empty());
    assert_eq!(cl.cursor, 0);
}

#[test]
fn command_line_activate_and_cancel() {
    let mut cl = CommandLine::new();
    cl.activate("open ");
    assert_eq!(cl.mode, CommandLineMode::Input);
    assert_eq!(cl.buffer, "open ");
    assert_eq!(cl.cursor, 5);

    cl.cancel();
    assert_eq!(cl.mode, CommandLineMode::Idle);
    assert!(cl.buffer.is_empty());
    assert_eq!(cl.cursor, 0);
}

#[test]
fn command_line_typing() {
    let mut cl = CommandLine::new();
    cl.activate("");
    cl.handle_key(KeyCode::Char('q'));
    assert_eq!(cl.buffer, "q");
    assert_eq!(cl.cursor, 1);
}

#[test]
fn command_line_backspace() {
    let mut cl = CommandLine::new();
    cl.activate("abc");
    cl.handle_key(KeyCode::Backspace);
    assert_eq!(cl.buffer, "ab");
    assert_eq!(cl.cursor, 2);
}

#[test]
fn command_line_cursor_movement() {
    let mut cl = CommandLine::new();
    cl.activate("abc");
    assert_eq!(cl.cursor, 3);

    cl.handle_key(KeyCode::Left);
    assert_eq!(cl.cursor, 2);

    cl.handle_key(KeyCode::Left);
    assert_eq!(cl.cursor, 1);

    cl.handle_key(KeyCode::Right);
    assert_eq!(cl.cursor, 2);

    // Can't go past end
    cl.handle_key(KeyCode::Right);
    cl.handle_key(KeyCode::Right);
    assert_eq!(cl.cursor, 3);

    // Can't go before start
    cl.handle_key(KeyCode::Left);
    cl.handle_key(KeyCode::Left);
    cl.handle_key(KeyCode::Left);
    cl.handle_key(KeyCode::Left);
    assert_eq!(cl.cursor, 0);
}

#[test]
fn command_line_esc_cancels() {
    let mut cl = CommandLine::new();
    cl.activate("open foo");
    let result = cl.handle_key(KeyCode::Esc);
    assert!(result.is_none());
    assert_eq!(cl.mode, CommandLineMode::Idle);
    assert!(cl.buffer.is_empty());
}

#[test]
fn command_line_enter_returns_command() {
    let mut cl = CommandLine::new();
    cl.activate("q");
    let result = cl.handle_key(KeyCode::Enter);
    assert!(result.is_some());
    // After enter, command line is cancelled (back to idle)
    assert_eq!(cl.mode, CommandLineMode::Idle);
}

#[test]
fn command_line_message_display() {
    let mut cl = CommandLine::new();
    cl.set_message("error: connection refused", true);
    assert_eq!(cl.mode, CommandLineMode::Message);
    assert!(cl.is_error);
    assert_eq!(cl.message, "error: connection refused");
}

#[test]
fn command_line_message_strips_terminal_controls_and_newlines() {
    let mut cl = CommandLine::new();
    cl.set_message("bad\x1b]52;c;Y2xpcGJvYXJk\x07\nnext", true);
    assert_eq!(cl.message, "bad next");
    assert!(!cl.message.contains('\x1b'));
}

#[test]
fn command_line_formatted_message_is_fallible_and_bounded() {
    struct FailingDisplay;

    impl std::fmt::Display for FailingDisplay {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Write::write_str(formatter, "partial")?;
            Err(std::fmt::Error)
        }
    }

    let mut cl = CommandLine::new();
    let remote = format!("\x1b[31m{}", "界".repeat(MAX_COMMAND_MESSAGE_BYTES));
    cl.set_message_args(format_args!("error: {remote}"), true);

    assert_eq!(cl.mode, CommandLineMode::Message);
    assert!(cl.is_error);
    assert!(cl.message.starts_with("error: "));
    assert!(!cl.message.contains('\x1b'));
    assert!(cl.message.len() <= MAX_COMMAND_MESSAGE_BYTES);
    assert!(cl.message.is_char_boundary(cl.message.len()));

    let prior = cl.message.clone();
    cl.set_message_args(format_args!("replacement: {FailingDisplay}"), false);
    assert_eq!(cl.mode, CommandLineMode::Message);
    assert!(cl.is_error);
    assert_eq!(cl.message, prior);
}

#[test]
fn command_line_unicode_editing_uses_character_boundaries() {
    let mut cl = CommandLine::new();
    cl.activate("");
    cl.handle_key(KeyCode::Char('é'));
    cl.handle_key(KeyCode::Char('界'));
    assert_eq!(cl.buffer, "é界");
    assert_eq!(cl.cursor, cl.buffer.len());
    cl.handle_key(KeyCode::Left);
    cl.handle_key(KeyCode::Backspace);
    assert_eq!(cl.buffer, "界");
    assert_eq!(cl.cursor, 0);
}

#[test]
fn display_truncation_preserves_graphemes_and_cell_width() {
    assert_eq!(truncate_to_display_width("a界b", 3), ("a界".into(), 3));
    assert_eq!(
        truncate_to_display_width("e\u{301}x", 1),
        ("e\u{301}".into(), 1)
    );
}

#[test]
fn command_line_message_clears_on_keypress() {
    let mut cl = CommandLine::new();
    cl.set_message("some info", false);
    assert_eq!(cl.mode, CommandLineMode::Message);

    let cleared = cl.clear_message_if_needed();
    assert!(cleared);
    assert_eq!(cl.mode, CommandLineMode::Idle);
    assert!(cl.message.is_empty());

    // Second call returns false
    let cleared = cl.clear_message_if_needed();
    assert!(!cleared);
}

// ─── Command parsing ────────────────────────────────────

#[test]
fn parse_command_quit() {
    assert!(matches!(parse_command("q"), ParsedCommand::Quit));
    assert!(matches!(parse_command("quit"), ParsedCommand::Quit));
    assert!(matches!(parse_command("  q  "), ParsedCommand::Quit));
}

#[test]
fn parse_command_reload() {
    assert!(matches!(parse_command("r"), ParsedCommand::Reload));
    assert!(matches!(parse_command("reload"), ParsedCommand::Reload));
}

#[test]
fn parse_command_help() {
    assert!(matches!(parse_command("h"), ParsedCommand::Help));
    assert!(matches!(parse_command("help"), ParsedCommand::Help));
    assert!(matches!(parse_command("  help  "), ParsedCommand::Help));
}

#[test]
fn parse_command_open() {
    match parse_command("o atp://example.com/") {
        ParsedCommand::Open(uri) => assert_eq!(uri, "atp://example.com/"),
        _ => panic!("expected Open"),
    }
    match parse_command("open atp://example.com/path") {
        ParsedCommand::Open(uri) => assert_eq!(uri, "atp://example.com/path"),
        _ => panic!("expected Open"),
    }
    match parse_command("  open   atp://foo  ") {
        ParsedCommand::Open(uri) => assert_eq!(uri, "atp://foo"),
        _ => panic!("expected Open"),
    }
}

#[test]
fn parse_command_sessions() {
    assert!(matches!(parse_command("sessions"), ParsedCommand::Sessions));
    assert!(matches!(parse_command("s"), ParsedCommand::Sessions));
    assert!(matches!(
        parse_command("sessions clear"),
        ParsedCommand::SessionsClear(None)
    ));
    assert!(matches!(
        parse_command("s clear"),
        ParsedCommand::SessionsClear(None)
    ));
    match parse_command("sessions clear dustnet.org:1985") {
        ParsedCommand::SessionsClear(Some(site)) => assert_eq!(site, "dustnet.org:1985"),
        _ => panic!("expected SessionsClear with site"),
    }
    match parse_command("s clear example.com:1985") {
        ParsedCommand::SessionsClear(Some(site)) => assert_eq!(site, "example.com:1985"),
        _ => panic!("expected SessionsClear with site"),
    }
}

#[test]
fn help_modal_renders_client_guidance_and_close_hint() {
    let state = ViewportState::new(80, 24, 1);
    let mut out = Vec::new();

    write_help_modal(&mut out, &state).unwrap();

    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains("DUSTNET CLIENT HELP"));
    assert!(rendered.contains('┌'));
    assert!(rendered.contains('─'));
    assert!(rendered.contains('┐'));
    assert!(rendered.contains('│'));
    assert!(rendered.contains('└'));
    assert!(rendered.contains('┘'));
    assert!(rendered.contains("m│\x1b[0;37;40m Navigate sites"));
    assert!(rendered.contains(":open <atp://...>"));
    assert!(rendered.contains("Tab / Shift-Tab"));
    assert!(rendered.contains("Esc, Enter, or q to close"));
}

#[test]
fn help_modal_stays_within_a_small_viewport() {
    let state = ViewportState::new(20, 8, 1);
    let mut out = Vec::new();

    write_help_modal(&mut out, &state).unwrap();

    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains(" HELP "));
    assert!(rendered.contains("Esc/q close"));
    assert!(!rendered.contains("\x1b[9;"));
}

#[test]
fn render_sessions_page_empty() {
    let store = crate::session::SessionStore::new();
    let aml = render_sessions_page(&store, false);
    assert!(aml.contains("No active sessions"));
    assert!(aml.contains("Held in memory only"));
}

#[test]
fn render_sessions_page_names_whether_sessions_are_remembered() {
    let store = crate::session::SessionStore::new();
    assert!(render_sessions_page(&store, true).contains("Remembered across launches"));
    assert!(render_sessions_page(&store, false).contains("Held in memory only"));
}

#[test]
fn render_sessions_page_with_sessions() {
    let mut store = crate::session::SessionStore::new();
    let uri = crate::protocol::uri::AtpUri::parse("atp://example.com/").unwrap();
    let origin = crate::protocol::origin::Origin::from_uri(
        &uri,
        crate::protocol::origin::TransportSecurity::VerifiedTls,
    )
    .unwrap();
    store.apply_directive(
        &origin,
        &crate::session::SessionDirective::Set {
            token: "tok123".into(),
            scope: "/".into(),
            expires: None,
        },
    );
    let aml = render_sessions_page(&store, false);
    assert!(aml.contains("verified-tls|example.com:1985"));
    assert!(aml.contains("no expiry"));
    assert!(aml.contains("1 session(s)"));
}

#[test]
fn parse_command_unknown() {
    assert!(matches!(parse_command("xyz"), ParsedCommand::Unknown(_)));
    assert!(matches!(parse_command(""), ParsedCommand::Unknown(_)));
}

// ─── Command history ────────────────────────────────────

#[test]
fn command_line_history_saved_on_enter() {
    let mut cl = CommandLine::new();
    cl.activate("");
    cl.handle_key(KeyCode::Char('r'));
    cl.handle_key(KeyCode::Enter);
    assert_eq!(cl.history.len(), 1);
    assert_eq!(cl.history[0], "r");

    // Second command
    cl.activate("");
    cl.handle_key(KeyCode::Char('q'));
    cl.handle_key(KeyCode::Enter);
    assert_eq!(cl.history.len(), 2);
    assert_eq!(cl.history[0], "r");
    assert_eq!(cl.history[1], "q");
}

#[test]
fn command_line_history_no_duplicates() {
    let mut cl = CommandLine::new();
    cl.activate("");
    cl.handle_key(KeyCode::Char('r'));
    cl.handle_key(KeyCode::Enter);

    cl.activate("");
    cl.handle_key(KeyCode::Char('r'));
    cl.handle_key(KeyCode::Enter);

    assert_eq!(cl.history.len(), 1);
    assert_eq!(cl.history[0], "r"); // not ["r", "r"]
}

#[test]
fn command_line_history_is_fixed_and_evicts_the_oldest_entry() {
    let mut cl = CommandLine::new();
    for index in 0..(MAX_COMMAND_HISTORY + 6) {
        cl.activate(&format!("open {index}"));
        cl.handle_key(KeyCode::Enter);
    }

    assert_eq!(cl.history.len(), MAX_COMMAND_HISTORY);
    assert_eq!(cl.history.capacity(), MAX_COMMAND_HISTORY);
    assert_eq!(cl.history.first().map(String::as_str), Some("open 6"));
    assert_eq!(cl.history.last().map(String::as_str), Some("open 69"));
}

#[test]
fn command_line_input_enforces_its_utf8_byte_bound() {
    let mut cl = CommandLine::new();
    let oversized = "é".repeat(MAX_COMMAND_BYTES);
    cl.activate(&oversized);
    assert_eq!(cl.buffer.len(), MAX_COMMAND_BYTES);
    assert!(cl.buffer.is_char_boundary(cl.buffer.len()));
    assert_eq!(cl.buffer, "é".repeat(MAX_COMMAND_BYTES / 2));

    let before = cl.buffer.clone();
    cl.handle_key(KeyCode::Char('x'));
    assert_eq!(cl.buffer, before);
    cl.handle_key(KeyCode::Enter);
    assert_eq!(cl.history.len(), 1);
    assert_eq!(cl.history[0].len(), MAX_COMMAND_BYTES);
}

#[test]
fn command_line_history_empty_not_saved() {
    let mut cl = CommandLine::new();
    cl.activate("");
    cl.handle_key(KeyCode::Enter);
    assert!(cl.history.is_empty());
}

#[test]
fn command_line_up_recalls_previous() {
    let mut cl = CommandLine::new();

    // Enter two commands
    cl.activate("");
    cl.handle_key(KeyCode::Char('r'));
    cl.handle_key(KeyCode::Enter);

    cl.activate("");
    for ch in "open atp://foo".chars() {
        cl.handle_key(KeyCode::Char(ch));
    }
    cl.handle_key(KeyCode::Enter);

    // Start a new prompt, press Up
    cl.activate("");
    cl.handle_key(KeyCode::Up);
    assert_eq!(cl.buffer, "open atp://foo");
    assert_eq!(cl.cursor, 14);

    // Press Up again
    cl.handle_key(KeyCode::Up);
    assert_eq!(cl.buffer, "r");

    // Up at the top stays
    cl.handle_key(KeyCode::Up);
    assert_eq!(cl.buffer, "r");
}

#[test]
fn command_line_down_goes_forward() {
    let mut cl = CommandLine::new();

    cl.activate("");
    cl.handle_key(KeyCode::Char('r'));
    cl.handle_key(KeyCode::Enter);

    cl.activate("");
    cl.handle_key(KeyCode::Char('q'));
    cl.handle_key(KeyCode::Enter);

    // Go up twice, then down
    cl.activate("");
    cl.handle_key(KeyCode::Up); // "q"
    cl.handle_key(KeyCode::Up); // "r"
    cl.handle_key(KeyCode::Down); // "q"
    assert_eq!(cl.buffer, "q");

    // Down again restores live buffer
    cl.handle_key(KeyCode::Down);
    assert_eq!(cl.buffer, ""); // back to empty live buffer
}

#[test]
fn command_line_history_preserves_live_buffer() {
    let mut cl = CommandLine::new();

    cl.activate("");
    cl.handle_key(KeyCode::Char('r'));
    cl.handle_key(KeyCode::Enter);

    // Start typing, then browse history, then come back
    cl.activate("");
    cl.handle_key(KeyCode::Char('o'));
    cl.handle_key(KeyCode::Char('p'));
    assert_eq!(cl.buffer, "op");

    cl.handle_key(KeyCode::Up); // "r"
    assert_eq!(cl.buffer, "r");

    cl.handle_key(KeyCode::Down); // back to "op"
    assert_eq!(cl.buffer, "op");
}

#[test]
fn command_line_history_copy_rejection_preserves_exact_navigation_state() {
    let mut cl = CommandLine::new();
    cl.activate("open atp://history");
    cl.handle_key(KeyCode::Enter);
    cl.activate("live");
    let live_ptr = cl.buffer.as_ptr();

    reject_command_history_copy_after(1);
    cl.handle_key(KeyCode::Up);
    assert_eq!(cl.buffer, "live");
    assert_eq!(cl.buffer.as_ptr(), live_ptr);
    assert_eq!(cl.cursor, 4);
    assert_eq!(cl.history_index, cl.history.len());
    assert!(cl.saved_buffer.is_empty());

    cl.handle_key(KeyCode::Up);
    assert_eq!(cl.buffer, "open atp://history");
    assert_eq!(cl.saved_buffer, "live");
    let history_ptr = cl.buffer.as_ptr();

    reject_command_history_copy_after(0);
    cl.handle_key(KeyCode::Down);
    assert_eq!(cl.buffer, "open atp://history");
    assert_eq!(cl.buffer.as_ptr(), history_ptr);
    assert_eq!(cl.history_index, 0);
    assert_eq!(cl.saved_buffer, "live");

    cl.handle_key(KeyCode::Down);
    assert_eq!(cl.buffer, "live");
    assert_eq!(cl.history_index, cl.history.len());
}

#[test]
fn command_line_down_at_bottom_does_nothing() {
    let mut cl = CommandLine::new();
    cl.activate("");
    cl.handle_key(KeyCode::Down); // no history, does nothing
    assert_eq!(cl.buffer, "");
    assert_eq!(cl.cursor, 0);
}

// ─── Invalidation-driven scoped relayout (Stage 5 drain) ─────
//
// After Phase C's drain generalization, `layout_pass_invalidated`
// handles every `NodeKind` (not just Panel) via `relayout_in_place`
// and cascades to parents on size change. These tests exercise the
// drain end-to-end for Panel state flips, which is the most common
// triggering case.

mod scoped_panel_relayout {
    use super::super::*;
    use crate::compositor::animate::{AdvanceCtx, AdvanceResult, AnimState, Animation};
    use crate::compositor::layout::engine::LAYOUT_CALLS;
    use crate::resource::{MAX_SCENE_CELLS, ResourceCategory};
    use std::cell::Cell as Counter;
    use std::rc::Rc;

    type FocusSnapshot = (NodeId, Option<String>, String, u16, u16, u16, bool);

    struct ProbeAnimation {
        advances: Rc<Counter<u32>>,
        skips: Rc<Counter<u32>>,
        finished: bool,
    }

    impl Animation for ProbeAnimation {
        fn id(&self) -> &str {
            "probe"
        }

        fn advance(&mut self, _ctx: &mut AdvanceCtx) -> AdvanceResult {
            self.advances.set(self.advances.get() + 1);
            self.finished = true;
            AdvanceResult::none()
        }

        fn finished(&self) -> bool {
            self.finished
        }

        fn state(&self) -> AnimState {
            if self.finished {
                AnimState::Finished
            } else {
                AnimState::Running
            }
        }

        fn trigger_stop(&mut self) {
            self.skips.set(self.skips.get() + 1);
            self.finished = true;
        }
    }

    fn parse_aml(src: &str) -> crate::parser::ast::Document {
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        crate::parser::parse(tokens).document.expect("parse failed")
    }

    async fn build_page(aml: &str) -> LoadedPage {
        let doc = parse_aml(aml);
        layout_page(
            doc,
            80,
            24,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
            None,
            None,
            None,
        )
        .await
    }

    /// Phase 2 of the composite-unification plan moves text into
    /// per-node buffers. Tests that check cell content must read the
    /// composited view rather than raw `page.buf`.
    fn composited(page: &LoadedPage) -> crate::compositor::layout::cell::CellBuffer {
        let anim_rt = crate::compositor::animate::AnimationRuntime::new(Vec::new());
        crate::compositor::composite::walk(&page.scene, &anim_rt, page.buf.width, page.buf.height)
    }

    fn rendered_prefix(page: &LoadedPage, len: u16) -> String {
        let composed = composited(page);
        (0..len)
            .map(|x| composed.get(x, 0).map_or(' ', |cell| cell.ch))
            .collect()
    }

    fn focus_snapshot(page: &LoadedPage) -> Vec<FocusSnapshot> {
        page.focusables
            .iter()
            .map(|focusable| {
                (
                    focusable.node_id,
                    focusable.id.clone(),
                    focusable.label.clone(),
                    focusable.col,
                    focusable.row,
                    focusable.width,
                    focusable.is_sticky,
                )
            })
            .collect()
    }

    fn placed_snapshot(page: &LoadedPage) -> Vec<(String, Rect)> {
        page.placed
            .iter()
            .map(|placed| (placed.id.clone(), placed.rect))
            .collect()
    }

    /// Stage 5 (invalidation drain) runs `layout_node` exactly once
    /// per invalidated subtree, not the whole page. The counter proves
    /// the drain path uses `relayout_in_place` rather than calling
    /// `layout_scene` on the full scene.
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn scoped_relayout_issues_exactly_one_layout_call() {
        let mut page = build_page(
            r#"[page mode=screen cols=40 rows=10]
                [panel id="p" state="a"]
                    [state name="a" x=0 y=0 w=40 h=5][text]State A[/text][/state]
                    [state name="b" x=0 y=0 w=40 h=5][text]State B[/text][/state]
                [/panel]
            [/page]"#,
        )
        .await;

        // Flip to state b — patch populates `invalidation.layout`.
        assert!(apply_panel_patch(&mut page.scene, "p", "b"));
        assert!(!page.scene.invalidation.layout.is_empty());

        let before = LAYOUT_CALLS.with(|c| c.get());
        layout_pass_invalidated(
            &mut page.scene,
            &mut page.buf,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );
        let after = LAYOUT_CALLS.with(|c| c.get());

        assert_eq!(
            after - before,
            1,
            "stage 5 must run relayout_in_place exactly once for the single invalidated node",
        );
        assert!(
            page.scene.invalidation.layout.is_empty(),
            "stage 5 must drain invalidation.layout",
        );
    }

    /// After stage 5 drains a screen-mode panel flip, page.buf
    /// contains the new state's content in the panel's region.
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn scoped_relayout_paints_new_state() {
        let mut page = build_page(
            r#"[page mode=screen cols=40 rows=10]
                [panel id="p" state="a"]
                    [state name="a"][text]AAAA[/text][/state]
                    [state name="b"][text]BBBB[/text][/state]
                [/panel]
            [/page]"#,
        )
        .await;

        apply_panel_patch(&mut page.scene, "p", "b");
        layout_pass_invalidated(
            &mut page.scene,
            &mut page.buf,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        let composed = composited(&page);
        let row0: String = (0..4)
            .map(|x| composed.get(x, 0).map(|c| c.ch).unwrap_or(' '))
            .collect();
        assert_eq!(row0, "BBBB", "stage 5 should paint new state; got {row0:?}");
    }

    /// Sequential panel flips through stage 5 leave the buffer on the
    /// final state with no residue from intermediate flips.
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn scoped_relayout_interaction_sequence() {
        let mut page = build_page(
            r#"[page mode=screen cols=40 rows=10]
                [panel id="p" state="a"]
                    [state name="a"][text]AAAA[/text][/state]
                    [state name="b"][text]BBBB[/text][/state]
                    [state name="c"][text]CCCC[/text][/state]
                [/panel]
            [/page]"#,
        )
        .await;
        let cs = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();

        for s in ["b", "c", "a", "c"] {
            apply_panel_patch(&mut page.scene, "p", s);
            layout_pass_invalidated(&mut page.scene, &mut page.buf, cs, wcfg);
        }
        let composed = composited(&page);
        let row0: String = (0..4)
            .map(|x| composed.get(x, 0).map(|c| c.ch).unwrap_or(' '))
            .collect();
        assert_eq!(row0, "CCCC", "ended at state 'c', got {row0:?}");
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn invalidation_drain_pressure_preserves_the_exact_pending_projection() {
        let mut page = build_page(
            r#"[page mode=screen cols=40 rows=10]
                [input id="field" name="field" value="old" /]
            [/page]"#,
        )
        .await;
        let field = page.scene.find_by_aml_id("field").unwrap();
        let buffer_text = |page: &LoadedPage| {
            let buffer = page.scene.buffer_of(field).unwrap();
            (0..buffer.height)
                .flat_map(|row| (0..buffer.width).map(move |col| buffer.get(col, row).unwrap().ch))
                .collect::<String>()
        };
        let old_buffer = buffer_text(&page);
        let old_placement = *page.scene.get(field).unwrap().placement();
        PatchApplier::apply(
            &mut page.scene,
            Patch::SetInputValue {
                node: field,
                value: "pending".into(),
            },
        );
        let pending_invalidation = page.scene.invalidation.clone();
        let baseline = page.governor.total_used();
        let available = crate::resource::MAX_REMOTE_MEMORY
            .saturating_sub(baseline)
            .saturating_sub(1);
        let blocker = page
            .governor
            .reserve(ResourceCategory::RemoteCollections, available)
            .unwrap();

        assert!(!drain_invalidated_layout_transactionally(
            &mut page,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        ));
        assert_eq!(buffer_text(&page), old_buffer);
        assert_eq!(*page.scene.get(field).unwrap().placement(), old_placement);
        assert_eq!(page.scene.invalidation.layout, pending_invalidation.layout);
        assert_eq!(
            page.scene.invalidation.composite.as_slice(),
            pending_invalidation.composite.as_slice()
        );
        assert!(!page.scene.resource_limit_exceeded());

        drop(blocker);
        assert!(drain_invalidated_layout_transactionally(
            &mut page,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        ));
        assert!(page.scene.invalidation.layout.is_empty());
        assert_ne!(buffer_text(&page), old_buffer);

        let composite = page.scene.invalidation.composite.clone();
        let present = page.scene.invalidation.present.clone();
        let remaining =
            crate::resource::MAX_REMOTE_MEMORY.saturating_sub(page.governor.total_used());
        let exhausted = page
            .governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();
        let exhausted_used = page.governor.total_used();
        assert!(drain_invalidated_layout_transactionally(
            &mut page,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        ));
        assert_eq!(page.governor.total_used(), exhausted_used);
        assert_eq!(
            page.scene.invalidation.composite.as_slice(),
            composite.as_slice()
        );
        assert_eq!(
            page.scene.invalidation.present.as_slice(),
            present.as_slice()
        );
        drop(exhausted);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn transactional_panel_buffer_rejection_restores_exact_page() {
        let mut page = build_page(
            r#"[page mode=screen cols=40 rows=10]
                [panel id="p" state="a"]
                    [state name="a"][text]AAAA[/text][/state]
                    [state name="b"][text]BBBBBBBB[/text][/state]
                [/panel]
            [/page]"#,
        )
        .await;
        let mut state = ViewportState::new(40, 10, page.buf.height);
        state.scroll_offset = 1;
        let old_active = panel_active_node(&page.scene, "p").unwrap();
        let old_render = rendered_prefix(&page, 8);
        let old_placement = *page.scene.get(old_active).unwrap().placement();
        let old_focusables = focus_snapshot(&page);
        let old_placed = placed_snapshot(&page);
        let old_invalidation = page.scene.invalidation.clone();
        let old_content_height = state.content_height;
        let baseline = page.governor.total_used();

        assert!(page.scene.begin_relayout_transaction());
        let used = page.governor.used(ResourceCategory::SceneCells);
        let blocker = page
            .governor
            .reserve_with_cost(ResourceCategory::SceneCells, MAX_SCENE_CELLS - used, 0)
            .unwrap();
        assert!(apply_panel_patch(&mut page.scene, "p", "b"));
        let committed = relayout_panels_for(
            &mut page,
            &mut state,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
            Some("p"),
            None,
            None,
        )
        .await;
        assert!(!committed);
        let panel = page.scene.find_by_aml_id("p").unwrap();
        PatchApplier::apply(
            &mut page.scene,
            Patch::SetPanelActive {
                panel,
                active: old_active,
            },
        );
        page.scene.rollback_relayout_transaction();
        drop(blocker);

        assert_eq!(panel_active_node(&page.scene, "p"), Some(old_active));
        assert_eq!(rendered_prefix(&page, 8), old_render);
        assert_eq!(
            *page.scene.get(old_active).unwrap().placement(),
            old_placement
        );
        assert_eq!(focus_snapshot(&page), old_focusables);
        assert_eq!(placed_snapshot(&page), old_placed);
        assert_eq!(page.scene.invalidation.layout, old_invalidation.layout);
        assert_eq!(
            page.scene.invalidation.composite.as_slice(),
            old_invalidation.composite.as_slice()
        );
        assert_eq!(state.content_height, old_content_height);
        assert_eq!(state.scroll_offset, 1);
        assert!(!page.scene.resource_limit_exceeded());
        assert!(!page.buf.allocation_failed());
        assert_eq!(page.governor.total_used(), baseline);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn transactional_details_rejection_restores_collapsed_layout() {
        let mut page = build_page(
            r#"[page mode=document]
                [details summary="More"]
                    [text]expanded body that requires another owned buffer[/text]
                    [input name="inside" /]
                [/details]
                [text]after[/text]
            [/page]"#,
        )
        .await;
        let mut state = ViewportState::new(80, 24, page.buf.height);
        let details = page.scene.find_details_by_index(0).unwrap();
        let old_rect = page.scene.get(details).unwrap().placement().rect;
        let old_render = rendered_prefix(&page, 12);
        let old_focusables = focus_snapshot(&page);
        let old_height = state.content_height;
        let baseline = page.governor.total_used();

        assert!(page.scene.begin_relayout_transaction());
        let used = page.governor.used(ResourceCategory::SceneCells);
        let blocker = page
            .governor
            .reserve_with_cost(ResourceCategory::SceneCells, MAX_SCENE_CELLS - used, 0)
            .unwrap();
        PatchApplier::apply(&mut page.scene, Patch::ToggleDetails { node: details });
        let committed = relayout_panels_for(
            &mut page,
            &mut state,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;
        assert!(!committed);
        PatchApplier::apply(&mut page.scene, Patch::ToggleDetails { node: details });
        page.scene.rollback_relayout_transaction();
        drop(blocker);

        let NodeKind::Flow(details_data) = page.scene.get(details).unwrap().kind() else {
            panic!("details node changed kind");
        };
        assert!(!details_data.details_open);
        assert_eq!(page.scene.get(details).unwrap().placement().rect, old_rect);
        assert_eq!(rendered_prefix(&page, 12), old_render);
        assert_eq!(focus_snapshot(&page), old_focusables);
        assert_eq!(state.content_height, old_height);
        assert!(!page.scene.resource_limit_exceeded());
        assert_eq!(page.governor.total_used(), baseline);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn transactional_details_success_commits_all_derived_state() {
        let mut page = build_page(
            r#"[page mode=document]
                [details summary="More"]
                    [text]expanded body[/text]
                    [input name="inside" /]
                [/details]
                [text]after[/text]
            [/page]"#,
        )
        .await;
        let mut state = ViewportState::new(80, 24, page.buf.height);
        let details = page.scene.find_details_by_index(0).unwrap();
        let collapsed_height = state.content_height;

        assert!(page.scene.begin_relayout_transaction());
        PatchApplier::apply(&mut page.scene, Patch::ToggleDetails { node: details });
        assert!(
            relayout_panels_for(
                &mut page,
                &mut state,
                crate::color::ColorSupport::Truecolor,
                crate::compositor::layout::text::WidthConfig::default(),
                None,
                None,
                None,
            )
            .await
        );

        let NodeKind::Flow(details_data) = page.scene.get(details).unwrap().kind() else {
            panic!("details node changed kind");
        };
        assert!(details_data.details_open);
        assert!(rendered_prefix(&page, 12).starts_with("▼ More"));
        assert!(state.content_height >= collapsed_height);
        assert!(page.scene.invalidation.layout.is_empty());
        assert!(!page.scene.resource_limit_exceeded());
        // A committed transaction has no rollback state left to apply.
        let committed_render = rendered_prefix(&page, 12);
        page.scene.rollback_relayout_transaction();
        assert_eq!(rendered_prefix(&page, 12), committed_render);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn relayout_pressure_evicts_resource_then_retries_exact_action() {
        let aml = r#"[page mode=screen cols=40 rows=10]
            [panel id="p" state="a"]
                [state name="a"][text]AAAA[/text][/state]
                [state name="b"][text]BBBB[/text][/state]
            [/panel]
        [/page]"#;
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = crate::viewer::PageScope {
            origin: origin.clone(),
            generation: 1,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml(aml),
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        client
            .resource_cache
            .insert(origin, "/old".into(), vec![7; 1024 * 1024])
            .unwrap();

        let governor = page.governor.clone();
        let available = crate::resource::MAX_REMOTE_MEMORY
            .saturating_sub(governor.total_used())
            .saturating_sub(1);
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, available)
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let mut model = LifecycleModel::new(40, 10);
        model.scope = Some(scope.clone());
        model.current_uri = Some(uri);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        let mut lifecycle = ReducerPort::new(model);

        crate::compositor::terminal::dispatch_runtime_events(
            &mut runtime,
            &mut lifecycle,
            [LifecycleEvent::PresentationActionRequested {
                scope: Some(scope),
                action: PresentationAction::SetPanel {
                    panel_id: "p".into(),
                    state: "b".into(),
                },
            }],
        )
        .await
        .unwrap();

        assert_eq!(
            scene_panel_current_state(&runtime.page.scene, "p"),
            Some("b")
        );
        assert!(rendered_prefix(&runtime.page, 4).starts_with("BBBB"));
        assert_eq!(
            runtime
                .client
                .as_ref()
                .unwrap()
                .governor
                .used(ResourceCategory::ResourceCache),
            0
        );
        assert_eq!(lifecycle.phase, NavigationPhase::Ready);
        assert_eq!(lifecycle.connection, ConnectionStatus::Connected);
        assert!(runtime.render_authorized);
        drop(blocker);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn authored_action_pressure_resumes_exact_item_without_replaying_event() {
        let aml = r#"[page mode=screen cols=40 rows=10]
            [on event="page-load" do="toggle" target="p" /]
            [panel id="p" state="a"]
                [state name="a"][text]AAAA[/text][/state]
                [state name="b"][text]BBBB[/text][/state]
            [/panel]
        [/page]"#;
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = crate::viewer::PageScope {
            origin: origin.clone(),
            generation: 1,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml(aml),
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        client
            .resource_cache
            .insert(origin, "/old".into(), vec![7; 1024 * 1024])
            .unwrap();

        let governor = page.governor.clone();
        let available = crate::resource::MAX_REMOTE_MEMORY
            .saturating_sub(governor.total_used())
            .saturating_sub(1);
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, available)
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let mut model = LifecycleModel::new(40, 10);
        model.scope = Some(scope.clone());
        model.current_uri = Some(uri);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        let mut lifecycle = ReducerPort::new(model);

        crate::compositor::terminal::dispatch_runtime_events(
            &mut runtime,
            &mut lifecycle,
            [LifecycleEvent::PresentationActionRequested {
                scope: Some(scope),
                action: PresentationAction::PageLoad,
            }],
        )
        .await
        .unwrap();

        assert_eq!(
            scene_panel_current_state(&runtime.page.scene, "p"),
            Some("b"),
            "retrying PageLoad would enqueue Toggle twice and return the panel to a"
        );
        assert!(rendered_prefix(&runtime.page, 4).starts_with("BBBB"));
        assert_eq!(runtime.event_dispatcher.pending_len(), 0);
        assert_eq!(
            runtime
                .client
                .as_ref()
                .unwrap()
                .governor
                .used(ResourceCategory::ResourceCache),
            0
        );
        assert_eq!(lifecycle.phase, NavigationPhase::Ready);
        assert_eq!(lifecycle.connection, ConnectionStatus::Connected);
        assert!(runtime.render_authorized);
        drop(blocker);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn invalidation_drain_pressure_evicts_resource_then_retries_exact_action() {
        let aml = r#"[page mode=screen cols=40 rows=10]
            [input id="field" name="field" value="old" /]
        [/page]"#;
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = crate::viewer::PageScope {
            origin: origin.clone(),
            generation: 1,
        };
        client.activate_page_scope(scope.clone()).await;
        let mut page = layout_page(
            parse_aml(aml),
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        let field = page.scene.find_by_aml_id("field").unwrap();
        let old_buffer = page.scene.buffer_of(field).unwrap().get(1, 0).unwrap().ch;
        PatchApplier::apply(
            &mut page.scene,
            Patch::SetInputValue {
                node: field,
                value: "pending".into(),
            },
        );
        client
            .resource_cache
            .insert(origin, "/old".into(), vec![7; 1024 * 1024])
            .unwrap();
        let governor = page.governor.clone();
        let available = crate::resource::MAX_REMOTE_MEMORY
            .saturating_sub(governor.total_used())
            .saturating_sub(1);
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, available)
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let mut model = LifecycleModel::new(40, 10);
        model.scope = Some(scope.clone());
        model.current_uri = Some(uri);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        let mut lifecycle = ReducerPort::new(model);

        crate::compositor::terminal::dispatch_runtime_events(
            &mut runtime,
            &mut lifecycle,
            [LifecycleEvent::PresentationActionRequested {
                scope: Some(scope),
                action: PresentationAction::DrainLayout,
            }],
        )
        .await
        .unwrap();

        assert!(runtime.page.scene.invalidation.layout.is_empty());
        assert_ne!(
            runtime
                .page
                .scene
                .buffer_of(field)
                .unwrap()
                .get(1, 0)
                .unwrap()
                .ch,
            old_buffer
        );
        assert_eq!(
            runtime
                .client
                .as_ref()
                .unwrap()
                .governor
                .used(ResourceCategory::ResourceCache),
            0
        );
        assert_eq!(lifecycle.phase, NavigationPhase::Ready);
        assert_eq!(lifecycle.connection, ConnectionStatus::Connected);
        assert!(runtime.render_authorized);
        drop(blocker);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn animation_tick_pressure_preserves_the_exact_attempt_until_retry() {
        let mut page = build_page("[page mode=document][text]active[/text][/page]").await;
        let advances = Rc::new(Counter::new(0));
        let skips = Rc::new(Counter::new(0));
        page.anim_rt.animations = vec![Box::new(ProbeAnimation {
            advances: advances.clone(),
            skips,
            finished: false,
        })];
        let governor = page.governor.clone();
        let remaining = crate::resource::MAX_REMOTE_MEMORY.saturating_sub(governor.total_used());
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            None,
            Vec::new(),
            80,
            24,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let scope = crate::viewer::PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 1,
        };
        let mut model = LifecycleModel::new(80, 24);
        model.scope = Some(scope);
        let owner = match model.reduce(LifecycleEvent::WasmRequested {
            path: "/tick.wasm".into(),
        })[0]
        {
            LifecycleEffect::LoadWasm { ref owner, .. } => owner.clone(),
            _ => panic!("expected reducer-owned WASM request"),
        };
        let old_needs_redraw = runtime.needs_redraw;

        let failed = runtime
            .execute(
                LifecycleEffect::TickWasm {
                    owner: Some(owner.clone()),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            failed.as_slice(),
            [LifecycleEvent::PresentationFailed {
                retry: Some(PressureRetry::TickWasm { owner: Some(retry) }),
                ..
            }] if retry == &owner
        ));
        assert_eq!(advances.get(), 0);
        assert!(runtime.pending_tick.is_none());
        let (attempt_key, first_attempt) = runtime.pending_tick_attempt.unwrap();
        assert_eq!(attempt_key, Some(PreparedWorkKey::from(&owner)));
        assert!(runtime.prepared_wasm.get(&owner).is_none());
        assert_eq!(runtime.needs_redraw, old_needs_redraw);
        assert!(!runtime.page.scene.resource_limit_exceeded());

        let failed_again = runtime
            .execute(
                LifecycleEffect::TickWasm {
                    owner: Some(owner.clone()),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            failed_again.as_slice(),
            [LifecycleEvent::PresentationFailed {
                retry: Some(PressureRetry::TickWasm { owner: Some(retry) }),
                ..
            }] if retry == &owner
        ));
        assert_eq!(
            runtime.pending_tick_attempt,
            Some((attempt_key, first_attempt))
        );

        drop(blocker);
        assert_eq!(
            runtime
                .execute(
                    LifecycleEffect::TickWasm {
                        owner: Some(owner.clone()),
                    },
                    &model,
                )
                .await
                .unwrap(),
            vec![LifecycleEvent::WasmPrepared {
                owner: owner.clone()
            }]
        );
        assert_eq!(advances.get(), 1);
        assert!(runtime.pending_tick_attempt.is_none());
        assert!(runtime.prepared_wasm.get(&owner).is_some());
        let tick = runtime.pending_tick.take().unwrap();
        assert_eq!(tick.newly_finished, ["probe"]);
        assert!(!tick.allocation_failed);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn animation_skip_pressure_does_not_stop_before_exact_retry() {
        let mut page = build_page("[page mode=document][text]active[/text][/page]").await;
        let advances = Rc::new(Counter::new(0));
        let skips = Rc::new(Counter::new(0));
        page.anim_rt.animations = vec![Box::new(ProbeAnimation {
            advances,
            skips: skips.clone(),
            finished: false,
        })];
        let governor = page.governor.clone();
        let remaining = crate::resource::MAX_REMOTE_MEMORY.saturating_sub(governor.total_used());
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            None,
            Vec::new(),
            80,
            24,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let scope = crate::viewer::PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 1,
        };
        let mut model = LifecycleModel::new(80, 24);
        model.scope = Some(scope.clone());
        let old_needs_redraw = runtime.needs_redraw;

        let failed = runtime
            .execute(
                LifecycleEffect::ApplyPresentationAction {
                    scope: Some(scope.clone()),
                    action: PresentationAction::SkipAnimations,
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            failed.as_slice(),
            [LifecycleEvent::PresentationFailed {
                retry: Some(PressureRetry::Presentation {
                    scope: Some(retry_scope),
                    action: PresentationAction::SkipAnimations,
                }),
                ..
            }] if retry_scope == &scope
        ));
        assert_eq!(skips.get(), 0);
        assert!(runtime.pending_tick.is_none());
        assert_eq!(runtime.needs_redraw, old_needs_redraw);
        assert!(!runtime.page.scene.resource_limit_exceeded());

        drop(blocker);
        assert!(
            runtime
                .execute(
                    LifecycleEffect::ApplyPresentationAction {
                        scope: Some(scope),
                        action: PresentationAction::SkipAnimations,
                    },
                    &model,
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(skips.get(), 1);
        let tick = runtime.pending_tick.take().unwrap();
        assert_eq!(tick.newly_finished, ["probe"]);
        assert!(!tick.allocation_failed);
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn page_transition_start_pressure_preserves_exact_capture_until_retry() {
        let page = build_page("[page mode=document][text]active[/text][/page]").await;
        let governor = page.governor.clone();
        let mut runtime = TerminalRuntime::new(
            page,
            None,
            Vec::new(),
            80,
            24,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let scope = crate::viewer::PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 1,
        };
        let mut model = LifecycleModel::new(80, 24);
        model.scope = Some(scope.clone());

        assert!(
            runtime
                .execute(
                    LifecycleEffect::ApplyPresentationAction {
                        scope: Some(scope.clone()),
                        action: PresentationAction::CapturePageTransition,
                    },
                    &model,
                )
                .await
                .unwrap()
                .is_empty()
        );
        // The capture must fail closed when its sub-buffer is refused, and
        // must not leave a half-captured transition behind. Budget pressure
        // alone cannot reach this: the snapshot is the first thing the capture
        // allocates, so the governor refuses it for a different reason.
        {
            use crate::compositor::terminal::runner::{RunnerAllocationSite, RunnerRejectionGuard};
            let previous = runtime.pending_page_transition.take();
            let _rejection = RunnerRejectionGuard::at(RunnerAllocationSite::SubBuffer);
            let _ = runtime
                .execute(
                    LifecycleEffect::ApplyPresentationAction {
                        scope: Some(scope.clone()),
                        action: PresentationAction::CapturePageTransition,
                    },
                    &model,
                )
                .await;
            assert!(
                runtime.pending_page_transition.is_none(),
                "a refused snapshot must not install a partial transition"
            );
            runtime.pending_page_transition = previous;
        }

        let captured = crate::compositor::present::render_to_string(
            &runtime
                .pending_page_transition
                .as_ref()
                .unwrap()
                .old_snapshot,
        );
        let remaining = crate::resource::MAX_REMOTE_MEMORY.saturating_sub(governor.total_used());
        let blocker = governor
            .reserve(ResourceCategory::CompositorCells, remaining)
            .unwrap();
        let old_redraw = runtime.needs_redraw;
        let action = PresentationAction::StartPageTransition {
            kind: ast::TransitionKind::Fade,
            duration_ms: 231,
        };

        let failed = runtime
            .execute(
                LifecycleEffect::ApplyPresentationAction {
                    scope: Some(scope.clone()),
                    action: action.try_clone().unwrap(),
                },
                &model,
            )
            .await
            .unwrap();
        assert!(matches!(
            failed.as_slice(),
            [LifecycleEvent::PresentationFailed {
                retry: Some(PressureRetry::Presentation {
                    scope: Some(retry_scope),
                    action: retry_action,
                }),
                ..
            }] if retry_scope == &scope && retry_action == &action
        ));
        assert_eq!(
            crate::compositor::present::render_to_string(
                &runtime
                    .pending_page_transition
                    .as_ref()
                    .unwrap()
                    .old_snapshot,
            ),
            captured,
        );
        assert!(!runtime.page.anim_rt.has_page_transition());
        assert!(runtime.page.scene.page_transition_overlay().is_none());
        assert_eq!(runtime.needs_redraw, old_redraw);

        drop(blocker);
        assert!(
            runtime
                .execute(
                    LifecycleEffect::ApplyPresentationAction {
                        scope: Some(scope),
                        action,
                    },
                    &model,
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert!(runtime.pending_page_transition.is_none());
        assert!(runtime.page.anim_rt.has_page_transition());
        assert!(runtime.page.scene.page_transition_overlay().is_some());
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn resize_pressure_evicts_resource_then_commits_exact_projection() {
        let aml = r#"[page mode=document title="Resize"]
            [text]content that remains active throughout resize recovery[/text]
            [link id="next" href="/next"][text]next[/text][/link]
        [/page]"#;
        let mut client = AtpClient::new(crate::client::TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/current").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let scope = crate::viewer::PageScope {
            origin: origin.clone(),
            generation: 1,
        };
        client.activate_page_scope(scope.clone()).await;
        let page = layout_page(
            parse_aml(aml),
            80,
            24,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(&mut client),
            Some(uri.try_clone().unwrap()),
            None,
        )
        .await;
        client
            .resource_cache
            .insert(origin, "/old".into(), vec![7; 1024 * 1024])
            .unwrap();
        let governor = page.governor.clone();
        let available = crate::resource::MAX_REMOTE_MEMORY
            .saturating_sub(governor.total_used())
            .saturating_sub(1);
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, available)
            .unwrap();
        let mut runtime = TerminalRuntime::new(
            page,
            Some(client),
            Vec::new(),
            80,
            24,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let mut model = LifecycleModel::new(80, 24);
        model.scope = Some(scope);
        model.current_uri = Some(uri);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        let mut lifecycle = ReducerPort::new(model);

        crate::compositor::terminal::dispatch_runtime_events(
            &mut runtime,
            &mut lifecycle,
            [LifecycleEvent::Resize {
                width: 43,
                height: 18,
            }],
        )
        .await
        .unwrap();

        assert_eq!(runtime.state.term_w, 43);
        assert_eq!(runtime.state.term_h, 18);
        assert_eq!(runtime.page.buf.width, 43);
        assert_eq!(lifecycle.viewport, (43, 18));
        assert_eq!(lifecycle.phase, NavigationPhase::Ready);
        assert_eq!(lifecycle.connection, ConnectionStatus::Connected);
        assert_eq!(governor.used(ResourceCategory::ResourceCache), 0);
        assert!(runtime.render_authorized);
        drop(blocker);
    }
}

// ─── Transition integration ───────────────────────────

mod transition_integration {
    use super::super::*;
    use crate::compositor::animate::{Animation, TransitionAdapter};
    use crate::compositor::layout::Rect;
    use crate::compositor::layout::cell::{CellBuffer, CellStyle};
    use crate::parser::ast::TransitionKind;

    fn fill(w: u16, h: u16, ch: char) -> CellBuffer {
        let mut buf = CellBuffer::new(w, h);
        for y in 0..h {
            for x in 0..w {
                buf.put_char(x, y, ch, &CellStyle::default());
            }
        }
        buf
    }

    /// Shape-mismatched panel transition: state A is 4×2 at top-left,
    /// state B is 2×2 at bottom-right. A dissolve transition runs;
    /// the adapter composes the union rect with cells exclusive to each
    /// state coming from that state's buffer.
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn shape_mismatched_transition_composes_cleanly() {
        let doc_src = r#"[page mode=document]
            [panel id="p" state="a"]
                [state name="a"][text]AAAA[/text][/state]
                [state name="b"][text]BB[/text][/state]
            [/panel]
        [/page]"#;
        let page = layout_page(
            {
                let mut s = crate::scanner::Scanner::new(doc_src.as_bytes()).unwrap();
                let t = s.scan_all().unwrap();
                crate::parser::parse(t).document.unwrap()
            },
            80,
            24,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;

        // Manually build a TransitionAdapter with shape-mismatched
        // rects to prove end-to-end that the scene-native transition
        // flow handles them.
        let panel_node = page.scene.find_by_aml_id("p").unwrap();
        let target = Rect::new(0, 0, 10, 5);
        let old_rect = Rect::new(0, 0, 4, 2);
        let new_rect = Rect::new(6, 3, 2, 2);
        let t = TransitionAdapter::new(
            "shape-mismatch".into(),
            panel_node,
            target,
            fill(4, 2, 'A'),
            old_rect,
            fill(2, 2, 'B'),
            new_rect,
            TransitionKind::Dissolve,
            100,
        );

        // A-exclusive cell → sources from A.
        assert_eq!(t.blend_cell(1, 1).map(|c| c.ch), Some('A'));
        // B-exclusive cell → sources from B.
        assert_eq!(t.blend_cell(6, 3).map(|c| c.ch), Some('B'));
        // Neither-rect cell → reveals base (None = transparent).
        assert_eq!(t.blend_cell(9, 4), None);
    }

    /// Regression: `hydrate_scene_buffers` must hydrate the node's
    /// placement from the layout `PlacedElement.rect`. Previously only
    /// the buffer was allocated, leaving `node.placement()` at the
    /// default `(0,0,0,0)` — which caused `render_layer` to blit the
    /// animation at screen origin instead of the box it lives in.
    #[test]
    fn animation_node_placement_hydrated_from_layout() {
        use crate::color::ColorSupport;
        use crate::compositor::layout::engine::layout_scene;
        use crate::compositor::layout::text::WidthConfig;

        let src = r#"[page mode=screen]
            [box y=3 w=40 h=5 border=single align=center]
                [animate id="tag" fps=12 autoplay=false]
                    [frame][pre]hello[/pre][/frame]
                [/animate]
            [/box]
        [/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let layout = layout_scene(
            &mut scene,
            80,
            24,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );

        super::hydrate_scene_buffers(&mut scene, &layout.placed);
        let node_id = scene.find_by_aml_id("tag").expect("animation node exists");
        let placement_rect = scene.get(node_id).unwrap().placement().rect;

        let expected = layout
            .placed
            .iter()
            .find(|p| p.id == "tag")
            .expect("placed element");
        assert_eq!(
            placement_rect, expected.rect,
            "node.placement().rect must match PlacedElement.rect"
        );
        // And sanity — the animation sits inside the box, not at origin.
        assert!(
            placement_rect.y >= 3,
            "animation y must be inside the y=3 box, got {}",
            placement_rect.y
        );
        assert!(
            placement_rect.x > 0,
            "animation x must be non-zero (box is centered), got {}",
            placement_rect.x
        );
    }

    /// Cut transitions don't schedule anything — the panel simply
    /// flips to the new state without an adapter. `Cut` produces
    /// zero animation scheduling by design.
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn cut_transition_produces_no_adapter() {
        let target = Rect::new(0, 0, 5, 2);
        let t = TransitionAdapter::new(
            "cut".into(),
            crate::compositor::scene::NodeId::default(),
            target,
            fill(5, 2, 'A'),
            target,
            fill(5, 2, 'B'),
            target,
            TransitionKind::Cut,
            500, // duration irrelevant for Cut
        );
        assert!(t.finished(), "Cut is born finished so the runtime drops it");
    }
}

// ─── Debug harness: dustnet header dissolve ─────────────────
//
// Runs the actual sites/dustnet/index.aml through the page-load → set
// cascade and prints what happens at each tick. Invoke with:
//
//   cargo test -p dustnet --lib debug_dustnet_header_dissolve -- --nocapture
//
// Always passes; the value is in the printed trace.

#[cfg(test)]
mod debug_dissolve {
    use super::*;
    use crate::color::ColorSupport;
    use crate::compositor::animate::Animation;
    use crate::compositor::layout::text::WidthConfig;
    use crate::compositor::scene::NodeKind;
    use std::time::{Duration, Instant};

    fn panel_projection_fixture() -> crate::parser::ast::Document {
        parse_aml(
            "[page mode=screen]\n\
             [panel id=dir-box state=hidden]\n\
               [state name=hidden][box x=28 y=22 w=22 h=6 border=none /][/state]\n\
               [state name=visible][box x=28 y=22 w=22 h=6 border=rounded /][/state]\n\
             [/panel]\n\
             [panel id=stats-box state=hidden]\n\
               [state name=hidden][box x=56 y=30 w=25 h=7 border=none /][/state]\n\
               [state name=visible][box x=56 y=30 w=25 h=7 border=rounded /][/state]\n\
             [/panel]\n\
             [/page]",
        )
        .expect("parse hermetic panel projection fixture")
    }

    fn wasm_projection_fixture() -> crate::parser::ast::Document {
        parse_aml(
            "[page mode=document]\n\
             [animate id=title-lifecycle src=\"/effects/typewriter.wasm\" fps=30 loop=false][pre]DUSTNET[/pre][/animate]\n\
             [animate id=tagline src=\"/effects/typewriter.wasm\" fps=30 loop=false][pre]ANSI[/pre][/animate]\n\
             [animate id=welcome-directory-line src=\"/effects/typewriter.wasm\" fps=30 loop=false][pre]line[/pre][/animate]\n\
             [/page]",
        )
        .expect("parse hermetic WASM projection fixture")
    }

    const PANEL_ID: &str = "title-box";

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn dustnet_chained_panels_keep_server_box_visible() {
        let mut page = layout_page(
            panel_projection_fixture(),
            100,
            40,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;
        let mut state = ViewportState::new(100, 40, page.buf.height);

        for panel_id in ["dir-box", "stats-box"] {
            page.scene.invalidation.clear();
            let old_panel = capture_panel_transition_source(&page, panel_id);
            assert!(apply_panel_patch(&mut page.scene, panel_id, "visible"));
            relayout_panels_for(
                &mut page,
                &mut state,
                ColorSupport::Truecolor,
                WidthConfig::default(),
                Some(panel_id),
                old_panel,
                None,
            )
            .await;
        }

        let stats = page.scene.find_by_aml_id("stats-box").unwrap();
        let rect = page.scene.get(stats).unwrap().placement().rect;
        assert_eq!(rect, Rect::new(56, 30, 25, 7));
        assert!(
            page.scene
                .invalidation
                .present
                .as_slice()
                .iter()
                .any(|dirty| dirty.contains_point(rect.x, rect.y)),
            "the newly revealed Server box must be included in terminal presentation",
        );

        let frame =
            crate::compositor::composite::walk_static(&page.scene, page.buf.width, page.buf.height);
        assert_eq!(frame.get(rect.x, rect.y).unwrap().ch, '╭');
        assert_eq!(frame.get(rect.x, rect.y + rect.h - 1).unwrap().ch, '╰');
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    async fn resize_preserves_finished_dustnet_wasm_layers() {
        let site_dir = crate::repository_root().join("tests/fixtures/site");
        let mut page = layout_page(
            wasm_projection_fixture(),
            100,
            40,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            Some(&site_dir),
        )
        .await;

        let foreground_ids = ["title-lifecycle", "tagline", "welcome-directory-line"];
        for id in foreground_ids {
            assert!(page.anim_rt.trigger_start(id), "missing animation {id}");
        }
        let skipped = page.anim_rt.skip_all(&mut page.scene);
        for id in foreground_ids {
            assert!(
                skipped.newly_finished.iter().any(|finished| finished == id),
                "{id} was not finished by the test setup"
            );
        }

        for id in foreground_ids {
            let node = page.scene.find_by_aml_id(id).unwrap();
            let buffer = page.scene.buffer_of(node).unwrap();
            assert!(
                (0..buffer.height)
                    .any(|y| { (0..buffer.width).any(|x| buffer.get(x, y).unwrap().ch != ' ') }),
                "{id} should have a rendered terminal frame before resize"
            );
        }

        let mut focusables = Vec::new();
        let layout = full_layout_pass(
            &mut page.scene,
            120,
            40,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            &mut focusables,
            None,
        );
        hydrate_scene_buffers(&mut page.scene, &layout.placed);
        let prepared = page.anim_rt.prepare_resize(&page.scene).unwrap();
        page.anim_rt.commit_resize(&mut page.scene, prepared);

        for id in foreground_ids {
            let animation = page
                .anim_rt
                .animations
                .iter()
                .find(|animation| animation.id() == id)
                .unwrap();
            assert!(animation.finished(), "{id} lifecycle reset during resize");

            let node = page.scene.find_by_aml_id(id).unwrap();
            let buffer = page.scene.buffer_of(node).unwrap();
            assert!(
                (0..buffer.height)
                    .any(|y| { (0..buffer.width).any(|x| buffer.get(x, y).unwrap().ch != ' ') }),
                "{id} final pixels were erased during resize"
            );
        }
    }

    fn panel_buffer_signature(scene: &crate::compositor::scene::Scene, panel_id: &str) -> String {
        let Some(node_id) = scene.find_by_aml_id(panel_id) else {
            return "(panel not in scene)".into();
        };
        let Some(node) = scene.get(node_id) else {
            return "(node missing)".into();
        };
        let Some(buf) = node.buffer() else {
            return "(no buffer)".into();
        };
        let total = (buf.width as usize) * (buf.height as usize);
        let mut sample = String::new();
        let mid_y = buf.height / 2;
        for x in 0..buf.width.min(40) {
            let cell = buf
                .get(x, mid_y)
                .cloned()
                .unwrap_or(crate::compositor::layout::cell::Cell::empty());
            sample.push(if cell.ch == '\0' || cell.ch == ' ' {
                '.'
            } else {
                cell.ch
            });
        }
        let mut full_non_empty = 0usize;
        for y in 0..buf.height {
            for x in 0..buf.width {
                let cell = buf
                    .get(x, y)
                    .cloned()
                    .unwrap_or(crate::compositor::layout::cell::Cell::empty());
                if cell.ch != '\0' && cell.ch != ' ' {
                    full_non_empty += 1;
                }
            }
        }
        format!(
            "buf {}x{} ({}/{} non-empty, mid-row sample [{}]: {})",
            buf.width, buf.height, full_non_empty, total, mid_y, sample
        )
    }

    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    #[tokio::test]
    #[ignore = "manual trace harness; run explicitly with --ignored --nocapture"]
    async fn debug_dustnet_header_dissolve() {
        println!("\n=== DUSTNET HEADER DISSOLVE DEBUG ===\n");

        let doc = panel_projection_fixture();
        let term_w = 100u16;
        let term_h = 40u16;

        let mut page = layout_page(
            doc,
            term_w,
            term_h,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
        )
        .await;
        let mut state = ViewportState::new(term_w, term_h, page.buf.height);

        // 1. Inspect what was parsed.
        println!("--- step 1: scene + bindings ---");
        let Some(panel_node) = page.scene.find_by_aml_id(PANEL_ID) else {
            println!("  fixture no longer contains panel {PANEL_ID:?}; nothing to trace");
            return;
        };
        let panel_kind = page.scene.get(panel_node).map(|n| n.kind().clone());
        match &panel_kind {
            Some(NodeKind::Panel {
                states,
                active,
                initial_state,
            }) => {
                println!(
                    "  panel header-box: {} states, initial={}, active={:?}",
                    states.len(),
                    initial_state,
                    active
                );
                for &state_id in states {
                    if let Some(state_node) = page.scene.get(state_id) {
                        let name = state_node.aml_id().unwrap_or("?");
                        if let NodeKind::Flow(fd) = state_node.kind() {
                            println!(
                                "    state {:?}: transition={:?} duration_ms={} children={}",
                                name,
                                fd.state_transition,
                                fd.state_transition_duration_ms,
                                state_node.children().len()
                            );
                        }
                    }
                }
            }
            other => println!("  expected Panel, got {:?}", other),
        }
        for b in &page.scene.event_bindings {
            println!(
                "  binding: event={:?} source={:?} action={:?} target={:?} to={:?} delay={}ms",
                b.event, b.source, b.action, b.target, b.to, b.delay_ms
            );
        }

        // 2. Initial layout — where does the panel sit?
        println!("\n--- step 2: initial placement (state=hidden) ---");
        for p in page.panels() {
            println!("  placed panel {:?}: rect={:?}", p.id, p.rect);
        }
        println!(
            "  initial {}",
            panel_buffer_signature(&page.scene, PANEL_ID)
        );

        // 3. Manually flip to visible, simulating what execute_on_actions does.
        println!("\n--- step 3: flip header-box → visible ---");
        let old_panel = capture_panel_transition_source(&page, PANEL_ID);
        let flipped = apply_panel_patch(&mut page.scene, PANEL_ID, "visible");
        println!("  apply_panel_patch returned: {}", flipped);
        println!(
            "  scene.invalidation.composite empty? {}",
            page.scene.invalidation.composite.is_empty()
        );
        println!(
            "  scene.invalidation.layout empty? {}",
            page.scene.invalidation.layout.is_empty()
        );

        relayout_panels_for(
            &mut page,
            &mut state,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            Some(PANEL_ID),
            old_panel,
            None,
        )
        .await;

        println!("  after relayout:");
        for p in page.panels() {
            println!("    placed panel {:?}: rect={:?}", p.id, p.rect);
        }
        println!(
            "    transition_animations.len() = {}",
            page.anim_rt.transition_animations.len()
        );
        for trans in &page.anim_rt.transition_animations {
            println!(
                "    transition: id={:?} finished={}",
                trans.id(),
                trans.finished()
            );
        }
        println!("    {}", panel_buffer_signature(&page.scene, PANEL_ID));
        println!(
            "    composite invalidation empty? {}",
            page.scene.invalidation.composite.is_empty()
        );

        // 4. Tick the runtime several times, watching the panel buffer mutate.
        println!("\n--- step 4: tick runtime, observe panel buffer ---");
        let mut compositor = crate::compositor::composite::Compositor::new(term_w, term_h);
        // Force first composite to populate the cache.
        let frame0 = compositor.composite(&page.scene, &page.anim_rt).unwrap();
        println!(
            "  composite#0 produced frame {}x{} (shared strong_count={})",
            frame0.width,
            frame0.height,
            triomphe::Arc::strong_count(&frame0)
        );
        page.scene.invalidation.composite.clear();
        page.scene.invalidation.present.clear();
        drop(frame0);

        let start = Instant::now();
        for tick_n in 0..25 {
            let now = start + Duration::from_millis((tick_n as u64) * 33);
            let tick_result = page.anim_rt.tick(
                &mut page.scene,
                now,
                state.scroll_offset,
                state.viewport_height(),
            );
            // Mirror what terminal.rs does: mark composite-dirty for wrote_buffers.
            for node_id in &tick_result.wrote_buffers {
                if let Some(node) = page.scene.get(*node_id) {
                    let rect = node.placement().rect;
                    if !rect.is_empty() {
                        page.scene.invalidation.mark_composite(rect);
                    }
                }
            }
            page.anim_rt.paint_into_scene(&mut page.scene);

            let inv_was_empty = page.scene.invalidation.composite.is_empty();
            let frame = compositor.composite(&page.scene, &page.anim_rt).unwrap();
            let cache_hit = triomphe::Arc::strong_count(&frame) > 1;
            page.scene.invalidation.composite.clear();
            page.scene.invalidation.present.clear();

            let trans_count = page.anim_rt.transition_animations.len();
            let trans_state: String = page
                .anim_rt
                .transition_animations
                .iter()
                .map(|t| format!("{}:t={:.2}", t.id(), t.t()))
                .collect::<Vec<_>>()
                .join(", ");

            println!(
                "  tick {:>2}: changed={} wrote_buffers={} inv_empty_pre={} cache_hit={} trans=[{}] trans_count={} | {}",
                tick_n,
                tick_result.changed,
                tick_result.wrote_buffers.len(),
                inv_was_empty,
                cache_hit,
                trans_state,
                trans_count,
                panel_buffer_signature(&page.scene, PANEL_ID),
            );
            drop(frame);
        }

        // 5. Visual dump of the final buffer.
        println!("\n--- step 5: final panel buffer (t=1.0) ---");
        if let Some(node_id) = page.scene.find_by_aml_id(PANEL_ID)
            && let Some(node) = page.scene.get(node_id)
            && let Some(buf) = node.buffer()
        {
            for y in 0..buf.height {
                let mut row = String::new();
                for x in 0..buf.width {
                    let cell = buf
                        .get(x, y)
                        .cloned()
                        .unwrap_or(crate::compositor::layout::cell::Cell::empty());
                    row.push(if cell.ch == '\0' { '.' } else { cell.ch });
                }
                println!("    │{}│", row);
            }
        }

        println!("\n=== END DEBUG ===\n");
    }
}
#[test]
fn scanner_construction_pressure_is_recoverable_not_authored_invalidity() {
    assert_eq!(
        classify_scan_error(&crate::scanner::ScanError::ResourceExhausted { requested: 7 }),
        RemoteParseError::ResourceRejected
    );
    assert_eq!(
        classify_scan_error(&crate::scanner::ScanError::InvalidUtf8),
        RemoteParseError::Invalid
    );
}

/// Every layout allocation site refuses the whole page, and the same input
/// succeeds once the site is disarmed.
///
/// Layout previously had no rejection injection: its tests exhausted a small
/// real governor, which shows a refusal is *possible* but not *which*
/// allocation refused, so it could not show one candidate failing while the
/// rest of the page rolls back. Arming a named site does both — and the
/// recovery half is what proves the refusal left nothing behind, rather than
/// merely that the first attempt failed.
#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn layout_allocation_rejection_refuses_the_page_at_every_site_and_recovers() {
    use crate::compositor::layout::{LayoutAllocationSite, LayoutRejectionGuard};

    let doc = parse_aml(
        r#"[page mode=document]
            [heading level=1]A heading long enough to wrap across the width[/heading]
            [input name="query"]
            [row][col][text]left column[/text][/col][col][text]right[/text][/col][/row]
            [table]
                [tr][th]Header[/th][th]Other[/th][/tr]
                [tr][td]a value[/td][td]another[/td][/tr]
            [/table]
            [link id="docs" href="/documentation"][text]Read the docs[/text][/link]
            [box sticky=bottom][text]footer[/text][/box]
        [/page]"#,
    )
    .unwrap();

    // The baseline is measured before anything is armed, so recovery is
    // compared against what this document actually produces rather than
    // against an assumption about it.
    let baseline = layout_page_with_admission(
        &doc,
        40,
        12,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("baseline page must lay out");
    let (baseline_placed, baseline_focusables) = (baseline.placed.len(), baseline.focusables.len());
    assert!(
        baseline_focusables > 0,
        "the fixture must produce focusables for recovery to mean anything"
    );
    drop(baseline);

    for site in [
        LayoutAllocationSite::TempVec,
        LayoutAllocationSite::FixedMetadata,
        LayoutAllocationSite::WrappedText,
    ] {
        let rejection = LayoutRejectionGuard::at(site);
        let refused = layout_page_with_admission(
            &doc,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            refused.is_err(),
            "{site:?} did not refuse the page; the hook is not on a load-bearing path"
        );
        drop(rejection);

        // The refusal must be exactly the injected one: with the site
        // disarmed the identical input has to produce a complete page.
        let accepted = layout_page_with_admission(
            &doc,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let page =
            accepted.unwrap_or_else(|_| panic!("page must succeed once {site:?} is disarmed"));
        assert_eq!(
            (page.placed.len(), page.focusables.len()),
            (baseline_placed, baseline_focusables),
            "recovered page does not match the baseline after {site:?}"
        );
    }
}

/// Refusing the page canvas or the event queue refuses the whole page, and
/// the identical input succeeds once the site is disarmed.
///
/// Both are admitted during page preparation alongside the scene, the layout
/// metadata and the WASM batch. Exhausting a governor refuses whichever the
/// budget reaches first; naming the site is what shows each one individually
/// fails closed rather than producing a page missing its canvas or its input
/// queue.
#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn page_canvas_and_event_queue_rejection_refuse_the_page_and_recover() {
    use crate::compositor::terminal::runner::{RunnerAllocationSite, RunnerRejectionGuard};

    let doc = parse_aml(
        r#"[page mode=document]
            [button action="toggle" target="p" states="a,b"]Toggle[/button]
            [panel id="p" state="a"]
                [state name="a"][text]A[/text][/state]
                [state name="b"][text]B[/text][/state]
            [/panel]
            [box sticky=bottom][text]footer[/text][/box]
        [/page]"#,
    )
    .unwrap();

    for site in [
        RunnerAllocationSite::PageCanvas,
        RunnerAllocationSite::EventQueue,
    ] {
        let rejection = RunnerRejectionGuard::at(site);
        let refused = layout_page_with_admission(
            &doc,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(refused.is_err(), "{site:?} did not refuse the page");
        drop(rejection);

        let accepted = layout_page_with_admission(
            &doc,
            40,
            12,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            accepted.is_ok(),
            "the page must lay out once {site:?} is disarmed"
        );
    }
}

/// A refused region-row reservation keeps the previous generation intact.
///
/// `RegionBuffer::rebuild` holds the old lease while reserving and building
/// the replacement, so both generations coexist until a transactional swap.
/// Exhausting the governor refuses at the lease, before the rows are reserved;
/// naming the site refuses at the reservation instead, which is the only way
/// to exercise the window where two generations are live at once.
#[test]
fn region_row_rejection_keeps_the_previous_generation_and_recovers() {
    use crate::compositor::layout::cell::CellStyle;
    use crate::compositor::terminal::runner::{RunnerAllocationSite, RunnerRejectionGuard};

    let governor = ResourceGovernor::new();
    let mut mini = CellBuffer::new(1, 1);
    mini.put_char(0, 0, 'x', &CellStyle::default());
    let key = SubscriptionRegionKey::from_placed_index(0).unwrap();
    let mut table = RegionBuffers::new();
    assert!(table.update(key, 1, 1, 1, &governor, &mini, RegionBufferUpdate::Replace,));
    let installed = governor.used(ResourceCategory::SceneCells);
    assert!(installed > 0);

    let rejection = RunnerRejectionGuard::at(RunnerAllocationSite::RegionRows);
    assert!(
        !table.update(key, 1, 1, 1, &governor, &mini, RegionBufferUpdate::Append,),
        "a refused row reservation must not report success"
    );
    assert_eq!(
        governor.used(ResourceCategory::SceneCells),
        installed,
        "the refused replacement must leave the previous generation's cells"
    );
    drop(rejection);

    assert!(table.update(key, 1, 1, 1, &governor, &mini, RegionBufferUpdate::Append,));
    drop(table);
    assert_eq!(governor.used(ResourceCategory::SceneCells), 0);
    assert_eq!(governor.count(ResourceCategory::SceneCells), 0);
}

/// A refused panel-transition capture yields a buffer that reports its
/// failure, so the transition is never installed with half its pixels.
///
/// `TransitionAdapter`'s two buffers are the output of this capture. Their
/// accounting test constructs them directly, which cannot exercise the
/// production path where the sub-buffer extraction is refused and the caller
/// has to notice.
#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn refused_panel_transition_capture_reports_failure_and_recovers() {
    use crate::compositor::terminal::runner::{RunnerAllocationSite, RunnerRejectionGuard};

    let page = layout_page(
        parse_aml(
            r#"[page mode=document]
            [panel id="p" state="a" transition=fade]
                [state name="a"][text]A[/text][/state]
                [state name="b"][text]B[/text][/state]
            [/panel]
        [/page]"#,
        )
        .unwrap(),
        40,
        12,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        None,
        None,
        None,
    )
    .await;

    let rejection = RunnerRejectionGuard::at(RunnerAllocationSite::SubBuffer);
    let refused = capture_panel_transition_source(&page, "p");
    assert!(
        refused
            .as_ref()
            .is_some_and(|(_, buf)| buf.allocation_failed()),
        "a refused capture must report its failure rather than returning pixels"
    );
    drop(rejection);

    let captured = capture_panel_transition_source(&page, "p");
    assert!(
        captured
            .as_ref()
            .is_some_and(|(_, buf)| !buf.allocation_failed()),
        "the capture must succeed once the site is disarmed"
    );
}

/// A terminal that has gone away reports `Ok((0, 0))`, not an error.
///
/// The viewer's main loop polls the terminal descriptor; once the terminal is
/// gone that descriptor is permanently readable-at-EOF, so `crossterm`'s poll
/// spins inside `read(2)` and never returns. Because it never returns, the
/// loop never reaches its termination-flag check either — so the process
/// ignores SIGTERM and burns a core until it is SIGKILLed. Orphaned viewers
/// were observed doing exactly that for three days.
///
/// The only place to catch it is before entering the doomed poll, and the
/// only signal available is the size. Checking `is_err()` — the obvious
/// reading — detects none of it, because the dead descriptor answers happily
/// with zeroes.
///
/// Reading `Ok((0, 0))` as loss on its own is the opposite error, and it was
/// made here first: a pty nobody has sized answers identically, and under a
/// test harness with no controlling terminal that is *every* pty. A viewer
/// that quits on it quits before it can be driven at all. Only the transition
/// is loss, so the decision carries history.
#[test]
fn zero_size_is_loss_only_after_a_real_size_has_been_seen() {
    use crate::compositor::terminal::runner::TerminalPresence;
    use std::io::{Error, ErrorKind};

    let mut sized = TerminalPresence::default();
    assert!(sized.observe(Ok((80, 24))));
    assert!(sized.observe(Ok((1, 1))));
    // The case that actually occurs when a pty master closes.
    assert!(
        !sized.observe(Ok((0, 0))),
        "a dead terminal reports Ok((0, 0)); treating it as usable is the bug"
    );

    let mut half_sized = TerminalPresence::default();
    assert!(half_sized.observe(Ok((80, 24))));
    assert!(!half_sized.observe(Ok((80, 0))));

    // Never sized: an unsized pty is not a dead one, and the viewer must
    // keep running so it can be driven and asked to quit.
    let mut never_sized = TerminalPresence::default();
    assert!(never_sized.observe(Ok((0, 0))));
    assert!(never_sized.observe(Ok((0, 24))));

    // A failing ioctl is loss whether or not a size was ever seen: unlike a
    // zero size, it has no benign reading.
    let mut failing = TerminalPresence::default();
    assert!(!failing.observe(Err(Error::from(ErrorKind::NotConnected))));
}

/// Firing a `set` at a panel that already holds the requested state used to
/// hang the viewer: `apply_panel_patch` answers `false`, the old code
/// `continue`d without retiring the action, and `next_ready` — which only
/// peeks — handed the same action straight back, forever, at 100% of a core.
/// Duplicated `set`s are ordinary: fast-forward (`f`) re-fires `animation-end`
/// while collapsing every authored delay, and history restores pages already
/// settled in their final states.
#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[test]
fn redundant_authored_set_retires_instead_of_spinning() {
    // `to="a"` is the state the panel already holds, so the patch is a no-op.
    let aml = r#"[page mode=document]
            [panel id="p" state="a"]
                [state name="a"][text]A[/text][/state]
                [state name="b"][text]B[/text][/state]
            [/panel]
        [/page]"#;
    let binding = crate::compositor::scene::EventBinding {
        event: crate::parser::ast::EventKind::PageLoad,
        source: None,
        action: crate::parser::ast::ActionKind::Set,
        target: "p".into(),
        to: Some("a".into()),
        delay_ms: 0,
    };
    assert_eq!(
        drain_one_authored_action(aml, binding),
        Some(0),
        "an action this loop has peeked must be retired on every exit, or it is offered again",
    );
}

/// The same defect in the `Toggle` branch: a panel with a single state has
/// nothing to advance to, so `toggle_panel_scene_state` answers `false` and
/// the action must still retire.
#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[test]
fn untoggleable_panel_retires_instead_of_spinning() {
    let aml = r#"[page mode=document]
            [panel id="p" state="a"]
                [state name="a"][text]A[/text][/state]
            [/panel]
        [/page]"#;
    let binding = crate::compositor::scene::EventBinding {
        event: crate::parser::ast::EventKind::PageLoad,
        source: None,
        action: crate::parser::ast::ActionKind::Toggle,
        target: "p".into(),
        to: None,
        delay_ms: 0,
    };
    assert_eq!(drain_one_authored_action(aml, binding), Some(0));
}

/// Schedules `binding` against a page laid out from `aml`, drains the authored
/// action queue once, and answers how many actions remain pending — or `None`
/// if the drain did not finish.
///
/// The drain runs on its own thread because the failure it guards is a busy
/// loop that never yields: `tokio::time::timeout` cannot preempt one, so a
/// regression would wedge the test rather than fail it. A thread plus
/// `recv_timeout` turns "never returned" into an ordinary assertion. The
/// spinning thread is abandoned on failure, which is the right trade for a
/// test binary that is about to fail anyway.
fn drain_one_authored_action(
    aml: &'static str,
    binding: crate::compositor::scene::EventBinding,
) -> Option<usize> {
    let (report, drained) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the drain thread must own a runtime");
        let remaining = runtime.block_on(async move {
            let mut page = layout_page(
                parse_aml(aml).unwrap(),
                40,
                12,
                ColorSupport::Truecolor,
                WidthConfig::default(),
                None,
                None,
                None,
            )
            .await;
            let mut state = ViewportState::with_sticky(40, 12, page.buf.height, &page.sticky_buf);
            let bindings = [binding];
            let mut dispatcher = EventDispatcher::new();
            let prepared = dispatcher
                .prepare_fire(&bindings, crate::parser::ast::EventKind::PageLoad, None, 0)
                .unwrap();
            dispatcher.commit(prepared);
            assert_eq!(dispatcher.pending_len(), 1);

            let mut needs_redraw = false;
            execute_on_actions(
                &bindings,
                &mut page,
                &mut state,
                ColorSupport::Truecolor,
                WidthConfig::default(),
                None,
                &mut dispatcher,
                &mut needs_redraw,
            )
            .await
            .expect("an action with nothing to apply is not an error");
            dispatcher.pending_len()
        });
        let _ = report.send(remaining);
    });
    drained
        .recv_timeout(std::time::Duration::from_secs(10))
        .ok()
}
