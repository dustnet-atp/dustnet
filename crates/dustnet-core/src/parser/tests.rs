use super::*;
use crate::color::{Color, NamedColor};
use crate::scanner::Scanner;

fn parse_aml(input: &str) -> ParseResult {
    let mut scanner = Scanner::new(input.as_bytes()).unwrap();
    let tokens = scanner.scan_all().unwrap();
    parse(tokens)
}

fn parse_ok(input: &str) -> Document {
    let result = parse_aml(input);
    if result.has_errors() {
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.level == DiagnosticLevel::Error)
            .collect();
        panic!("parse errors: {errors:?}");
    }
    result.document.unwrap()
}

fn parse_with_errors(input: &str) -> ParseResult {
    let result = parse_aml(input);
    assert!(result.has_errors(), "expected errors but got none");
    result
}

fn has_diagnostic_code(result: &ParseResult, code: &str) -> bool {
    result.diagnostics.iter().any(|d| d.code == code)
}

// ─── Minimal Valid Documents ─────────────────────────────────

#[test]
fn minimal_document_mode() {
    let doc = parse_ok("[page mode=document][text]hello[/text][/page]");
    assert_eq!(doc.page.mode, PageMode::Document);
    assert_eq!(doc.page.children.len(), 1);
}

#[test]
fn minimal_screen_mode() {
    let doc = parse_ok("[page mode=screen cols=80 rows=24][text]hi[/text][/page]");
    assert_eq!(
        doc.page.mode,
        PageMode::Screen {
            cols: Some(80),
            rows: Some(24)
        }
    );
}

#[test]
fn page_with_title() {
    let doc = parse_ok("[page mode=document title=\"My Page\"][/page]");
    assert_eq!(doc.page.title, Some("My Page".into()));
}

#[test]
fn screen_mode_defaults_to_terminal_size() {
    let doc = parse_ok("[page mode=screen][/page]");
    assert_eq!(
        doc.page.mode,
        PageMode::Screen {
            cols: None,
            rows: None
        }
    );
}

#[test]
fn screen_mode_partial_dims() {
    let doc = parse_ok("[page mode=screen cols=80][/page]");
    assert_eq!(
        doc.page.mode,
        PageMode::Screen {
            cols: Some(80),
            rows: None
        }
    );
}

// ─── Text Elements ───────────────────────────────────────────

#[test]
fn text_with_styles() {
    let doc = parse_ok("[page mode=document][text bold italic fg=red]styled[/text][/page]");
    match &doc.page.children[0] {
        Element::Text(t) => {
            assert!(t.bold);
            assert!(t.italic);
            assert_eq!(t.fg, Some(Color::Named(NamedColor::Red)));
            assert_eq!(t.content, "styled");
        }
        _ => panic!("expected Text"),
    }
}

#[test]
fn text_with_hex_color() {
    let doc = parse_ok("[page mode=document][text fg=#ff6600]colored[/text][/page]");
    match &doc.page.children[0] {
        Element::Text(t) => {
            assert_eq!(
                t.fg,
                Some(Color::Rgb {
                    r: 255,
                    g: 102,
                    b: 0
                })
            );
        }
        _ => panic!("expected Text"),
    }
}

#[test]
fn text_all_styles() {
    let doc = parse_ok(
        "[page mode=document][text bold italic underline strikethrough dim blink]x[/text][/page]",
    );
    match &doc.page.children[0] {
        Element::Text(t) => {
            assert!(t.bold);
            assert!(t.italic);
            assert!(t.underline);
            assert!(t.strikethrough);
            assert!(t.dim);
            assert!(t.blink);
        }
        _ => panic!("expected Text"),
    }
}

#[test]
fn text_alignment() {
    let doc = parse_ok("[page mode=document][text align=center]centered[/text][/page]");
    match &doc.page.children[0] {
        Element::Text(t) => assert_eq!(t.align, Alignment::Center),
        _ => panic!("expected Text"),
    }
}

#[test]
fn pre_element() {
    let doc = parse_ok("[page mode=document][pre]  art  \n  here  [/pre][/page]");
    match &doc.page.children[0] {
        Element::Pre(p) => {
            assert!(p.content.contains("art"));
            assert!(p.content.contains("here"));
        }
        _ => panic!("expected Pre"),
    }
}

#[test]
fn heading_levels() {
    let doc = parse_ok("[page mode=document][heading level=2 fg=cyan]Title[/heading][/page]");
    match &doc.page.children[0] {
        Element::Heading(h) => {
            assert_eq!(h.level, 2);
            assert_eq!(h.fg, Some(Color::Named(NamedColor::Cyan)));
            assert_eq!(h.content, "Title");
        }
        _ => panic!("expected Heading"),
    }
}

// ─── Layout Elements ────────────────────────────────────────

#[test]
fn box_element() {
    let doc = parse_ok(
        "[page mode=document][box w=40 h=10 border=double fg=green padding=2 title=\"Status\"][text]inside[/text][/box][/page]",
    );
    match &doc.page.children[0] {
        Element::Box(b) => {
            assert_eq!(b.w, Dimension::Fixed(40));
            assert_eq!(b.h, Dimension::Fixed(10));
            assert_eq!(b.border, BorderStyle::Double);
            assert_eq!(b.fg, Some(Color::Named(NamedColor::Green)));
            assert_eq!(b.padding, 2);
            assert_eq!(b.title, Some("Status".into()));
            assert_eq!(b.children.len(), 1);
        }
        _ => panic!("expected Box"),
    }
}

#[test]
fn box_fill_fit_dimensions() {
    let doc = parse_ok("[page mode=document][box w=fill h=fit][/box][/page]");
    match &doc.page.children[0] {
        Element::Box(b) => {
            assert_eq!(b.w, Dimension::Fill);
            assert_eq!(b.h, Dimension::Fit);
        }
        _ => panic!("expected Box"),
    }
}

