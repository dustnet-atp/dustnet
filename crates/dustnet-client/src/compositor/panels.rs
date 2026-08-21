//! Focusables collection + trigger refs.
//!
//! Panel state + details state + event bindings all live on the Scene
//! now. This module is what's left: the focusables catalog that the
//! focus-cycling input handlers consume, plus the input-mode types
//! they emit actions to.

use crate::parser::ast::*;

/// A focusable element found in the document with its trigger info.
///
/// Carries the scene `NodeId` — the single authority for identifying
/// which focusable is currently focused. The viewer derives the list
/// index from `Scene.focus` by matching `node_id` here; `focus_index`
/// is no longer stored on `ViewportState`.
#[derive(Debug, Clone)]
pub struct FocusableElement {
    /// Scene node id — the canonical handle for this focusable.
    pub node_id: crate::compositor::scene::NodeId,
    /// Optional AML element ID for event binding source matching.
    pub id: Option<String>,
    /// Display label for the element.
    pub label: String,
    /// What happens when activated (Enter key).
    pub action: FocusAction,
    /// Column in the cell buffer where this element appears.
    pub col: u16,
    /// Row in the cell buffer where this element appears.
    pub row: u16,
    /// Width in columns of the focusable text.
    pub width: u16,
    /// Whether this element is in a sticky region (coordinates are in sticky_buf).
    pub is_sticky: bool,
}

impl FocusableElement {
    pub(crate) fn retained_payload_capacity(&self) -> Option<usize> {
        self.id
            .as_ref()
            .map_or(0, String::capacity)
            .checked_add(self.label.capacity())?
            .checked_add(self.action.retained_payload_capacity()?)
    }
}

#[derive(Debug, Clone)]
pub enum FocusAction {
    /// Toggle button: cycle panel through states.
    Toggle {
        panel_id: String,
        states: Vec<String>,
    },
    /// Set button: jump panel to specific state.
    Set {
        panel_id: String,
        state_name: String,
    },
    /// Navigation link.
    Navigate {
        href: String,
        transition: Option<TransitionKind>,
        transition_duration_ms: u32,
        defer_animation: Option<String>,
    },
    /// Submit form.
    Submit {
        target: String,
        form: Option<crate::compositor::scene::NodeId>,
    },
    /// Edit an input field.
    EditInput {
        name: String,
        form: Option<crate::compositor::scene::NodeId>,
        maxlen: u32,
        password: bool,
    },
    /// Cycle a select control to its next option.
    EditSelect {
        form: Option<crate::compositor::scene::NodeId>,
    },
    /// Toggle a details element open/closed.
    ToggleDetails { details_index: usize },
    /// No action (just focusable).
    None,
}

impl FocusAction {
    pub(crate) fn try_clone(&self) -> Result<Self, std::collections::TryReserveError> {
        fn owned(value: &str) -> Result<String, std::collections::TryReserveError> {
            let mut result = String::new();
            result.try_reserve_exact(value.len())?;
            result.push_str(value);
            Ok(result)
        }

        Ok(match self {
            Self::Toggle { panel_id, states } => {
                let mut cloned_states = Vec::new();
                cloned_states.try_reserve_exact(states.len())?;
                for state in states {
                    cloned_states.push(owned(state)?);
                }
                Self::Toggle {
                    panel_id: owned(panel_id)?,
                    states: cloned_states,
                }
            }
            Self::Set {
                panel_id,
                state_name,
            } => Self::Set {
                panel_id: owned(panel_id)?,
                state_name: owned(state_name)?,
            },
            Self::Navigate {
                href,
                transition,
                transition_duration_ms,
                defer_animation,
            } => Self::Navigate {
                href: owned(href)?,
                transition: *transition,
                transition_duration_ms: *transition_duration_ms,
                defer_animation: defer_animation.as_deref().map(owned).transpose()?,
            },
            Self::Submit { target, form } => Self::Submit {
                target: owned(target)?,
                form: *form,
            },
            Self::EditInput {
                name,
                form,
                maxlen,
                password,
            } => Self::EditInput {
                name: owned(name)?,
                form: *form,
                maxlen: *maxlen,
                password: *password,
            },
            Self::EditSelect { form } => Self::EditSelect { form: *form },
            Self::ToggleDetails { details_index } => Self::ToggleDetails {
                details_index: *details_index,
            },
            Self::None => Self::None,
        })
    }

