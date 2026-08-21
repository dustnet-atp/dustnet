use std::collections::{HashMap, HashSet};

use super::ast::*;
use super::{
    Diagnostic, DiagnosticLevel, MAX_ON_BINDINGS, ParserAllocationSite, mark_allocation_failure,
    reject_allocation, try_diagnostic, try_push,
};

/// Maximum cascade depth for event chains.
const MAX_CASCADE_DEPTH: usize = 4;

fn reject_map_allocation() -> bool {
    reject_allocation(ParserAllocationSite::ValidationMap)
}

macro_rules! diagnostic {
    ($diagnostics:expr, $failed:expr, $level:expr, $code:expr, $($args:tt)*) => {
        try_diagnostic(
            $diagnostics,
            $level,
            $code,
            format_args!($($args)*),
            $failed,
        )
    };
}

/// Post-parse validation: checks that all trigger references point to
/// existing panels and states, and validates [on] bindings.
pub fn validate_triggers(doc: &Document) -> (Vec<Diagnostic>, bool) {
    let mut diagnostics = Vec::new();
    let mut allocation_failed = false;

    // Collect all panels and their states
    let panels = collect_panels(&doc.page.children, &mut allocation_failed);
    let animations = collect_animation_ids(&doc.page.children, &mut allocation_failed);

    // Validate all triggers in the document
    validate_element_triggers(
        &doc.page.children,
        &panels,
        &animations,
        &mut diagnostics,
        &mut allocation_failed,
    );

    // Validate [on] bindings
    validate_on_bindings(
        &doc.page.children,
        &panels,
        &mut diagnostics,
        &mut allocation_failed,
    );

    // Flag visual-transition combinations that can expose retained terminal
    // cells or require animation topology to change during panel relayout.
    validate_visual_transition_risks(doc, &mut diagnostics, &mut allocation_failed);
    validate_animation_limits(doc, &mut diagnostics, &mut allocation_failed);

    (diagnostics, allocation_failed)
}

fn validate_animation_limits(
    doc: &Document,
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) {
    fn count(
        elements: &[Element],
        regions: &mut usize,
        wasm_instances: &mut usize,
        frames: &mut usize,
    ) {
        for element in elements {
            match element {
                Element::Animate(animation) => {
                    *regions += 1;
                    if animation.src.is_some() {
                        *wasm_instances += 1;
                    }
                }
                Element::Frame(_) => *frames += 1,
                _ => {}
            }
            count(element.children(), regions, wasm_instances, frames);
        }
    }

    let (mut regions, mut wasm_instances, mut frames) = (0, 0, 0);
    count(
        &doc.page.children,
        &mut regions,
        &mut wasm_instances,
        &mut frames,
    );
    if regions > super::MAX_ANIMATE_REGIONS {
        diagnostic!(
            diagnostics,
            allocation_failed,
            DiagnosticLevel::Error,
            "E049",
            "maximum {} animation regions per page, found {regions}",
            super::MAX_ANIMATE_REGIONS
        );
    }
    if wasm_instances > super::MAX_WASM_INSTANCES {
        diagnostic!(
            diagnostics,
            allocation_failed,
            DiagnosticLevel::Error,
            "E051",
            "maximum {} WASM instances per page, found {wasm_instances}",
            super::MAX_WASM_INSTANCES
        );
    }
    if frames > super::MAX_ANIMATION_FRAMES {
        diagnostic!(
            diagnostics,
            allocation_failed,
            DiagnosticLevel::Error,
            "E050",
            "maximum {} animation frames per page, found {frames}",
            super::MAX_ANIMATION_FRAMES
        );
    }
}

// ─── Visual transition risk validation ─────────────────────