#[test]
fn box_connector_junctions() {
    let doc = parse_ok(
        "[page mode=document][box join-top=5 join-bottom=6 join-left=3 join-right=4][/box][/page]",
    );
    match &doc.page.children[0] {
        Element::Box(b) => {
            assert_eq!(b.join_top, Some(5));
            assert_eq!(b.join_bottom, Some(6));
            assert_eq!(b.join_left, Some(3));
            assert_eq!(b.join_right, Some(4));
        }
        _ => panic!("expected Box"),
    }
}

#[test]
fn row_col_layout() {
    let doc = parse_ok(
        "[page mode=document][row gap=3][col w=30]left[/col][col w=50]right[/col][/row][/page]",
    );
    match &doc.page.children[0] {
        Element::Row(r) => {
            assert_eq!(r.gap, 3);
            assert_eq!(r.children.len(), 2);
            match &r.children[0] {
                Element::Col(c) => assert_eq!(c.w, Dimension::Fixed(30)),
                _ => panic!("expected Col"),
            }
        }
        _ => panic!("expected Row"),
    }
}

#[test]
fn hr_element() {
    let doc = parse_ok("[page mode=document][hr style=dash fg=yellow /][/page]");
    match &doc.page.children[0] {
        Element::Hr(h) => {
            assert_eq!(h.style, HrStyle::Dash);
            assert_eq!(h.fg, Some(Color::Named(NamedColor::Yellow)));
        }
        _ => panic!("expected Hr"),
    }
}

#[test]
fn spacer_element() {
    let doc = parse_ok("[page mode=document][spacer lines=3 /][/page]");
    match &doc.page.children[0] {
        Element::Spacer(s) => assert_eq!(s.lines, 3),
        _ => panic!("expected Spacer"),
    }
}

#[test]
fn oversized_spacer_is_rejected_to_a_safe_default() {
    let result = parse_aml("[page mode=document][spacer lines=65535 /][/page]");
    assert!(result.diagnostics.iter().any(|diag| diag.code == "W002"));
    let doc = result.document.unwrap();
    match &doc.page.children[0] {
        Element::Spacer(s) => assert_eq!(s.lines, 1),
        _ => panic!("expected Spacer"),
    }
}

#[test]
fn sticky_nav() {
    let doc = parse_ok("[page mode=document][nav sticky=top][text]Home[/text][/nav][/page]");
    match &doc.page.children[0] {
        Element::Nav(n) => {
            assert_eq!(n.sticky, Some(StickyPosition::Top));
        }
        _ => panic!("expected Nav"),
    }
}

// ─── List Elements ───────────────────────────────────────────

#[test]
fn list_with_items() {
    let doc = parse_ok(
        "[page mode=document][list style=number][item]first[/item][item]second[/item][/list][/page]",
    );
    match &doc.page.children[0] {
        Element::List(l) => {
            assert_eq!(l.style, ListStyle::Number);
            assert_eq!(l.children.len(), 2);
        }
        _ => panic!("expected List"),
    }
}

// ─── Interactive Elements ────────────────────────────────────

#[test]
fn link_element() {
    let doc = parse_ok(
        "[page mode=document][link href=\"atp://neon.city/\" transition=\"dissolve\" key=v][text]Visit[/text][/link][/page]",
    );
    match &doc.page.children[0] {
        Element::Link(l) => {
            assert_eq!(l.href, "atp://neon.city/");
            assert_eq!(l.key, Some('v'));
            assert!(!l.prefetch);
            assert_eq!(l.children.len(), 1);
        }
        _ => panic!("expected Link"),
    }
}

#[test]
fn link_requires_href() {
    let result = parse_aml("[page mode=document][link][text]no href[/text][/link][/page]");
    assert!(has_diagnostic_code(&result, "E011"));
}

#[test]
fn input_element() {
    let doc = parse_ok(
        "[page mode=document][input name=\"msg\" maxlen=280 placeholder=\"Say something\" password /][/page]",
    );
    match &doc.page.children[0] {
        Element::Input(i) => {
            assert_eq!(i.name, "msg");
            assert_eq!(i.maxlen, 280);
            assert_eq!(i.placeholder, Some("Say something".into()));
            assert!(i.password);
        }
        _ => panic!("expected Input"),
    }
}

#[test]
fn input_requires_name() {
    let result = parse_aml("[page mode=document][input /][/page]");
    assert!(has_diagnostic_code(&result, "E011"));
}

#[test]
fn select_with_options() {
    let doc = parse_ok(
        "[page mode=document][select name=\"color\" label=\"Pick:\"][option value=\"red\"]Red[/option][option value=\"blue\" selected]Blue[/option][/select][/page]",
    );
    match &doc.page.children[0] {
        Element::Select(s) => {
            assert_eq!(s.name, "color");
            assert_eq!(s.label, Some("Pick:".into()));
            assert_eq!(s.children.len(), 2);
            match &s.children[1] {
                Element::Option(o) => {
                    assert_eq!(o.value, "blue");
                    assert!(o.selected);
                    assert_eq!(o.label, "Blue");
                }
                _ => panic!("expected Option"),
            }
        }
        _ => panic!("expected Select"),
    }
}

#[test]
fn button_element() {
    let doc =
        parse_ok("[page mode=document][button action=submit target=\"/post\"]Post[/button][/page]");
    match &doc.page.children[0] {
        Element::Button(b) => {
            assert_eq!(b.action, ButtonAction::Submit);
            assert_eq!(b.target, Some("/post".into()));
            assert_eq!(b.label, "Post");
        }
        _ => panic!("expected Button"),
    }
}