    fn retained_payload_capacity(&self) -> Option<usize> {
        match self {
            Self::Toggle { panel_id, states } => {
                let total = panel_id.capacity().checked_add(
                    states
                        .capacity()
                        .checked_mul(std::mem::size_of::<String>())?,
                )?;
                states
                    .iter()
                    .try_fold(total, |sum, state| sum.checked_add(state.capacity()))
            }
            Self::Set {
                panel_id,
                state_name,
            } => panel_id.capacity().checked_add(state_name.capacity()),
            Self::Navigate {
                href,
                defer_animation,
                ..
            } => href
                .capacity()
                .checked_add(defer_animation.as_ref().map_or(0, String::capacity)),
            Self::Submit { target, .. } => Some(target.capacity()),
            Self::EditInput { name, .. } => Some(name.capacity()),
            Self::EditSelect { .. } | Self::ToggleDetails { .. } | Self::None => Some(0),
        }
    }
}

/// Return the number of visible focusables and a conservative bound for every
/// nested allocation their retained projection records will clone. This walk
/// mirrors collection but allocates nothing, so the governor can admit remote
/// payloads before construction starts.
pub(crate) fn focusable_storage_requirements(
    scene: &crate::compositor::scene::Scene,
) -> Option<(usize, usize)> {
    let root = scene.get(scene.root())?;
    focusable_requirements_for_children(scene, root.children())
}

fn checked_string_vec_capacity(values: &Vec<String>) -> Option<usize> {
    values.iter().try_fold(
        values
            .capacity()
            .checked_mul(std::mem::size_of::<String>())?,
        |total, value| total.checked_add(value.capacity()),
    )
}

fn focusable_requirements_for_children(
    scene: &crate::compositor::scene::Scene,
    children: &[crate::compositor::scene::NodeId],
) -> Option<(usize, usize)> {
    use crate::compositor::scene::{FlowSource, NodeKind};

    let mut count = 0usize;
    let mut bytes = 0usize;
    for &child_id in children {
        let Some(node) = scene.get(child_id) else {
            continue;
        };
        let payload = match node.kind() {
            NodeKind::Button(data) => {
                let action = match data.action {
                    ButtonAction::Toggle => match (&data.target, &data.states) {
                        (Some(target), Some(states)) => target
                            .capacity()
                            .checked_add(checked_string_vec_capacity(states)?)?,
                        _ => 0,
                    },
                    ButtonAction::Set => match (&data.target, &data.to) {
                        (Some(target), Some(to)) => target.capacity().checked_add(to.capacity())?,
                        _ => 0,
                    },
                    ButtonAction::Navigate => data.href.as_ref().map_or(0, String::capacity),
                    ButtonAction::Submit => data.target.as_ref().map_or(0, String::capacity),
                };
                Some(data.label.capacity().checked_add(action)?)
            }
            NodeKind::Link(data) => {
                let mut label_bytes = 0usize;
                for &descendant_id in node.children() {
                    if let Some(descendant) = scene.get(descendant_id)
                        && let NodeKind::Text(text) = descendant.kind()
                    {
                        for run in &text.runs {
                            label_bytes = label_bytes.checked_add(run.text.capacity())?;
                        }
                    }
                }
                if label_bytes == 0 {
                    label_bytes = "link".len();
                }
                Some(
                    node.aml_id_capacity()
                        .checked_add(label_bytes)?
                        .checked_add(data.href.capacity())?
                        .checked_add(data.defer_animation.as_ref().map_or(0, String::capacity))?,
                )
            }
            NodeKind::Input(data) => Some(
                node.aml_id_capacity()
                    .checked_add(
                        data.placeholder
                            .as_ref()
                            .map_or(data.name.capacity(), String::capacity),
                    )?
                    .checked_add(data.name.capacity())?,
            ),
            NodeKind::Flow(data) if matches!(data.source, FlowSource::Details) => {
                Some(data.details_summary.as_ref().map_or(0, String::capacity))
            }
            NodeKind::Select(data) => Some(
                node.aml_id_capacity().checked_add(
                    data.label
                        .as_ref()
                        .map_or(data.name.capacity(), String::capacity),
                )?,
            ),
            _ => None,
        };
        if let Some(payload) = payload {
            count = count.checked_add(1)?;
            bytes = bytes.checked_add(payload)?;
        }

        let recurse = match node.kind() {
            NodeKind::Panel { active, .. } => {
                scene.get(*active).map(|active_node| active_node.children())
            }
            NodeKind::Flow(data) if matches!(data.source, FlowSource::Details) => {
                let summary_count =
                    (data.details_summary_count as usize).min(node.children().len());
                let (summary, body) = node.children().split_at(summary_count);
                let (child_count, child_bytes) =
                    focusable_requirements_for_children(scene, summary)?;
                count = count.checked_add(child_count)?;
                bytes = bytes.checked_add(child_bytes)?;
                if data.details_open {
                    let (child_count, child_bytes) =
                        focusable_requirements_for_children(scene, body)?;
                    count = count.checked_add(child_count)?;
                    bytes = bytes.checked_add(child_bytes)?;
                }
                None
            }
            NodeKind::Button(_) | NodeKind::Link(_) | NodeKind::Input(_) | NodeKind::Select(_) => {
                None
            }
            NodeKind::Text(_) if node.children().is_empty() => None,
            NodeKind::Hr(_) | NodeKind::Spacer { .. } => None,
            _ => Some(node.children()),
        };
        if let Some(descendants) = recurse {
            let (child_count, child_bytes) =
                focusable_requirements_for_children(scene, descendants)?;
            count = count.checked_add(child_count)?;
            bytes = bytes.checked_add(child_bytes)?;
        }
    }
    Some((count, bytes))
}