fn validate_visual_transition_risks(
    doc: &Document,
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) {
    let has_animated_background = contains_background_animation(&doc.page.children);
    let panels = collect_panel_elements(&doc.page.children, allocation_failed);

    for panel in &panels {
        let Some(initial) = panel.children.iter().find_map(|element| match element {
            Element::State(state) if state.name == panel.initial_state => Some(state),
            _ => None,
        }) else {
            continue;
        };

        if doc.page.transition.is_some()
            && has_animated_background
            && is_transparent_fixed_placeholder(initial)
        {
            diagnostic!(
                diagnostics,
                allocation_failed,
                DiagnosticLevel::Warning,
                "W011",
                "panel \"{}\" starts as a transparent fixed-size placeholder over an animated background; render an opaque structural surface in the initial frame",
                panel.id
            );
        }

        let mut initial_wasm_ids = Vec::new();
        collect_wasm_animation_ids(&initial.children, &mut initial_wasm_ids, allocation_failed);
        let mut warned_wasm_ids = Vec::new();

        for element in &panel.children {
            let Element::State(state) = element else {
                continue;
            };
            if state.name == panel.initial_state {
                continue;
            }
            let mut wasm_ids = Vec::new();
            collect_wasm_animation_ids(&state.children, &mut wasm_ids, allocation_failed);
            for id in wasm_ids {
                if initial_wasm_ids.contains(&id) || warned_wasm_ids.contains(&id) {
                    continue;
                }
                try_push(&mut warned_wasm_ids, id, allocation_failed);
                diagnostic!(
                    diagnostics,
                    allocation_failed,
                    DiagnosticLevel::Warning,
                    "W013",
                    "WASM animation \"{id}\" exists only in non-initial state \"{}\" of panel \"{}\"; it must be created during panel relayout",
                    state.name,
                    panel.id
                );
            }
        }
    }

    if doc.page.transition.is_some() {
        let mut bindings = Vec::new();
        collect_on_bindings(&doc.page.children, &mut bindings, allocation_failed);
        for binding in bindings {
            if binding.event != EventKind::PageLoad
                || binding.action != ActionKind::Set
                || binding.delay_ms == 0
            {
                continue;
            }
            let Some(destination) = binding.to.as_deref() else {
                continue;
            };
            let Some(panel) = panels.iter().find(|panel| panel.id == binding.target) else {
                continue;
            };
            let destination_has_transition = panel.children.iter().any(|element| {
                matches!(element, Element::State(state)
                    if state.name == destination && state.transition.is_some())
            });
            if destination_has_transition {
                diagnostic!(
                    diagnostics,
                    allocation_failed,
                    DiagnosticLevel::Warning,
                    "W012",
                    "page transition overlaps delayed entrance of panel \"{}\"; transparent cells can expose the previous terminal frame",
                    panel.id
                );
            }
        }
    }
}

fn contains_background_animation(elements: &[Element]) -> bool {
    elements.iter().any(|element| {
        matches!(element, Element::Animate(animation) if animation.background)
            || contains_background_animation(element.children())
    })
}

fn collect_panel_elements<'a>(
    elements: &'a [Element],
    allocation_failed: &mut bool,
) -> Vec<&'a PanelElement> {
    let mut panels = Vec::new();
    for element in elements {
        if let Element::Panel(panel) = element {
            try_push(&mut panels, panel, allocation_failed);
        }
        let nested = collect_panel_elements(element.children(), allocation_failed);
        if panels.try_reserve(nested.len()).is_err() {
            mark_allocation_failure(allocation_failed);
        } else {
            panels.extend(nested);
        }
    }
    panels
}

fn is_transparent_fixed_placeholder(state: &StateElement) -> bool {
    state.children.len() == 1
        && matches!(state.children.first(), Some(Element::Box(box_element))
            if matches!(box_element.w, Dimension::Fixed(_))
                && matches!(box_element.h, Dimension::Fixed(_))
                && box_element.border == BorderStyle::None
                && box_element.bg.is_none()
                && box_element.children.is_empty())
}

fn collect_wasm_animation_ids<'a>(
    elements: &'a [Element],
    ids: &mut Vec<&'a str>,
    allocation_failed: &mut bool,
) {
    for element in elements {
        if let Element::Animate(animation) = element
            && animation.src.is_some()
        {
            try_push(ids, &animation.id, allocation_failed);
        }
        collect_wasm_animation_ids(element.children(), ids, allocation_failed);
    }
}

/// Map of panel_id → set of state names.
type PanelMap<'a> = HashMap<&'a str, HashSet<&'a str>>;