#[test]
fn form_element() {
    let doc = parse_ok(
        "[page mode=document][form action=\"/login\"][input name=\"user\" /][button action=submit]Go[/button][/form][/page]",
    );
    match &doc.page.children[0] {
        Element::Form(f) => {
            assert_eq!(f.action, "/login");
            assert_eq!(f.children.len(), 2);
        }
        _ => panic!("expected Form"),
    }
}

#[test]
fn forms_require_actions_and_selects_require_unambiguous_options() {
    let missing_action =
        parse_with_errors("[page mode=document][form][input name=\"x\" /][/form][/page]");
    assert!(has_diagnostic_code(&missing_action, "E011"));

    let empty_select =
        parse_with_errors("[page mode=document][select name=\"choice\"][/select][/page]");
    assert!(has_diagnostic_code(&empty_select, "E011"));

    let multiple_selected = parse_with_errors(
        "[page mode=document][select name=\"choice\"][option value=\"a\" selected]A[/option][option value=\"b\" selected]B[/option][/select][/page]",
    );
    assert!(has_diagnostic_code(&multiple_selected, "E011"));
}

// ─── Media Elements ──────────────────────────────────────────

#[test]
fn art_element() {
    let doc = parse_ok(
        "[page mode=document][art width=40 height=5 encoding=utf8 alt=\"logo\"]██ ART ██[/art][/page]",
    );
    match &doc.page.children[0] {
        Element::Art(a) => {
            assert_eq!(a.width, Some(40));
            assert_eq!(a.height, Some(5));
            assert_eq!(a.encoding, ArtEncoding::Utf8);
            assert_eq!(a.alt, Some("logo".into()));
            assert!(a.content.contains("ART"));
        }
        _ => panic!("expected Art"),
    }
}

#[test]
fn table_element() {
    let doc = parse_ok(
        "[page mode=document][table border=single][thead][tr][th]Name[/th][th]Score[/th][/tr][/thead][tbody][tr][td fg=green]Alice[/td][td]98[/td][/tr][/tbody][/table][/page]",
    );
    match &doc.page.children[0] {
        Element::Table(t) => {
            assert_eq!(t.border, BorderStyle::Single);
            assert_eq!(t.children.len(), 2); // thead + tbody
        }
        _ => panic!("expected Table"),
    }
}

// ─── Animation Elements ─────────────────────────────────────

#[test]
fn animate_element() {
    let doc = parse_ok(
        "[page mode=document][animate id=\"spin\" fps=12 loop=true][frame][text]A[/text][/frame][frame][text]B[/text][/frame][/animate][/page]",
    );
    match &doc.page.children[0] {
        Element::Animate(a) => {
            assert_eq!(a.id, "spin");
            assert_eq!(a.fps, 12);
            assert_eq!(a.loop_behavior, LoopBehavior::Infinite);
            assert_eq!(a.children.len(), 2); // 2 frames
        }
        _ => panic!("expected Animate"),
    }
}

#[test]
fn animate_fps_clamped() {
    let doc = parse_ok("[page mode=document][animate id=\"fast\" fps=60][/animate][/page]");
    match &doc.page.children[0] {
        Element::Animate(a) => {
            assert_eq!(a.fps, 30); // clamped to max
        }
        _ => panic!("expected Animate"),
    }
}

#[test]
fn animate_src_attribute() {
    let doc = parse_ok(
        "[page mode=document][animate id=\"intro\" src=\"/effects/typewriter.wasm\" fps=15][text]Hello world[/text][/animate][/page]",
    );
    match &doc.page.children[0] {
        Element::Animate(a) => {
            assert_eq!(a.id, "intro");
            assert_eq!(a.fps, 15);
            assert_eq!(a.src, Some("/effects/typewriter.wasm".to_string()));
            assert_eq!(a.children.len(), 1); // the text element
        }
        _ => panic!("expected Animate"),
    }
}

#[test]
fn animate_effect_compat_ignored() {
    let doc = parse_ok("[page mode=document][animate id=\"x\" effect=unknown][/animate][/page]");
    match &doc.page.children[0] {
        Element::Animate(a) => {
            assert_eq!(a.src, None);
        }
        _ => panic!("expected Animate"),
    }
}

#[test]
#[cfg_attr(miri, ignore = "1,025-region boundary is covered by native tests")]
fn rejects_more_than_1024_animation_regions() {
    let mut input = String::from("[page mode=document]");
    for index in 0..=MAX_ANIMATE_REGIONS {
        input.push_str(&format!("[animate id=\"a{index}\" /]"));
    }
    input.push_str("[/page]");

    let result = parse_with_errors(&input);
    assert!(has_diagnostic_code(&result, "E049"));
}

#[test]
fn rejects_wasm_instances_beyond_aggregate_memory_budget() {
    let mut input = String::from("[page mode=document]");
    for index in 0..=MAX_WASM_INSTANCES {
        input.push_str(&format!(
            "[animate id=\"w{index}\" src=\"/effects/{index}.wasm\" /]"
        ));
    }
    input.push_str("[/page]");

    let result = parse_with_errors(&input);
    assert!(has_diagnostic_code(&result, "E051"));
}

#[test]
#[cfg_attr(miri, ignore = "257-frame boundary is covered by native tests")]
fn rejects_more_than_256_animation_frames() {
    let mut input = String::from("[page mode=document][animate id=\"many\"]");
    for _ in 0..=MAX_ANIMATION_FRAMES {
        input.push_str("[frame /]");
    }
    input.push_str("[/animate][/page]");

    let result = parse_with_errors(&input);
    assert!(has_diagnostic_code(&result, "E050"));
}