/// Collect focusable elements from the scene in tree order. The scene
/// is authoritative for panel active-state, details open/closed, and
/// the on-screen rect of each focusable (via `focusable_screen_rect`).
/// For Panel nodes this walks only the active state's subtree.
pub fn collect_focusables_from_scene(
    scene: &crate::compositor::scene::Scene,
) -> Vec<FocusableElement> {
    let mut focusables = Vec::new();
    collect_focusables_from_scene_into(scene, &mut focusables);
    focusables
}

/// Fill caller-preallocated storage without performing a hidden collection
/// allocation. Governed layout uses this after reserving worst-case node
/// capacity before it mutates the active scene.
pub(crate) fn collect_focusables_from_scene_into(
    scene: &crate::compositor::scene::Scene,
    focusables: &mut Vec<FocusableElement>,
) {
    focusables.clear();
    let mut details_counter = 0usize;
    let root = scene.root();
    if let Some(root_node) = scene.get(root) {
        collect_from_scene_children(
            scene,
            root_node.children(),
            focusables,
            None,
            &mut details_counter,
        );
    }
}

fn rect_of(
    scene: &crate::compositor::scene::Scene,
    id: crate::compositor::scene::NodeId,
) -> (u16, u16, u16) {
    scene
        .get(id)
        .and_then(|n| n.focusable_screen_rect())
        .map(|r| (r.x, r.y, r.w))
        .unwrap_or((0, 0, 0))
}