fn collect_animation_ids<'a>(
    elements: &'a [Element],
    allocation_failed: &mut bool,
) -> HashSet<&'a str> {
    let mut ids = HashSet::new();
    for elem in elements {
        if let Element::Animate(animation) = elem {
            if reject_map_allocation() || ids.try_reserve(1).is_err() {
                mark_allocation_failure(allocation_failed);
            } else {
                ids.insert(animation.id.as_str());
            }
        }
        let nested = collect_animation_ids(elem.children(), allocation_failed);
        if ids.try_reserve(nested.len()).is_err() {
            mark_allocation_failure(allocation_failed);
        } else {
            ids.extend(nested);
        }
    }
    ids
}

fn collect_panels<'a>(elements: &'a [Element], allocation_failed: &mut bool) -> PanelMap<'a> {
    let mut panels = PanelMap::new();
    collect_panels_recursive(elements, &mut panels, allocation_failed);
    panels
}

fn collect_panels_recursive<'a>(
    elements: &'a [Element],
    panels: &mut PanelMap<'a>,
    allocation_failed: &mut bool,
) {
    for elem in elements {
        if let Element::Panel(p) = elem {
            let mut states = HashSet::new();
            for child in &p.children {
                if let Element::State(s) = child {
                    if reject_map_allocation() || states.try_reserve(1).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        states.insert(s.name.as_str());
                    }
                }
            }
            if reject_map_allocation() || panels.try_reserve(1).is_err() {
                mark_allocation_failure(allocation_failed);
            } else {
                panels.insert(p.id.as_str(), states);
            }
        }
        collect_panels_recursive(elem.children(), panels, allocation_failed);
    }
}

fn validate_element_triggers(
    elements: &[Element],
    panels: &PanelMap,
    animations: &HashSet<&str>,
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) {
    for elem in elements {
        match elem {
            Element::Button(b) => {
                // Validate toggle/set target and states
                if matches!(b.action, ButtonAction::Toggle | ButtonAction::Set)
                    && let Some(ref target) = b.target
                {
                    if let Some(panel_states) = panels.get(target.as_str()) {
                        // Validate states list for toggle
                        if let Some(ref states) = b.states {
                            for state_name in states {
                                if !panel_states.contains(state_name.as_str()) {
                                    diagnostic!(
                                        diagnostics,
                                        allocation_failed,
                                        DiagnosticLevel::Error,
                                        "E022",
                                        "state \"{state_name}\" not found on panel \"{target}\""
                                    );
                                }
                            }
                        }
                        // Validate to for set
                        if let Some(ref to) = b.to
                            && !panel_states.contains(to.as_str())
                        {
                            diagnostic!(
                                diagnostics,
                                allocation_failed,
                                DiagnosticLevel::Error,
                                "E021",
                                "state \"{to}\" not found on panel \"{target}\""
                            );
                        }
                    } else {
                        diagnostic!(
                            diagnostics,
                            allocation_failed,
                            DiagnosticLevel::Error,
                            "E020",
                            "trigger target panel \"{target}\" not found"
                        );
                    }
                }
                validate_trigger_attrs(&b.triggers, panels, diagnostics, allocation_failed);
            }
            Element::Input(i) => {
                validate_trigger_attrs(&i.triggers, panels, diagnostics, allocation_failed);
            }
            Element::Link(l) => {
                validate_trigger_attrs(&l.triggers, panels, diagnostics, allocation_failed);
                if let Some(defer_animation) = &l.defer_animation
                    && !animations.contains(defer_animation.as_str())
                {
                    diagnostic!(
                        diagnostics,
                        allocation_failed,
                        DiagnosticLevel::Error,
                        "E048",
                        "deferred navigation animation \"{defer_animation}\" not found"
                    );
                }
            }
            _ => {}
        }

        // Recurse into children
        for_each_child(elem, |children| {
            validate_element_triggers(children, panels, animations, diagnostics, allocation_failed);
        });
    }
}