#[test]
fn tween_element() {
    let doc = parse_ok(
        "[page mode=document][element id=\"star\" x=10 y=5 fg=white]★[/element][tween target=\"star\" duration=2s loop=bounce easing=ease-in-out][at t=0%]x=10 fg=white[/at][at t=100%]x=70 fg=red[/at][/tween][/page]",
    );

    // Find the tween element
    let tween = doc
        .page
        .children
        .iter()
        .find(|e| matches!(e, Element::Tween(_)));
    match tween {
        Some(Element::Tween(t)) => {
            assert_eq!(t.target, "star");
            assert_eq!(t.duration_ms, 2000);
            assert_eq!(t.loop_behavior, LoopBehavior::Bounce);
            assert_eq!(t.easing, Easing::EaseInOut);
            assert_eq!(t.keyframes.len(), 2);
            assert_eq!(t.keyframes[0].t_percent, 0.0);
            assert_eq!(t.keyframes[0].x, Some(10));
            assert_eq!(t.keyframes[1].t_percent, 100.0);
            assert_eq!(t.keyframes[1].x, Some(70));
        }
        _ => panic!("expected Tween"),
    }
}

#[test]
fn text_animate_element() {
    let doc = parse_ok(
        "[page mode=document][text-animate effect=typewriter speed=50ms]Hello world[/text-animate][/page]",
    );
    match &doc.page.children[0] {
        Element::TextAnimate(ta) => {
            assert_eq!(ta.effect, TextEffect::Typewriter);
            assert_eq!(ta.speed_ms, 50);
            assert_eq!(ta.content, "Hello world");
        }
        _ => panic!("expected TextAnimate"),
    }
}

// ─── Live Elements ───────────────────────────────────────────

#[test]
fn live_element() {
    let doc = parse_ok(
        "[page mode=document][live id=\"chat\" endpoint=\"/chat/stream\" height=20 scroll=tail buffer=500][text dim]Loading...[/text][/live][/page]",
    );
    match &doc.page.children[0] {
        Element::Live(l) => {
            assert_eq!(l.id, "chat");
            assert_eq!(l.endpoint, "/chat/stream");
            assert_eq!(l.height, Dimension::Fixed(20));
            assert_eq!(l.scroll, LiveScroll::Tail);
            assert_eq!(l.buffer, 500);
            assert!(!l.delta);
            assert_eq!(l.children.len(), 1);
        }
        _ => panic!("expected Live"),
    }
}

// ─── Structural Validation ───────────────────────────────────

#[test]
fn col_must_be_in_row() {
    let result = parse_aml("[page mode=document][col w=30]x[/col][/page]");
    assert!(has_diagnostic_code(&result, "E002"));
}

#[test]
fn item_must_be_in_list() {
    let result = parse_aml("[page mode=document][item]x[/item][/page]");
    assert!(has_diagnostic_code(&result, "E003"));
}

#[test]
fn option_must_be_in_select() {
    let result = parse_aml("[page mode=document][option value=x]X[/option][/page]");
    assert!(has_diagnostic_code(&result, "E004"));
}

#[test]
fn tr_must_be_in_table() {
    let result = parse_aml("[page mode=document][tr][td]x[/td][/tr][/page]");
    assert!(has_diagnostic_code(&result, "E005"));
}

#[test]
fn td_must_be_in_tr() {
    let result = parse_aml("[page mode=document][table][td]x[/td][/table][/page]");
    assert!(has_diagnostic_code(&result, "E006"));
}

#[test]
fn frame_must_be_in_animate() {
    let result = parse_aml("[page mode=document][frame][text]x[/text][/frame][/page]");
    assert!(has_diagnostic_code(&result, "E007"));
}

// ─── Error Recovery ──────────────────────────────────────────

#[test]
fn unknown_tag_warns() {
    let result = parse_aml("[page mode=document][foobar]content[/foobar][/page]");
    assert!(has_diagnostic_code(&result, "W001"));
    // Should still produce a document
    assert!(result.document.is_some());
}

#[test]
fn unknown_attribute_warns() {
    let result = parse_aml("[page mode=document][text foobar=xyz]hi[/text][/page]");
    assert!(has_diagnostic_code(&result, "W002"));
    assert!(result.document.is_some());
}

#[test]
fn missing_close_tag_warns() {
    let result = parse_aml("[page mode=document][text]unclosed");
    assert!(has_diagnostic_code(&result, "W003"));
    // Should still produce a document via recovery
    assert!(result.document.is_some());
}

#[test]
fn mismatched_close_tag_warns() {
    let result = parse_aml("[page mode=document][box][text]content[/box][/text][/page]");
    assert!(has_diagnostic_code(&result, "W004"));
    assert!(result.document.is_some());
}

#[test]
fn invalid_color_warns() {
    let result = parse_aml("[page mode=document][text fg=orange]hi[/text][/page]");
    assert!(has_diagnostic_code(&result, "E011"));
    assert!(result.document.is_some());
}

// ─── Empty/Missing Root ──────────────────────────────────────

#[test]
fn empty_document_errors() {
    let result = parse_with_errors("");
    assert!(has_diagnostic_code(&result, "E001"));
}

#[test]
fn no_page_root_errors() {
    let result = parse_with_errors("[text]not a page[/text]");
    assert!(has_diagnostic_code(&result, "E001"));
}

// ─── Complex Documents ──────────────────────────────────────