fn collect_from_scene_children(
    scene: &crate::compositor::scene::Scene,
    children: &[crate::compositor::scene::NodeId],
    focusables: &mut Vec<FocusableElement>,
    form_id: Option<crate::compositor::scene::NodeId>,
    details_counter: &mut usize,
) {
    use crate::compositor::scene::{FlowSource, NodeKind};
    for &child_id in children {
        let Some(node) = scene.get(child_id) else {
            continue;
        };
        match node.kind() {
            NodeKind::Button(data) => {
                let action = match data.action {
                    ButtonAction::Toggle => match (&data.target, &data.states) {
                        (Some(target), Some(states)) => FocusAction::Toggle {
                            panel_id: target.clone(),
                            states: states.clone(),
                        },
                        _ => FocusAction::None,
                    },
                    ButtonAction::Set => match (&data.target, &data.to) {
                        (Some(target), Some(to)) => FocusAction::Set {
                            panel_id: target.clone(),
                            state_name: to.clone(),
                        },
                        _ => FocusAction::None,
                    },
                    ButtonAction::Navigate => FocusAction::Navigate {
                        href: data.href.clone().unwrap_or_default(),
                        transition: data.transition,
                        transition_duration_ms: data.transition_duration_ms,
                        defer_animation: None,
                    },
                    ButtonAction::Submit => FocusAction::Submit {
                        target: data.target.clone().unwrap_or_default(),
                        form: form_id,
                    },
                };
                let (col, row, width) = rect_of(scene, child_id);
                focusables.push(FocusableElement {
                    node_id: child_id,
                    id: None,
                    label: data.label.clone(),
                    action,
                    col,
                    row,
                    width,
                    is_sticky: false,
                });
            }
            NodeKind::Link(data) => {
                let label = scene_link_label(scene, node.children());
                let (col, row, width) = rect_of(scene, child_id);
                focusables.push(FocusableElement {
                    node_id: child_id,
                    id: node.aml_id().map(|s| s.to_string()),
                    label,
                    action: FocusAction::Navigate {
                        href: data.href.clone(),
                        transition: data.transition,
                        transition_duration_ms: data.transition_duration_ms,
                        defer_animation: data.defer_animation.clone(),
                    },
                    col,
                    row,
                    width,
                    is_sticky: false,
                });
            }
            NodeKind::Input(data) => {
                let (col, row, width) = rect_of(scene, child_id);
                focusables.push(FocusableElement {
                    node_id: child_id,
                    id: node.aml_id().map(|s| s.to_string()),
                    label: data
                        .placeholder
                        .clone()
                        .unwrap_or_else(|| data.name.clone()),
                    action: FocusAction::EditInput {
                        name: data.name.clone(),
                        form: form_id,
                        maxlen: data.maxlen,
                        password: data.password,
                    },
                    col,
                    row,
                    width,
                    is_sticky: false,
                });
            }
            NodeKind::Panel { active, .. } => {
                if let Some(active_node) = scene.get(*active) {
                    collect_from_scene_children(
                        scene,
                        active_node.children(),
                        focusables,
                        form_id,
                        details_counter,
                    );
                }
            }
            NodeKind::Flow(data) if matches!(data.source, FlowSource::Details) => {
                let idx = *details_counter;
                *details_counter += 1;
                let summary_count = data.details_summary_count as usize;
                let summary_label = data.details_summary.clone().unwrap_or_default();
                let (col, row, width) = rect_of(scene, child_id);
                focusables.push(FocusableElement {
                    node_id: child_id,
                    id: None,
                    label: summary_label,
                    action: FocusAction::ToggleDetails { details_index: idx },
                    col,
                    row,
                    width,
                    is_sticky: false,
                });
                let kids = node.children();
                let (summary_ids, body_ids) = kids.split_at(summary_count.min(kids.len()));
                collect_from_scene_children(
                    scene,
                    summary_ids,
                    focusables,
                    form_id,
                    details_counter,
                );
                if data.details_open {
                    collect_from_scene_children(
                        scene,
                        body_ids,
                        focusables,
                        form_id,
                        details_counter,
                    );
                }
            }
            NodeKind::Flow(data) if matches!(data.source, FlowSource::Form) => {
                collect_from_scene_children(
                    scene,
                    node.children(),
                    focusables,
                    Some(child_id),
                    details_counter,
                );
            }
            NodeKind::Select(data) => {
                let (col, row, width) = rect_of(scene, child_id);
                focusables.push(FocusableElement {
                    node_id: child_id,
                    id: node.aml_id().map(str::to_string),
                    label: data.label.clone().unwrap_or_else(|| data.name.clone()),
                    action: FocusAction::EditSelect { form: form_id },
                    col,
                    row,
                    width,
                    is_sticky: false,
                });
            }
            NodeKind::Text(_) => {
                if !node.children().is_empty() {
                    collect_from_scene_children(
                        scene,
                        node.children(),
                        focusables,
                        form_id,
                        details_counter,
                    );
                }
            }
            NodeKind::Hr(_) | NodeKind::Spacer { .. } => {}
            _ => {
                collect_from_scene_children(
                    scene,
                    node.children(),
                    focusables,
                    form_id,
                    details_counter,
                );
            }
        }
    }
}