fn validate_trigger_attrs(
    triggers: &TriggerAttrs,
    panels: &PanelMap,
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) {
    for (label, trigger_ref) in [
        ("trigger-focus", &triggers.trigger_focus),
        ("trigger-blur", &triggers.trigger_blur),
        ("trigger-hover", &triggers.trigger_hover),
        ("trigger-unhover", &triggers.trigger_unhover),
    ] {
        if let Some(tr) = trigger_ref {
            if let Some(panel_states) = panels.get(tr.panel_id.as_str()) {
                if !panel_states.contains(tr.state_name.as_str()) {
                    diagnostic!(
                        diagnostics,
                        allocation_failed,
                        DiagnosticLevel::Error,
                        "E021",
                        "{label}: state \"{}\" not found on panel \"{}\"",
                        tr.state_name,
                        tr.panel_id
                    );
                }
            } else {
                diagnostic!(
                    diagnostics,
                    allocation_failed,
                    DiagnosticLevel::Error,
                    "E020",
                    "{label}: target panel \"{}\" not found",
                    tr.panel_id
                );
            }
        }
    }
}

// ─── [on] binding validation ────────────────────────────────

/// Validate all [on] event bindings in the document.
fn validate_on_bindings(
    elements: &[Element],
    panels: &PanelMap,
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) {
    // Collect all element IDs (animate, panel, live, link, input, button, element-def)
    let mut element_ids = HashSet::new();
    collect_element_ids(elements, &mut element_ids, allocation_failed);

    // Collect all [on] bindings
    let mut bindings = Vec::new();
    collect_on_bindings(elements, &mut bindings, allocation_failed);

    // Check limit
    if bindings.len() > MAX_ON_BINDINGS {
        diagnostic!(
            diagnostics,
            allocation_failed,
            DiagnosticLevel::Error,
            "E044",
            "maximum {} [on] bindings per page, found {}",
            MAX_ON_BINDINGS,
            bindings.len()
        );
    }

    for on_elem in &bindings {
        // Validate source reference exists (if provided)
        if let Some(ref source) = on_elem.source
            && !element_ids.contains(source.as_str())
        {
            diagnostic!(
                diagnostics,
                allocation_failed,
                DiagnosticLevel::Error,
                "E045",
                "[on] source \"{}\" not found",
                source
            );
        }

        // Validate target reference exists
        if !on_elem.target.is_empty() && !element_ids.contains(on_elem.target.as_str()) {
            diagnostic!(
                diagnostics,
                allocation_failed,
                DiagnosticLevel::Error,
                "E046",
                "[on] target \"{}\" not found",
                on_elem.target
            );
        }

        // For set action, validate target panel's state
        if on_elem.action == ActionKind::Set
            && let Some(ref to) = on_elem.to
            && let Some(panel_states) = panels.get(on_elem.target.as_str())
            && !panel_states.contains(to.as_str())
        {
            diagnostic!(
                diagnostics,
                allocation_failed,
                DiagnosticLevel::Error,
                "E021",
                "[on] set: state \"{}\" not found on panel \"{}\"",
                to,
                on_elem.target
            );
        }
    }

    // Validate cascade depth: animation-end chains
    let depth = compute_cascade_depth(&bindings, allocation_failed);
    if depth > MAX_CASCADE_DEPTH {
        diagnostic!(
            diagnostics,
            allocation_failed,
            DiagnosticLevel::Warning,
            "W010",
            "event cascade depth {} exceeds maximum of {}",
            depth,
            MAX_CASCADE_DEPTH
        );
    }
}

/// Collect all element IDs from the AST.
fn collect_element_ids<'a>(
    elements: &'a [Element],
    ids: &mut HashSet<&'a str>,
    allocation_failed: &mut bool,
) {
    for elem in elements {
        match elem {
            Element::Animate(e) => {
                if !e.id.is_empty() {
                    if reject_map_allocation() || ids.try_reserve(1).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        ids.insert(&e.id);
                    }
                }
            }
            Element::Panel(e) => {
                if !e.id.is_empty() {
                    if reject_map_allocation() || ids.try_reserve(1).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        ids.insert(&e.id);
                    }
                }
            }
            Element::Live(e) => {
                if !e.id.is_empty() {
                    if reject_map_allocation() || ids.try_reserve(1).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        ids.insert(&e.id);
                    }
                }
            }
            Element::ElementDef(e) => {
                if !e.id.is_empty() {
                    if reject_map_allocation() || ids.try_reserve(1).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        ids.insert(&e.id);
                    }
                }
            }
            Element::Link(e) => {
                if let Some(ref id) = e.id
                    && !id.is_empty()
                {
                    if reject_map_allocation() || ids.try_reserve(1).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        ids.insert(id);
                    }
                }
            }
            Element::Input(e) => {
                if let Some(ref id) = e.id
                    && !id.is_empty()
                {
                    if reject_map_allocation() || ids.try_reserve(1).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        ids.insert(id);
                    }
                }
            }
            _ => {}
        }
        collect_element_ids(elem.children(), ids, allocation_failed);
    }
}