#[test]
fn realistic_page() {
    let input = r#"[page mode=document title="My Site"]
  [header]
    [text bold fg=cyan]Welcome[/text]
  [/header]
  [body]
    [box border=double fg=green w=60 title="Status"]
      [text]All systems operational[/text]
    [/box]
    [spacer lines=1 /]
    [hr style=dash fg=yellow /]
    [list style=bullet]
      [item][text]First item[/text][/item]
      [item][text]Second item[/text][/item]
    [/list]
    [link href="atp://other.site/" transition="dissolve"]
      [text fg=bright-yellow]Visit[/text]
    [/link]
    [form action="/post"]
      [input name="message" maxlen=280 placeholder="Say something..." /]
      [button action=submit]Post[/button]
    [/form]
  [/body]
  [footer]
    [nav sticky=bottom]
      [text dim]Dustnet v0.1.0[/text]
    [/nav]
  [/footer]
[/page]"#;

    let doc = parse_ok(input);
    assert_eq!(doc.page.mode, PageMode::Document);
    assert_eq!(doc.page.title, Some("My Site".into()));
    // header + body + footer
    assert_eq!(doc.page.children.len(), 3);
}

#[test]
fn screen_mode_page() {
    let input = r#"[page mode=screen cols=80 rows=24 title="Splash"]
  [box x=10 y=5 w=60 h=10 border=double fg=cyan]
    [text bold align=center]WELCOME[/text]
  [/box]
[/page]"#;

    let doc = parse_ok(input);
    assert_eq!(
        doc.page.mode,
        PageMode::Screen {
            cols: Some(80),
            rows: Some(24)
        }
    );
    match &doc.page.children[0] {
        Element::Box(b) => {
            assert_eq!(b.x, Some(10));
            assert_eq!(b.y, Some(5));
        }
        _ => panic!("expected Box"),
    }
}

// ─── Nesting Depth Limit ─────────────────────────────────────

#[test]
fn rejects_deep_nesting() {
    let mut input = String::from("[page mode=document]");
    for _ in 0..35 {
        input.push_str("[box]");
    }
    input.push_str("[text]deep[/text]");
    for _ in 0..35 {
        input.push_str("[/box]");
    }
    input.push_str("[/page]");

    let result = parse_aml(&input);
    assert!(has_diagnostic_code(&result, "E009"));
}

// ─── Diagnostic Count ────────────────────────────────────────

#[test]
fn clean_document_no_diagnostics() {
    let result = parse_aml("[page mode=document][text]hello[/text][/page]");
    assert_eq!(result.diagnostics.len(), 0);
}

#[test]
fn multiple_warnings_collected() {
    let result = parse_aml("[page mode=document][text foo=bar baz=qux]hi[/text][/page]");
    // Should have 2 W002 warnings for unknown attributes
    let w002_count = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "W002")
        .count();
    assert_eq!(w002_count, 2);
}

// ─── Panel Tests ─────────────────────────────────────────────