fn scene_link_label(
    scene: &crate::compositor::scene::Scene,
    children: &[crate::compositor::scene::NodeId],
) -> String {
    use crate::compositor::scene::NodeKind;
    let mut label = String::new();
    for &child_id in children {
        let Some(child) = scene.get(child_id) else {
            continue;
        };
        if let NodeKind::Text(tc) = child.kind() {
            for run in &tc.runs {
                label.push_str(run.text.trim());
            }
        }
    }
    if label.is_empty() {
        "link".into()
    } else {
        label
    }
}

/// Collect initial values from `[input]` nodes in the scene.
/// Returns (name, value) pairs for inputs whose `value` is non-empty.
pub fn collect_input_values(scene: &crate::compositor::scene::Scene) -> Vec<(String, String)> {
    use crate::compositor::scene::NodeKind;
    let mut out = Vec::new();
    for n in scene.iter_tree_order() {
        if let NodeKind::Input(data) = n.kind()
            && let Some(v) = data.value.as_deref()
            && !v.is_empty()
        {
            out.push((data.name.clone(), v.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::scanner::Scanner;

    fn parse_doc(input: &str) -> Document {
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let result = parser::parse(tokens);
        result.document.unwrap()
    }

    #[test]
    fn fallible_focus_action_clone_preserves_nested_payloads() {
        let action = FocusAction::Toggle {
            panel_id: "panel".into(),
            states: vec!["first".into(), "second".into()],
        };
        let cloned = action.try_clone().unwrap();
        assert!(matches!(
            cloned,
            FocusAction::Toggle { panel_id, states }
                if panel_id == "panel" && states == ["first", "second"]
        ));

        let navigation = FocusAction::Navigate {
            href: "/next".into(),
            transition: Some(TransitionKind::Fade),
            transition_duration_ms: 250,
            defer_animation: Some("exit".into()),
        };
        assert!(matches!(
            navigation.try_clone().unwrap(),
            FocusAction::Navigate {
                href,
                defer_animation: Some(wait_for),
                ..
            } if href == "/next" && wait_for == "exit"
        ));
    }

    #[test]
    fn details_layout_collapsed() {
        let doc = parse_doc(
            r#"[page mode=document]
            [details summary="Hidden section"]
                [text]This should not appear[/text]
            [/details]
        [/page]"#,
        );
        use crate::color::ColorSupport;
        use crate::compositor::layout::engine::layout_scene;
        use crate::compositor::layout::text::WidthConfig;
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let page_buf = layout_scene(
            &mut scene,
            60,
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
            plain.contains("\u{25B6}"),
            "should show collapsed indicator ▶"
        );
        assert!(plain.contains("Hidden section"), "should show summary");
        assert!(
            !plain.contains("This should not appear"),
            "children should be hidden"
        );
    }

    #[test]
    fn details_layout_expanded() {
        let doc = parse_doc(
            r#"[page mode=document]
            [details summary="Open section" open]
                [text]Visible content[/text]
            [/details]
        [/page]"#,
        );
        use crate::color::ColorSupport;
        use crate::compositor::layout::engine::layout_scene;
        use crate::compositor::layout::text::WidthConfig;
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let page_buf = layout_scene(
            &mut scene,
            60,
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
            plain.contains("\u{25BC}"),
            "should show expanded indicator ▼"
        );
        assert!(plain.contains("Open section"), "should show summary");
        assert!(
            plain.contains("Visible content"),
            "children should be visible"
        );
    }
}