/// Collect all [on] elements from the AST.
fn collect_on_bindings<'a>(
    elements: &'a [Element],
    bindings: &mut Vec<&'a OnElement>,
    allocation_failed: &mut bool,
) {
    for elem in elements {
        if let Element::On(on_elem) = elem {
            try_push(bindings, on_elem, allocation_failed);
        }
        collect_on_bindings(elem.children(), bindings, allocation_failed);
    }
}

/// Compute the maximum cascade depth of animation-end chains.
///
/// An animation-end event on source X that triggers animate on target Y,
/// combined with an animation-end event on source Y, forms a chain.
fn compute_cascade_depth(bindings: &[&OnElement], allocation_failed: &mut bool) -> usize {
    fn dfs(node: &str, bindings: &[&OnElement], visited: &mut Vec<usize>) -> usize {
        let mut max_depth = 0;
        for (index, binding) in bindings.iter().enumerate() {
            if visited.contains(&index)
                || binding.event != EventKind::AnimationEnd
                || binding.action != ActionKind::Animate
                || binding.source.as_deref() != Some(node)
            {
                continue;
            }
            visited.push(index);
            max_depth = max_depth.max(1 + dfs(&binding.target, bindings, visited));
            visited.pop();
        }
        max_depth
    }

    let mut visited = Vec::new();
    if visited.try_reserve_exact(bindings.len()).is_err() {
        mark_allocation_failure(allocation_failed);
        return 0;
    }
    bindings
        .iter()
        .filter_map(|binding| binding.source.as_deref())
        .map(|source| dfs(source, bindings, &mut visited))
        .max()
        .unwrap_or(0)
}