#[test]
fn panel_with_two_states() {
    let doc = parse_ok(
        r#"[page mode=document]
        [panel id="toggle" state="off"]
            [state name="off"][text]OFF[/text][/state]
            [state name="on"][text]ON[/text][/state]
        [/panel]
        [/page]"#,
    );
    let panel = doc
        .page
        .children
        .iter()
        .find(|c| matches!(c, Element::Panel(_)));
    match panel {
        Some(Element::Panel(p)) => {
            assert_eq!(p.id, "toggle");
            assert_eq!(p.initial_state, "off");
            let states: Vec<_> = p
                .children
                .iter()
                .filter_map(|c| {
                    if let Element::State(s) = c {
                        Some(s.name.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(states, vec!["off", "on"]);
        }
        _ => panic!("expected Panel"),
    }
}

#[test]
fn panel_state_with_transition() {
    let doc = parse_ok(
        r#"[page mode=document]
        [panel id="drawer" state="closed"]
            [state name="closed" h=1][text]Closed[/text][/state]
            [state name="open" h=fit transition="slide-down" duration=200ms][text]Open[/text][/state]
        [/panel]
        [/page]"#,
    );
    match &doc.page.children[0] {
        Element::Panel(p) => {
            let open_state = p
                .children
                .iter()
                .find_map(|c| {
                    if let Element::State(s) = c {
                        if s.name == "open" { Some(s) } else { None }
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(open_state.transition, Some("slide-down".into()));
            assert_eq!(open_state.duration_ms, 200);
            assert_eq!(open_state.h, Some(Dimension::Fit));
        }
        _ => panic!("expected Panel"),
    }
}

#[test]
fn panel_no_states_error() {
    let result = parse_aml("[page mode=document][panel id=\"p\" state=\"x\"][/panel][/page]");
    assert!(has_diagnostic_code(&result, "E025"));
}

#[test]
fn panel_duplicate_state_error() {
    let result = parse_aml(
        r#"[page mode=document]
        [panel id="p" state="a"]
            [state name="a"][/state]
            [state name="a"][/state]
        [/panel]
        [/page]"#,
    );
    assert!(has_diagnostic_code(&result, "E024"));
}

#[test]
fn panel_initial_state_not_found_error() {
    let result = parse_aml(
        r#"[page mode=document]
        [panel id="p" state="missing"]
            [state name="a"][/state]
            [state name="b"][/state]
        [/panel]
        [/page]"#,
    );
    assert!(has_diagnostic_code(&result, "E026"));
}

#[test]
fn panel_requires_id() {
    let result = parse_aml(
        "[page mode=document][panel state=\"x\"][state name=\"x\"][/state][/panel][/page]",
    );
    assert!(has_diagnostic_code(&result, "E011"));
}

#[test]
fn state_outside_panel_error() {
    let result = parse_aml("[page mode=document][state name=\"x\"][/state][/page]");
    assert!(has_diagnostic_code(&result, "E025"));
}

// ─── Button Toggle/Set Tests ─────────────────────────────────

#[test]
fn button_toggle_action() {
    let doc = parse_ok(
        r#"[page mode=document]
        [panel id="panel1" state="off"]
            [state name="off"][/state]
            [state name="on"][/state]
        [/panel]
        [button action="toggle" target="panel1" states="off,on"]Toggle[/button]
        [/page]"#,
    );
    let button = doc
        .page
        .children
        .iter()
        .find(|c| matches!(c, Element::Button(_)));
    match button {
        Some(Element::Button(b)) => {
            assert_eq!(b.action, ButtonAction::Toggle);
            assert_eq!(b.target, Some("panel1".into()));
            assert_eq!(b.states, Some(vec!["off".into(), "on".into()]));
        }
        _ => panic!("expected Button"),
    }
}

#[test]
fn button_set_action() {
    let doc = parse_ok(
        r#"[page mode=document]
        [panel id="panel1" state="off"]
            [state name="off"][/state]
            [state name="active"][/state]
        [/panel]
        [button action="set" target="panel1" to="active"]Set[/button]
        [/page]"#,
    );
    let button = doc
        .page
        .children
        .iter()
        .find(|c| matches!(c, Element::Button(_)));
    match button {
        Some(Element::Button(b)) => {
            assert_eq!(b.action, ButtonAction::Set);
            assert_eq!(b.target, Some("panel1".into()));
            assert_eq!(b.to, Some("active".into()));
        }
        _ => panic!("expected Button"),
    }
}

#[test]
fn button_toggle_needs_two_states() {
    let result = parse_aml(
        "[page mode=document][button action=\"toggle\" target=\"p\" states=\"only-one\"]X[/button][/page]",
    );
    assert!(has_diagnostic_code(&result, "E023"));
}

#[test]
fn button_set_needs_to() {
    let result =
        parse_aml("[page mode=document][button action=\"set\" target=\"p\"]X[/button][/page]");
    assert!(has_diagnostic_code(&result, "E011"));
}

// ─── Trigger Tests ───────────────────────────────────────────

#[test]
fn input_with_triggers() {
    let doc = parse_ok(
        r#"[page mode=document]
        [panel id="search" state="collapsed"]
            [state name="collapsed"][/state]
            [state name="expanded"][/state]
        [/panel]
        [input name="q" trigger-focus="search:expanded" trigger-blur="search:collapsed" /]
        [/page]"#,
    );
    let input = doc
        .page
        .children
        .iter()
        .find(|c| matches!(c, Element::Input(_)));
    match input {
        Some(Element::Input(i)) => {
            assert_eq!(
                i.triggers.trigger_focus,
                Some(TriggerRef {
                    panel_id: "search".into(),
                    state_name: "expanded".into()
                })
            );
            assert_eq!(
                i.triggers.trigger_blur,
                Some(TriggerRef {
                    panel_id: "search".into(),
                    state_name: "collapsed".into()
                })
            );
        }
        _ => panic!("expected Input"),
    }
}

#[test]
fn button_with_hover_triggers() {
    let doc = parse_ok(
        r#"[page mode=document]
        [panel id="tooltip" state="hidden"]
            [state name="hidden"][/state]
            [state name="visible"][/state]
        [/panel]
        [button action="submit" trigger-hover="tooltip:visible" trigger-unhover="tooltip:hidden"]Go[/button]
        [/page]"#,
    );
    let button = doc
        .page
        .children
        .iter()
        .find(|c| matches!(c, Element::Button(_)));
    match button {
        Some(Element::Button(b)) => {
            assert_eq!(
                b.triggers.trigger_hover,
                Some(TriggerRef {
                    panel_id: "tooltip".into(),
                    state_name: "visible".into()
                })
            );
            assert_eq!(
                b.triggers.trigger_unhover,
                Some(TriggerRef {
                    panel_id: "tooltip".into(),
                    state_name: "hidden".into()
                })
            );
        }
        _ => panic!("expected Button"),
    }
}

#[test]
fn link_with_transition_attrs_no_warning() {
    // transition/duration on links are parsed and stored — should not warn
    let doc = parse_ok(
        r#"[page mode=document]
        [link href="atp://x" transition="dissolve" duration=500ms][text]Go[/text][/link]
        [/page]"#,
    );
    match &doc.page.children[0] {
        Element::Link(l) => {
            assert_eq!(l.href, "atp://x");
            assert_eq!(l.transition, Some(ast::TransitionKind::Dissolve));
            assert_eq!(l.transition_duration_ms, 500);
        }
        _ => panic!("expected Link"),
    }
}

// ─── Page Transition Attributes ──────────────────────────────

#[test]
fn page_with_transition() {
    let doc =
        parse_ok(r#"[page mode=document title="Board" transition="fade" duration=500ms][/page]"#);
    assert_eq!(doc.page.transition, Some(ast::TransitionKind::Fade));
    assert_eq!(doc.page.transition_duration_ms, 500);
}

#[test]
fn page_with_slide_transition() {
    let doc = parse_ok(r#"[page mode=document transition="slide-left" duration=300ms][/page]"#);
    assert_eq!(doc.page.transition, Some(ast::TransitionKind::SlideLeft));
    assert_eq!(doc.page.transition_duration_ms, 300);
}

#[test]
fn page_without_transition() {
    let doc = parse_ok("[page mode=document][/page]");
    assert_eq!(doc.page.transition, None);
    assert_eq!(doc.page.transition_duration_ms, 300); // default
}

#[test]
fn link_with_transition() {
    let doc = parse_ok(
        r#"[page mode=document]
        [link href="atp://x" transition="slide-left" duration=300ms][text]Go[/text][/link]
        [/page]"#,
    );
    match &doc.page.children[0] {
        Element::Link(l) => {
            assert_eq!(l.href, "atp://x");
            assert_eq!(l.transition, Some(ast::TransitionKind::SlideLeft));
            assert_eq!(l.transition_duration_ms, 300);
        }
        _ => panic!("expected Link"),
    }
}

#[test]
fn link_without_transition() {
    let doc = parse_ok(
        r#"[page mode=document]
        [link href="atp://x"][text]Go[/text][/link]
        [/page]"#,
    );
    match &doc.page.children[0] {
        Element::Link(l) => {
            assert_eq!(l.transition, None);
            assert_eq!(l.transition_duration_ms, 300); // default
        }
        _ => panic!("expected Link"),
    }
}

#[test]
fn link_with_deferred_navigation() {
    let doc = parse_ok(
        r#"[page mode=document]
        [animate id="exit" fps=10 autoplay=false][frame][text]Bye[/text][/frame][/animate]
        [link href="/next" defer="exit"][text]Go[/text][/link]
        [/page]"#,
    );
    match &doc.page.children[1] {
        Element::Link(link) => assert_eq!(link.defer_animation.as_deref(), Some("exit")),
        _ => panic!("expected Link"),
    }
}

#[test]
fn button_navigate_with_transition() {
    let doc = parse_ok(
        r#"[page mode=document]
        [button action=navigate href="/board" transition="dissolve" duration=200ms]Go[/button]
        [/page]"#,
    );
    match &doc.page.children[0] {
        Element::Button(b) => {
            assert_eq!(b.action, ast::ButtonAction::Navigate);
            assert_eq!(b.href, Some("/board".into()));
            assert_eq!(b.transition, Some(ast::TransitionKind::Dissolve));
            assert_eq!(b.transition_duration_ms, 200);
        }
        _ => panic!("expected Button"),
    }
}

// ─── Panel + Layout Integration ──────────────────────────────

#[test]
#[cfg(any())] // client compositor integration is exercised in compositor tests
fn panel_renders_initial_state() {
    let input = r#"[page mode=document]
        [panel id="test" state="a"]
            [state name="a"][text]State A Content[/text][/state]
            [state name="b"][text]State B Content[/text][/state]
        [/panel]
    [/page]"#;

    let result = parse_aml(input);
    assert!(!result.has_errors());
    let doc = result.document.unwrap();

    // Layout should render state A's content. Phase 2 of the
    // composite-unification plan puts text in per-node buffers, so
    // the visible output is the composited view, not raw page.buf.
    use crate::color::ColorSupport;
    use crate::compositor::layout::engine::layout_scene;
    use crate::compositor::layout::text::WidthConfig;
    let mut scene = crate::compositor::scene::build::from_document(&doc);
    let page_buf = layout_scene(
        &mut scene,
        40,
        10,
        ColorSupport::Truecolor,
        WidthConfig::default(),
    )
    .buffer;
    let anim_rt = crate::compositor::animate::AnimationRuntime::new(Vec::new());
    let composed =
        crate::compositor::composite::walk(&scene, &anim_rt, page_buf.width, page_buf.height);
    let plain = crate::compositor::present::render_to_string(&composed);
    assert!(
        plain.contains("State A Content"),
        "should render initial state A"
    );
    assert!(
        !plain.contains("State B Content"),
        "should NOT render state B"
    );
}

// ─── Full Panel Example Check ────────────────────────────────

#[test]
fn panels_example_parses() {
    let input = r#"[page mode=document]
        [panel id="notif" state="off"]
            [state name="off"][text]OFF[/text][/state]
            [state name="on"][text]ON[/text][/state]
        [/panel]
        [button action="toggle" target="notif" states="off,on" key="n"]Toggle[/button]

        [panel id="tabs" state="files"]
            [state name="files"][text]File list here[/text][/state]
            [state name="edit"][text]Editor here[/text][/state]
        [/panel]
        [button action="set" target="tabs" to="files" key="1"]Files[/button]
        [button action="set" target="tabs" to="edit" key="2"]Edit[/button]
    [/page]"#;

    let result = parse_aml(input);
    assert!(
        !result.has_errors(),
        "panel example should parse without errors: {:?}",
        result.diagnostics
    );
}

// ─── Event Binding Parsing ──────────────────────────────────

#[test]
fn on_element_parses() {
    let doc = parse_ok(
        r#"[page mode=document]
        [animate id="hero" fps=10][frame][text]Hi[/text][/frame][/animate]
        [on event="page-load" do="animate" target="hero" /]
        [on event="animation-end" source="hero" do="set" target="hero" to="done" delay="300ms" /]
    [/page]"#,
    );

    let on_elements: Vec<_> = doc
        .page
        .children
        .iter()
        .filter_map(|c| {
            if let ast::Element::On(e) = c {
                Some(e)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(on_elements.len(), 2);

    assert_eq!(on_elements[0].event, ast::EventKind::PageLoad);
    assert_eq!(on_elements[0].action, ast::ActionKind::Animate);
    assert_eq!(on_elements[0].target, "hero");
    assert!(on_elements[0].source.is_none());
    assert_eq!(on_elements[0].delay_ms, 0);

    assert_eq!(on_elements[1].event, ast::EventKind::AnimationEnd);
    assert_eq!(on_elements[1].action, ast::ActionKind::Set);
    assert_eq!(on_elements[1].target, "hero");
    assert_eq!(on_elements[1].source.as_deref(), Some("hero"));
    assert_eq!(on_elements[1].to.as_deref(), Some("done"));
    assert_eq!(on_elements[1].delay_ms, 300);
}

#[test]
fn on_missing_event_errors() {
    let result = parse_with_errors(r#"[page mode=document][on do="animate" target="x" /][/page]"#);
    assert!(has_diagnostic_code(&result, "E041"));
}

#[test]
fn on_missing_action_errors() {
    let result =
        parse_with_errors(r#"[page mode=document][on event="page-load" target="x" /][/page]"#);
    assert!(has_diagnostic_code(&result, "E042"));
}

#[test]
fn on_missing_target_errors() {
    let result =
        parse_with_errors(r#"[page mode=document][on event="page-load" do="animate" /][/page]"#);
    assert!(has_diagnostic_code(&result, "E011"));
}

#[test]
fn on_animation_end_requires_source() {
    let result = parse_with_errors(
        r#"[page mode=document][on event="animation-end" do="animate" target="x" /][/page]"#,
    );
    assert!(has_diagnostic_code(&result, "E043"));
}

#[test]
fn retained_string_capacity_counts_metadata_and_nested_elements_exactly() {
    let mut title = String::with_capacity(32);
    title.push_str("title");
    let mut key = String::with_capacity(17);
    key.push_str("key");
    let mut value = String::with_capacity(29);
    value.push_str("value");
    let mut content = String::with_capacity(41);
    content.push_str("nested text");
    let expected = title.capacity() + key.capacity() + value.capacity() + content.capacity();

    let document = Document {
        page: Page {
            mode: PageMode::Document,
            title: Some(title),
            meta: vec![MetaEntry { key, value }],
            style: None,
            transition: None,
            transition_duration_ms: 0,
            children: vec![Element::Text(TextElement {
                content,
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
            })],
        },
    };

    assert_eq!(document.retained_string_capacity(), expected);
}

#[test]
fn parser_allocation_rejection_discards_every_candidate_phase_and_retries() {
    let cases = [
        (
            ParserAllocationSite::String,
            "[page title=hello][text]body[/text][/page]",
        ),
        (
            ParserAllocationSite::Collection,
            "[page][text]body[/text][/page]",
        ),
        (
            ParserAllocationSite::TokenCopy,
            "[page][text]body[/text][/page]",
        ),
        (ParserAllocationSite::Diagnostic, "[page][unknown /][/page]"),
        (
            ParserAllocationSite::ComponentMap,
            "[def name=card attrs=label][text]$label[/text][/def][page][card label=ok /][/page]",
        ),
        (
            ParserAllocationSite::SlotMap,
            "[def name=card slots=body][slot name=body /][/def][page][card][slot-content name=other][text]x[/text][/slot-content][/card][/page]",
        ),
        (
            ParserAllocationSite::Substitution,
            "[def name=card attrs=label][text]$label!tail[/text][/def][page][card label=\"a much longer replacement\" /][/page]",
        ),
        (
            ParserAllocationSite::ValidationMap,
            "[page][animate id=spinner /][/page]",
        ),
    ];

    for (site, input) in cases {
        let tokens = Scanner::new(input.as_bytes()).unwrap().scan_all().unwrap();
        REJECT_ALLOCATION.with(|rejected| rejected.set(Some(site)));
        let rejected = parse(tokens);
        REJECT_ALLOCATION.with(|rejected| rejected.set(None));

        assert!(rejected.resource_exhausted(), "site {site:?}");
        assert!(rejected.document.is_none(), "site {site:?}");

        let retried = parse_aml(input);
        assert!(!retried.resource_exhausted(), "site {site:?}");
        assert!(retried.document.is_some(), "site {site:?}");
    }
}

#[test]
fn expanding_component_substitution_preserves_the_trailing_literal() {
    let document = parse_ok(
        "[def name=card attrs=label][text]$label!tail[/text][/def]\
         [page][card label=\"a much longer replacement\" /][/page]",
    );
    match &document.page.children[0] {
        Element::Text(text) => assert_eq!(text.content, "a much longer replacement!tail"),
        other => panic!("expected expanded text, got {other:?}"),
    }
}

#[test]
fn public_value_parser_errors_preserve_the_offending_input() {
    assert_eq!(
        parse_alignment("sideways").unwrap_err(),
        "unknown alignment: sideways"
    );
    assert_eq!(
        parse_trigger_ref("missing-separator").unwrap_err(),
        "invalid trigger ref: missing-separator (expected panel-id:state-name)"
    );
}

// ─── Whitespace-only Trimmed Content ─────────────────────────

#[test]
fn whitespace_only_button_label_trims_to_empty() {
    let doc =
        parse_ok("[page mode=document][button action=submit]                  [/button][/page]");
    match &doc.page.children[0] {
        Element::Button(b) => assert_eq!(b.label, ""),
        other => panic!("expected Button, got {other:?}"),
    }
}

#[test]
fn whitespace_only_option_label_trims_to_empty() {
    let doc = parse_ok(
        "[page mode=document][select name=\"c\"][option value=\"a\"]   [/option][/select][/page]",
    );
    match &doc.page.children[0] {
        Element::Select(s) => match &s.children[0] {
            Element::Option(o) => assert_eq!(o.label, ""),
            other => panic!("expected Option, got {other:?}"),
        },
        other => panic!("expected Select, got {other:?}"),
    }
}

#[test]
fn whitespace_only_text_animate_content_trims_to_empty() {
    let doc = parse_ok(
        "[page mode=document][text-animate effect=typewriter speed=50ms]    [/text-animate][/page]",
    );
    match &doc.page.children[0] {
        Element::TextAnimate(t) => assert_eq!(t.content, ""),
        other => panic!("expected TextAnimate, got {other:?}"),
    }
}

#[test]
fn trim_owned_string_handles_whitespace_overlap() {
    // Leading and trailing runs overlap when the content is entirely
    // whitespace; the trimmed length must never exceed the truncated value.
    assert_eq!(trim_owned_string(String::new()), "");
    assert_eq!(trim_owned_string(" ".repeat(18)), "");
    assert_eq!(trim_owned_string("\t\n \r".to_string()), "");
    assert_eq!(trim_owned_string("  ab  ".to_string()), "ab");
    assert_eq!(trim_owned_string("ab".to_string()), "ab");
    assert_eq!(trim_owned_string("  ab".to_string()), "ab");
    assert_eq!(trim_owned_string("ab  ".to_string()), "ab");
}