/// Helper: call a closure with the children of any element that has children.
fn for_each_child(elem: &Element, mut f: impl FnMut(&[Element])) {
    match elem {
        Element::Box(e) => f(&e.children),
        Element::Row(e) => f(&e.children),
        Element::Col(e) => f(&e.children),
        Element::Header(e)
        | Element::Body(e)
        | Element::Footer(e)
        | Element::Thead(e)
        | Element::Tbody(e)
        | Element::Pagination(e) => f(&e.children),
        Element::Nav(e) => f(&e.children),
        Element::Text(e) => f(&e.children),
        Element::Heading(e) => f(&e.children),
        Element::List(e) => f(&e.children),
        Element::Item(e) => f(&e.children),
        Element::Link(e) => f(&e.children),
        Element::Select(e) => f(&e.children),
        Element::Form(e) => f(&e.children),
        Element::Table(e) => f(&e.children),
        Element::Tr(e) => f(&e.children),
        Element::Th(e) | Element::Td(e) => f(&e.children),
        Element::Animate(e) => f(&e.children),
        Element::Frame(e) => f(&e.children),
        Element::Live(e) => f(&e.children),
        Element::Panel(e) => f(&e.children),
        Element::State(e) => f(&e.children),
        Element::Details(e) => f(&e.children),
        Element::Button(_) => {
            // Button has label (text), no child elements
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::scanner::Scanner;

    fn parse_and_validate(input: &str) -> Vec<Diagnostic> {
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let result = parser::parse(tokens);
        // Return only validation diagnostics (filter out parse-level ones)
        result
            .diagnostics
            .into_iter()
            .filter(|d| {
                matches!(
                    d.code,
                    "E020"
                        | "E021"
                        | "E022"
                        | "E044"
                        | "E045"
                        | "E046"
                        | "E048"
                        | "W010"
                        | "W011"
                        | "W012"
                        | "W013"
                )
            })
            .collect()
    }

    fn has_code(diags: &[Diagnostic], code: &str) -> bool {
        diags.iter().any(|d| d.code == code)
    }

    #[test]
    fn valid_toggle_button() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [panel id="p" state="a"]
                [state name="a"][text]A[/text][/state]
                [state name="b"][text]B[/text][/state]
            [/panel]
            [button action="toggle" target="p" states="a,b"]Toggle[/button]
        [/page]"#,
        );
        assert!(diags.is_empty(), "expected no errors: {diags:?}");
    }

    #[test]
    fn valid_set_button() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [panel id="p" state="x"]
                [state name="x"][text]X[/text][/state]
                [state name="y"][text]Y[/text][/state]
            [/panel]
            [button action="set" target="p" to="y"]Set Y[/button]
        [/page]"#,
        );
        assert!(diags.is_empty(), "expected no errors: {diags:?}");
    }

    #[test]
    fn toggle_target_panel_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [button action="toggle" target="missing" states="a,b"]X[/button]
        [/page]"#,
        );
        assert!(has_code(&diags, "E020"));
    }

    #[test]
    fn set_target_panel_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [button action="set" target="missing" to="x"]X[/button]
        [/page]"#,
        );
        assert!(has_code(&diags, "E020"));
    }

    #[test]
    fn toggle_state_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [panel id="p" state="a"]
                [state name="a"][/state]
                [state name="b"][/state]
            [/panel]
            [button action="toggle" target="p" states="a,missing"]X[/button]
        [/page]"#,
        );
        assert!(has_code(&diags, "E022"));
    }

    #[test]
    fn set_to_state_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [panel id="p" state="a"]
                [state name="a"][/state]
            [/panel]
            [button action="set" target="p" to="missing"]X[/button]
        [/page]"#,
        );
        assert!(has_code(&diags, "E021"));
    }

    #[test]
    fn trigger_focus_panel_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [input name="q" trigger-focus="missing:expanded" /]
        [/page]"#,
        );
        assert!(has_code(&diags, "E020"));
    }

    #[test]
    fn trigger_focus_state_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [panel id="search" state="collapsed"]
                [state name="collapsed"][/state]
            [/panel]
            [input name="q" trigger-focus="search:missing" /]
        [/page]"#,
        );
        assert!(has_code(&diags, "E021"));
    }

    #[test]
    fn trigger_hover_valid() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [panel id="tip" state="hidden"]
                [state name="hidden"][/state]
                [state name="visible"][text]Tooltip[/text][/state]
            [/panel]
            [button action="submit" trigger-hover="tip:visible" trigger-unhover="tip:hidden"]Go[/button]
        [/page]"#,
        );
        assert!(diags.is_empty(), "expected no errors: {diags:?}");
    }

    #[test]
    fn trigger_hover_panel_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [button action="submit" trigger-hover="missing:visible"]Go[/button]
        [/page]"#,
        );
        assert!(has_code(&diags, "E020"));
    }

    #[test]
    fn nested_panel_validation() {
        // Panel inside a box — should still be found
        let diags = parse_and_validate(
            r#"[page mode=document]
            [box]
                [panel id="inner" state="a"]
                    [state name="a"][/state]
                    [state name="b"][/state]
                [/panel]
            [/box]
            [button action="set" target="inner" to="b"]Switch[/button]
        [/page]"#,
        );
        assert!(diags.is_empty(), "expected no errors: {diags:?}");
    }

    #[test]
    fn deferred_navigation_animation_must_exist() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [link href="/next" defer="missing"][text]Go[/text][/link]
        [/page]"#,
        );
        assert!(has_code(&diags, "E048"));
    }

    #[test]
    fn deferred_navigation_accepts_animation_target() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [animate id="exit" fps=10 autoplay=false][frame][text]Bye[/text][/frame][/animate]
            [link href="/next" defer="exit"][text]Go[/text][/link]
        [/page]"#,
        );
        assert!(diags.is_empty(), "expected no errors: {diags:?}");
    }

    #[test]
    fn warns_about_transparent_fixed_panel_over_animated_background() {
        let diags = parse_and_validate(
            r#"[page mode=screen transition="dissolve"]
            [animate id="bg" background=true src="/bg.wasm" /]
            [panel id="hero" state="hidden"]
                [state name="hidden"]
                    [box y=1 w=30 h=8 border=none padding=0][/box]
                [/state]
                [state name="visible"]
                    [box y=1 w=30 h=8 bg=black][/box]
                [/state]
            [/panel]
        [/page]"#,
        );
        assert!(has_code(&diags, "W011"), "diagnostics: {diags:?}");
    }

    #[test]
    fn opaque_initial_panel_does_not_warn_about_placeholder() {
        let diags = parse_and_validate(
            r#"[page mode=screen]
            [animate id="bg" background=true src="/bg.wasm" /]
            [panel id="hero" state="ready"]
                [state name="ready"]
                    [box y=1 w=30 h=8 border=none bg=black][/box]
                [/state]
            [/panel]
        [/page]"#,
        );
        assert!(!has_code(&diags, "W011"), "diagnostics: {diags:?}");
    }

    #[test]
    fn warns_when_page_transition_overlaps_delayed_panel_entrance() {
        let diags = parse_and_validate(
            r#"[page mode=screen transition="dissolve"]
            [panel id="hero" state="hidden"]
                [state name="hidden"][/state]
                [state name="visible" transition="draw-down"]
                    [box y=1 w=30 h=8 bg=black][/box]
                [/state]
            [/panel]
            [on event="page-load" do="set" target="hero" to="visible" delay="100ms" /]
        [/page]"#,
        );
        assert!(has_code(&diags, "W012"), "diagnostics: {diags:?}");
    }

    #[test]
    fn immediate_panel_entrance_does_not_warn_about_overlap() {
        let diags = parse_and_validate(
            r#"[page mode=screen transition="dissolve"]
            [panel id="hero" state="hidden"]
                [state name="hidden"][/state]
                [state name="visible" transition="draw-down"][/state]
            [/panel]
            [on event="page-load" do="set" target="hero" to="visible" /]
        [/page]"#,
        );
        assert!(!has_code(&diags, "W012"), "diagnostics: {diags:?}");
    }

    #[test]
    fn warns_about_wasm_animation_created_by_panel_relayout() {
        let diags = parse_and_validate(
            r#"[page mode=screen]
            [panel id="hero" state="hidden"]
                [state name="hidden"][/state]
                [state name="visible"]
                    [animate id="title" src="/title.wasm"]
                        [text]NETWORK[/text]
                    [/animate]
                [/state]
            [/panel]
        [/page]"#,
        );
        assert!(has_code(&diags, "W013"), "diagnostics: {diags:?}");
    }

    #[test]
    fn wasm_animation_present_in_initial_state_does_not_warn() {
        let diags = parse_and_validate(
            r#"[page mode=screen]
            [panel id="hero" state="visible"]
                [state name="visible"]
                    [animate id="title" src="/title.wasm"]
                        [text]NETWORK[/text]
                    [/animate]
                [/state]
            [/panel]
        [/page]"#,
        );
        assert!(!has_code(&diags, "W013"), "diagnostics: {diags:?}");
    }

    // ─── [on] binding validation ──────────────────────────────

    #[test]
    fn on_valid_bindings() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [animate id="intro" fps=10][frame][text]Hi[/text][/frame][/animate]
            [panel id="preview" state="a"]
                [state name="a"][text]A[/text][/state]
                [state name="b"][text]B[/text][/state]
            [/panel]
            [on event="page-load" do="animate" target="intro" /]
            [on event="animation-end" source="intro" do="set" target="preview" to="b" /]
        [/page]"#,
        );
        assert!(diags.is_empty(), "expected no errors: {diags:?}");
    }

    #[test]
    fn on_source_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [animate id="intro" fps=10][frame][text]Hi[/text][/frame][/animate]
            [on event="animation-end" source="missing" do="animate" target="intro" /]
        [/page]"#,
        );
        assert!(has_code(&diags, "E045"));
    }

    #[test]
    fn on_target_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [on event="page-load" do="animate" target="missing" /]
        [/page]"#,
        );
        assert!(has_code(&diags, "E046"));
    }

    #[test]
    fn on_set_state_not_found() {
        let diags = parse_and_validate(
            r#"[page mode=document]
            [panel id="p" state="a"]
                [state name="a"][/state]
            [/panel]
            [on event="page-load" do="set" target="p" to="missing" /]
        [/page]"#,
        );
        assert!(has_code(&diags, "E021"));
    }
}
