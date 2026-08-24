//! `AnimationRuntime`: the tick-driver for the animate subsystem.
//!
//! Holds every animation on a page as `Box<dyn Animation>`, plus
//! `TransitionAdapter` instances for panel state transitions.
//!
//! ## State model
//!
//! `AnimState` is the lifecycle signal every adapter publishes via
//! `Animation::state()`. The adapter owns its own state machine (because
//! per-kind rules differ — WASM has fuel exhaustion, frame has
//! loop-count retirement, etc.); the runtime just reads the state to
//! resolve chain dependencies and fire `animation-end` events.

use std::time::Instant;

use super::transition::TransitionAdapter;
use super::{
    AdvanceCtx, Animation, AnimationResizeCandidate, FrameAnimationAdapter, WasmAnimationAdapter,
};

pub(crate) trait PreparedWasmSource {
    fn get_prepared_wasm(&self, path: &str) -> Option<&[u8]>;
}

impl PreparedWasmSource for std::collections::HashMap<String, std::sync::Arc<[u8]>> {
    fn get_prepared_wasm(&self, path: &str) -> Option<&[u8]> {
        self.get(path).map(AsRef::as_ref)
    }
}
use crate::color::ColorSupport;
use crate::compositor::layout::text::WidthConfig;
use crate::compositor::scene::Scene;
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

/// Allocate a remotely sized vector only after its requested capacity has
/// been admitted. Reconcile allocator rounding before any remote values are
/// inserted, and retain the returned lease for as long as the vector lives.
fn try_governed_vec<T>(
    governor: &ResourceGovernor,
    capacity: usize,
) -> Option<(Vec<T>, BudgetLease)> {
    let requested_bytes = capacity.checked_mul(std::mem::size_of::<T>())?;
    let mut lease = governor
        .reserve(ResourceCategory::RemoteCollections, requested_bytes)
        .ok()?;
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).ok()?;
    let retained_bytes = values.capacity().checked_mul(std::mem::size_of::<T>())?;
    lease
        .try_resize_with_cost(retained_bytes, retained_bytes)
        .ok()?;
    Some((values, lease))
}

/// Bytes a single page-build notice may hold beyond the effect id it names.
const BUILD_NOTICE_TEXT_LIMIT: usize = 160;

/// Admit storage for the page's build notices: the vector and, unlike
/// `try_governed_vec`, the notice text that will be written into it. The
/// caller shrinks the lease to what was actually kept.
fn try_build_notice_storage(
    governor: &ResourceGovernor,
    capacity: usize,
    payload_bound: usize,
) -> Option<(Vec<String>, BudgetLease)> {
    let requested = capacity
        .checked_mul(std::mem::size_of::<String>())?
        .checked_add(payload_bound)?;
    let mut lease = governor
        .reserve(ResourceCategory::RemoteCollections, requested)
        .ok()?;
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).ok()?;
    let admitted = values
        .capacity()
        .checked_mul(std::mem::size_of::<String>())?
        .checked_add(payload_bound)?;
    lease.try_resize_with_cost(admitted, admitted).ok()?;
    Some((values, lease))
}

/// Record a page-build failure for the HUD without growing the pre-admitted
/// vector: a notice that does not fit its reservation is dropped rather than
/// allocated off-budget. Stderr is not an option here — the client owns the
/// terminal, and a line written behind its back lands on top of the page.
fn push_build_notice(notices: &mut Vec<String>, id_len: usize, args: std::fmt::Arguments<'_>) {
    if notices.len() == notices.capacity() {
        return;
    }
    if let Some(notice) =
        super::wasm::try_format_notice(id_len.saturating_add(BUILD_NOTICE_TEXT_LIMIT), args)
    {
        notices.push(notice);
    }
}

/// The subset of scene animation metadata needed while constructing runtime
/// adapters. Keeping this purpose-built avoids cloning `AnimationData::frames`:
/// frame children are discovered directly from the scene below.
struct AnimationNodeSnapshot {
    node: crate::compositor::scene::NodeId,
    fps: u8,
    autoplay: bool,
    loop_behavior: crate::parser::ast::LoopBehavior,
    background: bool,
    src: Option<String>,
    delay_ms: u32,
    after: Option<String>,
    id: String,
    region: crate::compositor::layout::Rect,
}

fn try_remote_string(value: &str) -> Option<String> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).ok()?;
    copy.push_str(value);
    Some(copy)
}

fn snapshot_string_capacity(snapshot: &AnimationNodeSnapshot) -> Option<usize> {
    snapshot
        .id
        .capacity()
        .checked_add(snapshot.src.as_ref().map_or(0, String::capacity))?
        .checked_add(snapshot.after.as_ref().map_or(0, String::capacity))
}

/// Pre-admit both the snapshot vector and every remotely supplied string before
/// copying scene data. The returned lease is later shrunk to the `id`/`after`
/// strings moved into successfully installed adapters.
fn try_animation_node_snapshots(
    scene: &Scene,
    animation_node_count: usize,
    governor: &ResourceGovernor,
) -> Option<(Vec<AnimationNodeSnapshot>, BudgetLease)> {
    use crate::compositor::scene::NodeKind;

    let payload_bound = scene
        .iter_tree_order()
        .filter_map(|node| match node.kind() {
            NodeKind::Animation(data) if scene.is_in_active_panel_state(node.id()) => {
                Some((node, data))
            }
            _ => None,
        })
        .take(animation_node_count)
        .try_fold(0usize, |total, (node, data)| {
            total
                .checked_add(node.aml_id().unwrap_or("").len())?
                .checked_add(data.src.as_ref().map_or(0, String::len))?
                .checked_add(data.after.as_ref().map_or(0, String::len))
        })?;
    let requested_structural =
        animation_node_count.checked_mul(std::mem::size_of::<AnimationNodeSnapshot>())?;
    let requested = requested_structural.checked_add(payload_bound)?;
    let mut lease = governor
        .reserve(ResourceCategory::RemoteCollections, requested)
        .ok()?;

    let mut snapshots = Vec::new();
    snapshots.try_reserve_exact(animation_node_count).ok()?;
    let admitted = snapshots
        .capacity()
        .checked_mul(std::mem::size_of::<AnimationNodeSnapshot>())?
        .checked_add(payload_bound)?;
    lease.try_resize_with_cost(admitted, admitted).ok()?;

    for node in scene.iter_tree_order() {
        let crate::compositor::scene::NodeKind::Animation(data) = node.kind() else {
            continue;
        };
        if !scene.is_in_active_panel_state(node.id()) {
            continue;
        }
        let src = match data.src.as_deref() {
            Some(value) => Some(try_remote_string(value)?),
            None => None,
        };
        let after = match data.after.as_deref() {
            Some(value) => Some(try_remote_string(value)?),
            None => None,
        };
        snapshots.push(AnimationNodeSnapshot {
            node: node.id(),
            fps: data.fps as u8,
            autoplay: data.autoplay,
            loop_behavior: data.loop_behavior,
            background: data.background,
            src,
            delay_ms: data.delay_ms,
            after,
            id: try_remote_string(node.aml_id().unwrap_or(""))?,
            region: node.placement().rect,
        });
        if snapshots.len() == animation_node_count {
            break;
        }
    }

    let retained = snapshots
        .capacity()
        .checked_mul(std::mem::size_of::<AnimationNodeSnapshot>())?
        .checked_add(snapshots.iter().try_fold(0usize, |total, snapshot| {
            total.checked_add(snapshot_string_capacity(snapshot)?)
        })?)?;
    lease.try_resize_with_cost(retained, retained).ok()?;
    Some((snapshots, lease))
}

struct OutputStorage {
    newly_finished: Vec<String>,
    wrote_buffers: Vec<crate::compositor::scene::NodeId>,
    patches: Vec<crate::compositor::scene::Patch>,
    notices: Vec<String>,
    lease: Option<BudgetLease>,
}

fn output_structural_bytes(storage: &OutputStorage) -> Option<usize> {
    storage
        .newly_finished
        .capacity()
        .checked_mul(std::mem::size_of::<String>())?
        .checked_add(
            storage
                .wrote_buffers
                .capacity()
                .checked_mul(std::mem::size_of::<crate::compositor::scene::NodeId>())?,
        )?
        .checked_add(
            storage
                .patches
                .capacity()
                .checked_mul(std::mem::size_of::<crate::compositor::scene::Patch>())?,
        )?
        .checked_add(
            storage
                .notices
                .capacity()
                .checked_mul(std::mem::size_of::<String>())?,
        )
}

fn try_output_storage(
    governor: Option<&ResourceGovernor>,
    newly_finished_capacity: usize,
    wrote_buffer_capacity: usize,
    patch_capacity: usize,
    notice_capacity: usize,
    payload_bound: usize,
) -> Option<OutputStorage> {
    let requested_structural = newly_finished_capacity
        .checked_mul(std::mem::size_of::<String>())?
        .checked_add(
            wrote_buffer_capacity
                .checked_mul(std::mem::size_of::<crate::compositor::scene::NodeId>())?,
        )?
        .checked_add(
            patch_capacity.checked_mul(std::mem::size_of::<crate::compositor::scene::Patch>())?,
        )?
        .checked_add(notice_capacity.checked_mul(std::mem::size_of::<String>())?)?;
    let requested = requested_structural.checked_add(payload_bound)?;
    let mut lease = match (governor, requested) {
        (_, 0) | (None, _) => None,
        (Some(governor), bytes) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, bytes)
                .ok()?,
        ),
    };
    let mut storage = OutputStorage {
        newly_finished: Vec::new(),
        wrote_buffers: Vec::new(),
        patches: Vec::new(),
        notices: Vec::new(),
        lease: None,
    };
    storage
        .newly_finished
        .try_reserve_exact(newly_finished_capacity)
        .ok()?;
    storage
        .wrote_buffers
        .try_reserve_exact(wrote_buffer_capacity)
        .ok()?;
    storage.patches.try_reserve_exact(patch_capacity).ok()?;
    storage.notices.try_reserve_exact(notice_capacity).ok()?;
    let admitted = output_structural_bytes(&storage)?.checked_add(payload_bound)?;
    if let Some(lease) = lease.as_mut() {
        lease.try_resize_with_cost(admitted, admitted).ok()?;
    }
    storage.lease = lease;
    Some(storage)
}

fn reconcile_output_storage(storage: &mut OutputStorage) -> Option<()> {
    let payload = storage
        .newly_finished
        .iter()
        .chain(&storage.notices)
        .try_fold(0usize, |total, value| total.checked_add(value.capacity()))?;
    let retained = output_structural_bytes(storage)?.checked_add(payload)?;
    if let Some(lease) = storage.lease.as_mut() {
        lease.try_resize_with_cost(retained, retained).ok()?;
    }
    Some(())
}

fn try_finished_snapshot(
    animations: &[Box<dyn Animation>],
    governor: Option<&ResourceGovernor>,
) -> Option<(Vec<String>, Option<BudgetLease>)> {
    let payload_bound = animations.iter().try_fold(0usize, |total, animation| {
        total.checked_add(animation.id().len())
    })?;
    let structural_bound = animations
        .len()
        .checked_mul(std::mem::size_of::<String>())?;
    let requested = structural_bound.checked_add(payload_bound)?;
    let mut lease = match (governor, requested) {
        (_, 0) | (None, _) => None,
        (Some(governor), bytes) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, bytes)
                .ok()?,
        ),
    };
    let mut finished = Vec::new();
    finished.try_reserve_exact(animations.len()).ok()?;
    let admitted = finished
        .capacity()
        .checked_mul(std::mem::size_of::<String>())?
        .checked_add(payload_bound)?;
    if let Some(lease) = lease.as_mut() {
        lease.try_resize_with_cost(admitted, admitted).ok()?;
    }
    for animation in animations
        .iter()
        .filter(|animation| animation.state() == AnimState::Finished)
    {
        finished.push(try_remote_string(animation.id())?);
    }
    let retained = finished
        .capacity()
        .checked_mul(std::mem::size_of::<String>())?
        .checked_add(
            finished
                .iter()
                .try_fold(0usize, |total, id| total.checked_add(id.capacity()))?,
        )?;
    if let Some(lease) = lease.as_mut() {
        lease.try_resize_with_cost(retained, retained).ok()?;
    }
    Some((finished, lease))
}

/// Collect `root` plus every descendant in tree order. Used by
/// `from_scene` to snapshot frame-subtree placements around the
/// `layout_subtree` call that builds each frame's bitmap.
fn collect_descendants(
    scene: &Scene,
    root: crate::compositor::scene::NodeId,
    governor: &ResourceGovernor,
) -> Option<(
    Vec<crate::compositor::scene::NodeId>,
    BudgetLease,
    BudgetLease,
)> {
    // The whole scene is a conservative bound for both traversal vectors.
    // Admission happens before either allocation, so a failed reservation or
    // allocator request leaves the scene untouched.
    let scene_nodes = scene.iter_tree_order().count();
    let (mut out, out_lease) = try_governed_vec(governor, scene_nodes)?;
    let (mut stack, stack_lease) = try_governed_vec(governor, scene_nodes)?;
    stack.push(root);
    while let Some(id) = stack.pop() {
        out.push(id);
        if let Some(n) = scene.get(id) {
            for &c in n.children() {
                stack.push(c);
            }
        }
    }
    Some((out, out_lease, stack_lease))
}

/// Lifecycle state every animation adapter reports via
/// `Animation::state()`. Consumed by the runtime to resolve
/// `after=` chain dependencies and by the trigger dispatcher to
/// fire `animation-end` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimState {
    /// Delay or `after` dependency not yet satisfied.
    Waiting,
    /// Currently playing.
    Running,
    /// Paused (scrolled out of viewport).
    Paused,
    /// Non-looping animation completed, or explicit Stop.
    Finished,
}

/// The runtime. Owns every animation on the page. Orchestrates ticking,
/// chain resolution, viewport visibility, event firing.
pub struct AnimationRuntime {
    pub animations: Vec<Box<dyn Animation>>,
    /// Panel state transitions as `TransitionAdapter`s — tracked
    /// separately from `animations` so `has_transitions()` returns a
    /// precise answer (transitions affect poll rate and compositor
    /// layer routing differently than regular animations).
    pub transition_animations: Vec<TransitionAdapter>,
    pub start_time: Instant,
    /// Retained-capacity accounting for `animations`. The vector reserves one
    /// extra slot for the page-transition adapter installed after page load.
    _animation_collection_lease: Option<BudgetLease>,
    /// Exact retained capacity of scene-authored `id` and `after` strings
    /// moved into installed frame/WASM adapters during construction.
    _animation_payload_lease: Option<BudgetLease>,
    /// Exact retained-capacity accounting for simultaneous panel transitions.
    _transition_collection_lease: Option<BudgetLease>,
    /// Shared budget for fallible per-tick and skip output collections.
    output_governor: Option<ResourceGovernor>,
    /// Failures raised while building the page that the page's author needs
    /// to see: an effect the frame budget could not admit, or one whose
    /// module would not load. Drained into the first `tick`, which is what
    /// carries them to the HUD Errors tab.
    build_notices: Vec<String>,
    /// Retained-capacity accounting for `build_notices`; released when they
    /// are drained.
    _build_notice_lease: Option<BudgetLease>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnimationPreparationRejected;

pub(crate) struct PreparedAnimationResize {
    candidates: Vec<(usize, AnimationResizeCandidate)>,
    _collection_lease: Option<BudgetLease>,
}

impl AnimationRuntime {
    pub fn empty() -> Self {
        let now = Instant::now();
        Self {
            animations: Vec::new(),
            transition_animations: Vec::new(),
            start_time: now,
            _animation_collection_lease: None,
            _animation_payload_lease: None,
            _transition_collection_lease: None,
            output_governor: None,
            build_notices: Vec::new(),
            _build_notice_lease: None,
        }
    }

    pub fn new(animations: Vec<Box<dyn Animation>>) -> Self {
        let now = Instant::now();
        Self {
            animations,
            transition_animations: Vec::new(),
            start_time: now,
            _animation_collection_lease: None,
            _animation_payload_lease: None,
            _transition_collection_lease: None,
            output_governor: None,
            build_notices: Vec::new(),
            _build_notice_lease: None,
        }
    }

    /// Build the runtime from a scene. Walks for `NodeKind::Animation`
    /// nodes, reads each node's `AnimationData` + its post-layout
    /// `placement.rect`, loads the WASM module (or renders frame
    /// subtrees via `layout_subtree`), and constructs the appropriate
    /// adapter directly.
    ///
    /// The scene must have been hydrated with per-node placements
    /// (`hydrate_scene_buffers`) before calling this — the rect is
    /// read from `node.placement().rect`, not from a parallel
    /// `PlacedElement` list.
    pub async fn from_scene(
        scene: &mut Scene,
        color_support: ColorSupport,
        wcfg: WidthConfig,
        wasm_dir: Option<&std::path::Path>,
    ) -> Self {
        Self::from_scene_with_sources(scene, color_support, wcfg, None, wasm_dir, None)
            .await
            .unwrap_or_else(|_| Self::empty())
    }

    pub(crate) async fn from_scene_with_prepared_wasm(
        scene: &mut Scene,
        color_support: ColorSupport,
        wcfg: WidthConfig,
        governor: &ResourceGovernor,
        prepared_wasm: &dyn PreparedWasmSource,
    ) -> Result<Self, AnimationPreparationRejected> {
        Self::from_scene_with_sources(
            scene,
            color_support,
            wcfg,
            Some(governor),
            None,
            Some(prepared_wasm),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn from_scene_with_sources(
        scene: &mut Scene,
        color_support: ColorSupport,
        wcfg: WidthConfig,
        supplied_governor: Option<&ResourceGovernor>,
        wasm_dir: Option<&std::path::Path>,
        prepared_wasm: Option<&dyn PreparedWasmSource>,
    ) -> Result<Self, AnimationPreparationRejected> {
        use crate::compositor::scene::NodeKind;
        use crate::compositor::wasm::WasmRuntime;

        let now = Instant::now();

        let governor = scene
            .resource_governor()
            .or_else(|| supplied_governor.cloned())
            .unwrap_or_default();
        let animation_node_count = scene
            .iter_tree_order()
            .filter(|node| {
                matches!(node.kind(), NodeKind::Animation(_))
                    && scene.is_in_active_panel_state(node.id())
            })
            .take(crate::parser::MAX_ANIMATE_REGIONS)
            .count();
        let panel_node_count = scene
            .iter_tree_order()
            .filter(|node| matches!(node.kind(), NodeKind::Panel { .. }))
            .count();
        // Runtime construction may subsequently install one page-transition
        // adapter. Reserving that slot here prevents a remote navigation from
        // growing this retained collection through an infallible `push`.
        let Some((mut animations, animation_collection_lease)) =
            try_governed_vec::<Box<dyn Animation>>(
                &governor,
                animation_node_count.saturating_add(1),
            )
        else {
            scene.record_resource_error();
            return Err(AnimationPreparationRejected);
        };
        let transition_collection =
            if super::reject_animate_allocation(super::AnimateAllocationSite::TransitionCollection)
            {
                None
            } else {
                try_governed_vec::<TransitionAdapter>(&governor, panel_node_count)
            };
        let Some((transition_animations, transition_collection_lease)) = transition_collection
        else {
            scene.record_resource_error();
            return Err(AnimationPreparationRejected);
        };

        // Collect only the runtime metadata needed after the immutable scene
        // walk. Snapshot capacity and nested remote strings are admitted
        // together; unused `AnimationData::frames` are not cloned.
        let payload = if super::reject_animate_allocation(super::AnimateAllocationSite::Payload) {
            None
        } else {
            try_animation_node_snapshots(scene, animation_node_count, &governor)
        };
        let Some((animation_nodes, mut animation_payload_lease)) = payload else {
            scene.record_resource_error();
            return Err(AnimationPreparationRejected);
        };

        // Notices raised below reach the user through the HUD, so their
        // storage is admitted alongside the adapters. Failing to admit it
        // costs the notices, not the page: every push is capacity-guarded.
        let build_notice_payload_bound = animation_nodes.iter().try_fold(0usize, |total, node| {
            total.checked_add(node.id.capacity().saturating_add(BUILD_NOTICE_TEXT_LIMIT))
        });
        let (mut build_notices, build_notice_lease) = match build_notice_payload_bound
            .filter(|_| {
                !super::reject_animate_allocation(super::AnimateAllocationSite::BuildNotices)
            })
            .and_then(|bound| try_build_notice_storage(&governor, animation_node_count, bound))
        {
            Some((values, lease)) => (values, Some(lease)),
            None => (Vec::new(), None),
        };

        let wasm_runtime = WasmRuntime::with_governor(governor.clone());
        let mut remaining_frames = crate::parser::MAX_ANIMATION_FRAMES;
        let mut remaining_wasm_instances = crate::parser::MAX_WASM_INSTANCES;
        let mut retained_animation_payload = 0usize;

        for snapshot in animation_nodes {
            let AnimationNodeSnapshot {
                node,
                fps,
                autoplay,
                loop_behavior,
                background,
                src,
                delay_ms,
                after,
                id: anim_id,
                region,
            } = snapshot;
            if region.h == 0 {
                continue;
            }

            // WASM-driven animation.
            if let Some(src_path) = src.as_deref() {
                if remaining_wasm_instances == 0 {
                    continue;
                }
                remaining_wasm_instances -= 1;
                // Pre-render content from the scene: any non-Frame
                // children of the Animation node become the content
                // buffer that WASM effects can read (typewriter, etc.).
                // Collect the non-frame child ids under an immutable
                // borrow, then release it before running the mutating
                // layout pass.
                let child_count = scene.get(node).map_or(0, |n| n.children().len());
                let Some((mut non_frame, _non_frame_lease)) =
                    try_governed_vec::<crate::compositor::scene::NodeId>(&governor, child_count)
                else {
                    scene.record_resource_error();
                    return Err(AnimationPreparationRejected);
                };
                if let Some(animation_node) = scene.get(node) {
                    for &cid in animation_node.children() {
                        if !matches!(
                            scene.get(cid).map(|c| c.kind()),
                            Some(crate::compositor::scene::NodeKind::Flow(f)) if matches!(
                                f.source,
                                crate::compositor::scene::FlowSource::Frame
                            )
                        ) {
                            non_frame.push(cid);
                        }
                    }
                }
                let content_buf = if non_frame.is_empty() {
                    None
                } else {
                    // Content-transform effects need the composed pixels of
                    // their children, not the now-empty shared layout buffer.
                    // `layout_subtree` performs the same per-node mini-
                    // composite used for frame animations. Preserve the
                    // screen-space placements it temporarily rewrites while
                    // rendering the subtree at animation-local (0, 0).
                    let Some((descendants, _descendants_lease, _stack_lease)) =
                        collect_descendants(scene, node, &governor)
                    else {
                        scene.record_resource_error();
                        return Err(AnimationPreparationRejected);
                    };
                    let Some((mut snapshot, _snapshot_lease)) =
                        try_governed_vec::<(
                            crate::compositor::scene::NodeId,
                            crate::compositor::layout::engine::Placement,
                        )>(&governor, descendants.len())
                    else {
                        scene.record_resource_error();
                        return Err(AnimationPreparationRejected);
                    };
                    for &id in &descendants {
                        if let Some(node) = scene.get(id) {
                            snapshot.push((id, *node.placement()));
                        }
                    }
                    let buf = crate::compositor::layout::engine::layout_subtree_governed(
                        scene,
                        node,
                        region.w,
                        region.h.max(1),
                        color_support,
                        wcfg,
                        &governor,
                    );
                    if buf.allocation_failed() || scene.resource_limit_exceeded() {
                        scene.record_resource_error();
                        return Err(AnimationPreparationRejected);
                    }
                    for (id, placement) in snapshot {
                        scene.update_placement(id, placement);
                    }
                    Some(buf)
                };

                let result = if let Some(bytes) =
                    prepared_wasm.and_then(|all| all.get_prepared_wasm(src_path))
                {
                    Some(crate::compositor::animate::wasm::load_wasm_bytes(
                        region,
                        bytes,
                        &wasm_runtime,
                        content_buf,
                    ))
                } else {
                    wasm_dir.map(|dir| {
                        crate::compositor::animate::wasm::load_wasm_from_file(
                            region,
                            src_path,
                            dir,
                            &wasm_runtime,
                            content_buf,
                        )
                    })
                };
                if let Some(result) = result {
                    match result {
                        Ok((instance, frame_count)) => {
                            let authored_frames = frame_count as usize;
                            if authored_frames > remaining_frames {
                                push_build_notice(
                                    &mut build_notices,
                                    anim_id.len(),
                                    format_args!(
                                        "effect '{anim_id}' needs {authored_frames} frames; the page has {remaining_frames} of its {} left",
                                        crate::parser::MAX_ANIMATION_FRAMES
                                    ),
                                );
                                continue;
                            }
                            remaining_frames -= authored_frames;
                            let retained_payload = anim_id
                                .capacity()
                                .saturating_add(after.as_ref().map_or(0, String::capacity));
                            let Ok(adapter) = WasmAnimationAdapter::try_new(
                                anim_id,
                                node,
                                instance,
                                fps,
                                loop_behavior,
                                delay_ms,
                                after,
                                autoplay,
                                background,
                                frame_count,
                                region.w,
                                region.h,
                            ) else {
                                scene.record_resource_error();
                                return Err(AnimationPreparationRejected);
                            };
                            retained_animation_payload =
                                retained_animation_payload.saturating_add(retained_payload);
                            animations.push(Box::new(adapter));
                        }
                        Err(e) => {
                            if matches!(e, crate::compositor::wasm::WasmError::ResourceRejected) {
                                scene.record_resource_error();
                                return Err(AnimationPreparationRejected);
                            }
                            push_build_notice(
                                &mut build_notices,
                                anim_id.len(),
                                format_args!("effect '{anim_id}' failed to load: {e}"),
                            );
                        }
                    }
                }
                continue;
            }

            // Frame-based animation: render each scene `Frame` child
            // subtree via `layout_subtree`. The Animation node's scene
            // children are Flow nodes with `FlowSource::Frame`
            // (built by `build_animation`). Snapshot the frame ids
            // under an immutable borrow, then release it before
            // calling layout_subtree (which takes `&mut Scene`).
            let frame_child_capacity = scene.get(node).map_or(0, |n| n.children().len());
            let Some((mut frame_node_ids, _frame_node_ids_lease)) =
                try_governed_vec::<crate::compositor::scene::NodeId>(
                    &governor,
                    frame_child_capacity,
                )
            else {
                scene.record_resource_error();
                return Err(AnimationPreparationRejected);
            };
            if let Some(animation_node) = scene.get(node) {
                for &cid in animation_node.children() {
                    if matches!(
                        scene.get(cid).map(|n| n.kind()),
                        Some(crate::compositor::scene::NodeKind::Flow(f)) if matches!(
                            f.source, crate::compositor::scene::FlowSource::Frame
                        )
                    ) {
                        frame_node_ids.push(cid);
                    }
                }
            } else {
                continue;
            }
            if frame_node_ids.is_empty() {
                continue;
            }
            frame_node_ids.truncate(remaining_frames);
            remaining_frames -= frame_node_ids.len();
            if frame_node_ids.is_empty() {
                continue;
            }
            // `layout_subtree` runs the regular kind dispatch under a
            // throwaway buffer to build each frame's bitmap. The kind
            // dispatchers write `Node.placement` as a side effect, so
            // every `[pre]` under every frame would end up with origin
            // (0, 0) and a freshly-allocated per-node buffer — then the
            // composite walk would blit them at (0, 0) on top of the
            // real UI. Snapshot each frame's descendants before the
            // layout call and restore placements after so only the
            // animation's own output (Phase C of the composite walk)
            // drives what the user sees.
            let frame_collection = if super::reject_animate_allocation(
                super::AnimateAllocationSite::FrameCollection,
            ) {
                None
            } else {
                try_governed_vec::<crate::compositor::layout::cell::CellBuffer>(
                    &governor,
                    frame_node_ids.len(),
                )
            };
            let Some((mut frames, frame_collection_lease)) = frame_collection else {
                scene.record_resource_error();
                return Err(AnimationPreparationRejected);
            };
            for &fid in &frame_node_ids {
                let Some((descendants, _descendants_lease, _stack_lease)) =
                    collect_descendants(scene, fid, &governor)
                else {
                    scene.record_resource_error();
                    return Err(AnimationPreparationRejected);
                };
                let Some((mut snapshot, _snapshot_lease)) =
                    try_governed_vec::<(
                        crate::compositor::scene::NodeId,
                        crate::compositor::layout::engine::Placement,
                    )>(&governor, descendants.len())
                else {
                    scene.record_resource_error();
                    return Err(AnimationPreparationRejected);
                };
                for &id in &descendants {
                    if let Some(node) = scene.get(id) {
                        snapshot.push((id, *node.placement()));
                    }
                }
                let buf = crate::compositor::layout::engine::layout_subtree_governed(
                    scene,
                    fid,
                    region.w,
                    1,
                    color_support,
                    wcfg,
                    &governor,
                );
                if buf.allocation_failed() || scene.resource_limit_exceeded() {
                    scene.record_resource_error();
                    return Err(AnimationPreparationRejected);
                }
                for (id, placement) in snapshot {
                    scene.update_placement(id, placement);
                }
                frames.push(buf);
            }
            if frames.len() != frame_node_ids.len() {
                continue;
            }

            let retained_payload = anim_id
                .capacity()
                .saturating_add(after.as_ref().map_or(0, String::capacity));
            let adapter = FrameAnimationAdapter::new(
                anim_id,
                node,
                frames,
                fps,
                loop_behavior,
                delay_ms,
                after,
                autoplay,
                background,
            )
            .with_collection_budget(frame_collection_lease);
            retained_animation_payload =
                retained_animation_payload.saturating_add(retained_payload);
            animations.push(Box::new(adapter));
        }

        // The snapshot vector and temporary `src` strings have now dropped.
        // Keep only the exact capacities moved into installed adapters.
        animation_payload_lease.shrink_to(retained_animation_payload);
        let animation_payload_lease =
            (retained_animation_payload > 0).then_some(animation_payload_lease);

        // As with the payload lease: keep only what the notices retain, and
        // release the reservation outright on the common path where the page
        // built clean.
        build_notices.shrink_to_fit();
        let build_notice_lease = build_notice_lease.and_then(|mut lease| {
            let retained = build_notices
                .capacity()
                .saturating_mul(std::mem::size_of::<String>())
                .saturating_add(build_notices.iter().map(String::capacity).sum::<usize>());
            lease.shrink_to(retained);
            (retained > 0).then_some(lease)
        });

        Ok(Self {
            animations,
            transition_animations,
            start_time: now,
            _animation_collection_lease: Some(animation_collection_lease),
            _animation_payload_lease: animation_payload_lease,
            _transition_collection_lease: Some(transition_collection_lease),
            output_governor: Some(governor),
            build_notices,
            _build_notice_lease: build_notice_lease,
        })
    }

    /// Install a panel transition only when its page-lifetime collection was
    /// pre-admitted during scene construction. This never grows the vector.
    pub(crate) fn try_push_transition(&mut self, transition: TransitionAdapter) -> bool {
        if self.transition_animations.len() == self.transition_animations.capacity() {
            return false;
        }
        self.transition_animations.push(transition);
        true
    }

    pub(crate) fn can_push_transition(&self) -> bool {
        self.transition_animations.len() < self.transition_animations.capacity()
    }

    /// A successful viewport resize commits the newly laid-out panel state;
    /// old-size transition snapshots must not paint over it afterward.
    pub(crate) fn cancel_transitions_for_resize(&mut self) {
        self.transition_animations.clear();
    }

    #[cfg(test)]
    pub(crate) fn retained_collection_capacity_bytes(&self) -> usize {
        self.animations
            .capacity()
            .saturating_mul(std::mem::size_of::<Box<dyn Animation>>())
            .saturating_add(
                self.transition_animations
                    .capacity()
                    .saturating_mul(std::mem::size_of::<TransitionAdapter>()),
            )
    }

    /// `true` if there are any ongoing transition animations.
    pub fn has_transitions(&self) -> bool {
        !self.transition_animations.is_empty()
    }

    /// Any non-finished animation at all?
    pub fn has_animations(&self) -> bool {
        self.animations.iter().any(|a| !a.finished())
    }

    /// `true` if a scene-integrated page transition is currently
    /// running (a `PageTransitionAdapter` is in `animations` and has
    /// not yet finished). Distinct from `has_transitions` which only
    /// covers per-panel `TransitionAdapter`s.
    ///
    /// Used by the viewer loop to gate input handling and live-region
    /// polling — the user shouldn't be able to act mid-transition, and
    /// live subscriptions shouldn't deliver updates into a page that
    /// is still animating in.
    pub fn has_page_transition(&self) -> bool {
        self.animations
            .iter()
            .any(|a| a.id() == super::page_transition::PAGE_TRANSITION_ID && !a.finished())
    }

    pub(crate) fn can_push_page_transition(&self) -> bool {
        !self
            .animations
            .iter()
            .any(|animation| animation.id() == super::page_transition::PAGE_TRANSITION_ID)
            && self.animations.len() < self.animations.capacity()
    }

    pub(crate) fn try_push_page_transition(&mut self, animation: Box<dyn Animation>) -> bool {
        if animation.id() != super::page_transition::PAGE_TRANSITION_ID
            || !self.can_push_page_transition()
        {
            return false;
        }
        self.animations.push(animation);
        true
    }

    /// Retire a naturally-finished page transition only after its final frame
    /// has been painted. The specialized overlay path does not perform the
    /// fallible remotely-shaped subtree traversal used by generic patches.
    pub(crate) fn finish_page_transition(&mut self, scene: &mut Scene) {
        if let Some(overlay) = scene.page_transition_overlay() {
            scene.remove_page_transition_overlay(overlay);
        }
        self.animations
            .retain(|animation| animation.id() != super::page_transition::PAGE_TRANSITION_ID);
    }

    /// Cancel any running page transition: remove the adapter from
    /// `animations` and drop the associated `Overlay` node from the
    /// scene. Idempotent — a no-op if no page transition is active.
    ///
    /// Called on resize and on back/forward navigation interrupts,
    /// where finishing the current transition would deliver a blended
    /// frame into the wrong viewport or history state.
    pub fn cancel_page_transition(&mut self, scene: &mut Scene) -> bool {
        let had_transition = self
            .animations
            .iter()
            .any(|animation| animation.id() == super::page_transition::PAGE_TRANSITION_ID);
        self.animations.retain(|a| {
            if a.id() == super::page_transition::PAGE_TRANSITION_ID {
                // The adapter doesn't expose its overlay id through the
                // trait; downcasting would require Any. Instead, walk
                // the scene for the single Overlay with PageTransition
                // source — there can only be one at a time because the
                // loop cancels any prior before creating a new one.
                false
            } else {
                true
            }
        });
        let overlay = scene.page_transition_overlay();
        if let Some(id) = overlay {
            scene.remove_page_transition_overlay(id);
        }
        had_transition || overlay.is_some()
    }

    /// Any animation marked `background=true`?
    pub fn has_bg_animations(&self) -> bool {
        self.animations
            .iter()
            .any(|a| a.background() && !a.finished())
    }

    /// Trigger a specific animation to (re)start from the beginning.
    /// Called from `ActionKind::Animate`.
    pub fn trigger_start(&mut self, id: &str) -> bool {
        let now = Instant::now();
        let mut hit = false;
        for anim in &mut self.animations {
            if anim.id() == id {
                anim.trigger_start(now);
                hit = true;
            }
        }
        hit
    }

    /// `true` if any fast-forwardable animation is still running — the
    /// signal that the viewer's `f` shortcut should engage. Endless ambient
    /// backgrounds opt out, while finite background-layer cinematics opt in.
    pub fn has_foreground_animations(&self) -> bool {
        self.animations
            .iter()
            .any(|a| !a.finished() && a.fast_forwardable())
            || self.has_page_transition()
            || self.has_transitions()
    }

    /// Skip every fast-forwardable animation to its final rendered state and
    /// mark each finished. Endless ambient backgrounds are left running;
    /// finite cinematics may render behind content and still participate.
    ///
    /// Semantics differ from `trigger_stop`: this is "jump to the
    /// end", not "halt where you are". Each adapter decides what
    /// that means (frame → last frame, text effect → full string,
    /// tween → t=1.0 patch). Any active `PageTransitionAdapter` is
    /// canceled outright — dropping the overlay exposes the
    /// destination scene that was already composited underneath.
    /// Legacy per-panel `TransitionAdapter`s are cleared too.
    ///
    /// Returns `newly_finished` so the caller can fire
    /// `animation-end` events — many page-load flows cascade
    /// through those events (tagline finishes → `set:panel
    /// visible`), and a silent skip would leave dependent panels
    /// stranded.
    pub fn skip_all(&mut self, scene: &mut Scene) -> SkipResult {
        let governor = self.output_governor.clone();
        let animation_count = self.animations.len();
        let total_count = match animation_count.checked_add(self.transition_animations.len()) {
            Some(count) => count,
            None => return SkipResult::allocation_failed(),
        };
        let id_payload_bound = match self
            .animations
            .iter()
            .map(|animation| animation.id())
            .chain(self.transition_animations.iter().map(TransitionAdapter::id))
            .try_fold(0usize, |total, id| total.checked_add(id.len()))
        {
            Some(bytes) => bytes,
            None => return SkipResult::allocation_failed(),
        };
        let Some(mut output) = try_output_storage(
            governor.as_ref(),
            total_count,
            total_count,
            animation_count,
            0,
            id_payload_bound,
        ) else {
            return SkipResult::allocation_failed();
        };
        for animation in &self.animations {
            if animation.id() != super::page_transition::PAGE_TRANSITION_ID
                && !animation.finished()
                && animation.fast_forwardable()
            {
                let Some(id) = try_remote_string(animation.id()) else {
                    return SkipResult::allocation_failed();
                };
                output.newly_finished.push(id);
            }
        }
        for transition in &self.transition_animations {
            if !transition.finished() {
                let Some(id) = try_remote_string(transition.id()) else {
                    return SkipResult::allocation_failed();
                };
                output.newly_finished.push(id);
            }
        }

        // Page transitions: remove overlay node and drop the adapter.
        let page_transition_removed = self.cancel_page_transition(scene);

        for anim in &mut self.animations {
            if !anim.finished() && anim.fast_forwardable() {
                let r = anim.skip_with_scene(scene);
                if let Some(patch) = r.patch {
                    output.patches.push(patch);
                }
                if let Some(node) = r.wrote_buffer {
                    output.wrote_buffers.push(node);
                }
            }
        }
        output.newly_finished.retain(|id| {
            self.animations
                .iter()
                .any(|animation| animation.id() == id && animation.finished())
                || self
                    .transition_animations
                    .iter()
                    .any(|transition| transition.id() == id && !transition.finished())
        });
        // Commit final buffers from frame/effect adapters into scene.
        // paint_into_scene already no-ops for animations that haven't
        // changed state; background anims will continue painting
        // their own output on the next tick.
        self.paint_into_scene(scene);

        // Per-panel state transitions (fade/slide/dissolve between
        // panel states) write blended cells directly into the panel's
        // buffer every tick. A plain `clear()` would leave the panel
        // frozen on whatever intermediate blend was last painted —
        // `trigger_stop` + `paint` commits the t=1.0 frame (pure new
        // state) into the buffer before we drop the adapter.
        for trans in &mut self.transition_animations {
            let was_finished = trans.finished();
            if !was_finished {
                trans.trigger_stop();
            }
            if let Some(node) = trans.paint_into(scene) {
                output.wrote_buffers.push(node);
            }
        }
        self.transition_animations.clear();

        // Reconciling pre-admitted output releases units and admits nothing,
        // so it cannot be refused.
        let _ = reconcile_output_storage(&mut output);
        SkipResult {
            changed: page_transition_removed,
            patches: output.patches,
            wrote_buffers: output.wrote_buffers,
            newly_finished: output.newly_finished,
            allocation_failed: false,
            _collection_lease: output.lease,
        }
    }

    /// Force a specific animation to Finished. Called from
    /// `ActionKind::Stop`.
    pub fn trigger_stop(&mut self, id: &str) -> bool {
        let mut hit = false;
        for anim in &mut self.animations {
            if anim.id() == id {
                anim.trigger_stop();
                hit = true;
            }
        }
        hit
    }

    /// Resize size-dependent adapters against the scene's latest placements
    /// without rebuilding the runtime. Rebuilding would reset Waiting,
    /// Running, and Finished lifecycle state and strand animations that are
    /// started only by one-shot page events.
    pub(crate) fn prepare_resize(
        &self,
        scene: &Scene,
    ) -> Result<PreparedAnimationResize, AnimationPreparationRejected> {
        let governor = self
            .output_governor
            .clone()
            .or_else(|| scene.resource_governor())
            .unwrap_or_default();
        let mut candidates = Vec::new();
        let mut collection_lease = None;
        for (index, animation) in self.animations.iter().enumerate() {
            if let Some(candidate) = animation
                .prepare_resize(scene)
                .map_err(|_| AnimationPreparationRejected)?
            {
                if collection_lease.is_none() {
                    let Some((storage, lease)) =
                        try_governed_vec::<(usize, AnimationResizeCandidate)>(
                            &governor,
                            self.animations.len(),
                        )
                    else {
                        return Err(AnimationPreparationRejected);
                    };
                    candidates = storage;
                    collection_lease = Some(lease);
                }
                candidates.push((index, candidate));
            }
        }
        Ok(PreparedAnimationResize {
            candidates,
            _collection_lease: collection_lease,
        })
    }

    pub(crate) fn commit_resize(&mut self, scene: &mut Scene, prepared: PreparedAnimationResize) {
        for (index, candidate) in prepared.candidates {
            // The index was produced by the prepare pass over this same
            // collection; skip rather than abort if it no longer resolves.
            if let Some(animation) = self.animations.get_mut(index) {
                animation.commit_resize(scene, candidate);
            }
        }
    }

    /// Total linear-memory footprint of all WASM effect instances, in bytes.
    /// Zero when no WASM effects are active. Feeds the `{mem}` status var.
    pub fn total_wasm_memory(&self) -> usize {
        self.animations.iter().map(|a| a.memory_bytes()).sum()
    }

    /// Advance every animation by one tick. Returns `true` if any
    /// animation's `advance` reported a change (buffer write or patches).
    ///
    /// Also produces the list of animations that transitioned to
    /// `Finished` this tick, so the caller can fire `animation-end`
    /// events. Chain dependencies (`after="other"`) are resolved via
    /// `finished_ids` in `AdvanceCtx`.
    ///
    /// `scene` is required so WASM animations can `mem::swap` the scene
    /// node's buffer into the WASM host state for direct writes. Other
    /// adapters (frame, tween, text-effect) are scene-agnostic in their
    /// `advance` call; their paint happens via `paint_into_scene`
    /// after the tick.
    pub fn tick(
        &mut self,
        scene: &mut Scene,
        now: Instant,
        viewport_offset: u16,
        viewport_height: u16,
    ) -> TickResult {
        let governor = self.output_governor.clone();
        let animation_count = self.animations.len();
        let total_count = match animation_count.checked_add(self.transition_animations.len()) {
            Some(count) => count,
            None => return TickResult::allocation_failed(),
        };
        let id_payload_bound = match self
            .animations
            .iter()
            .map(|animation| animation.id())
            .chain(self.transition_animations.iter().map(TransitionAdapter::id))
            .try_fold(0usize, |total, id| total.checked_add(id.len()))
        {
            Some(bytes) => bytes,
            None => return TickResult::allocation_failed(),
        };
        let notice_payload_bound =
            match self.animations.iter().try_fold(0usize, |total, animation| {
                total.checked_add(animation.next_notice_capacity_bound())
            }) {
                Some(bytes) => bytes,
                None => return TickResult::allocation_failed(),
            };
        // Page-build notices are still waiting on the first tick after a
        // page load; they are admitted here with the adapters' own.
        let build_notice_count = self.build_notices.len();
        let Some(notice_capacity) = animation_count.checked_add(build_notice_count) else {
            return TickResult::allocation_failed();
        };
        let build_notice_payload = match self
            .build_notices
            .iter()
            .try_fold(0usize, |total, notice| total.checked_add(notice.capacity()))
        {
            Some(bytes) => bytes,
            None => return TickResult::allocation_failed(),
        };
        let Some((pre_finished, _pre_finished_lease)) =
            try_finished_snapshot(&self.animations, governor.as_ref())
        else {
            return TickResult::allocation_failed();
        };
        let Some(mut output) = try_output_storage(
            governor.as_ref(),
            animation_count,
            total_count,
            total_count,
            notice_capacity,
            match id_payload_bound
                .checked_add(notice_payload_bound)
                .and_then(|bytes| bytes.checked_add(build_notice_payload))
            {
                Some(bytes) => bytes,
                None => return TickResult::allocation_failed(),
            },
        ) else {
            return TickResult::allocation_failed();
        };
        for animation in &self.animations {
            let Some(id) = try_remote_string(animation.id()) else {
                return TickResult::allocation_failed();
            };
            output.newly_finished.push(id);
        }

        // The strings move into this tick's admitted payload, so the build
        // reservation goes with them.
        if build_notice_count > 0 {
            output.notices.append(&mut self.build_notices);
            self.build_notices = Vec::new();
            self._build_notice_lease = None;
        }

        let mut changed = false;

        for anim in &mut self.animations {
            let mut ctx = AdvanceCtx::new(now, viewport_offset, viewport_height, &pre_finished);
            // `advance_with_scene` defaults to `advance` for most
            // adapters; WASM overrides it to do the mem::swap dance.
            let r = anim.advance_with_scene(&mut ctx, scene);
            if !r.is_noop() {
                changed = true;
            }
            if let Some(node) = r.wrote_buffer {
                output.wrote_buffers.push(node);
            }
            if let Some(notice) = r.notice {
                output.notices.push(notice);
            }
            if let Some(patch) = r.patch {
                output.patches.push(patch);
            }
        }

        // Transitions tick via the Animation trait and write the
        // blended frame into the target node's buffer via paint.
        let finished_snapshot: &[String] = &[];
        for trans in &mut self.transition_animations {
            if trans.finished() {
                continue;
            }
            let mut ctx = AdvanceCtx::new(now, viewport_offset, viewport_height, finished_snapshot);
            let r = trans.advance(&mut ctx);
            if !r.is_noop() {
                changed = true;
            }
            if let Some(node) = r.wrote_buffer {
                output.wrote_buffers.push(node);
            }
            if let Some(patch) = r.patch {
                output.patches.push(patch);
            }
            trans.paint(scene);
        }
        self.transition_animations.retain(|t| !t.finished());

        output.newly_finished.retain(|id| {
            self.animations.iter().any(|animation| {
                animation.id() == id
                    && animation.state() == AnimState::Finished
                    && !pre_finished.iter().any(|finished| finished == id)
            })
        });

        // As above: reconciliation only ever releases.
        let _ = reconcile_output_storage(&mut output);

        TickResult {
            changed,
            newly_finished: output.newly_finished,
            wrote_buffers: output.wrote_buffers,
            patches: output.patches,
            notices: output.notices,
            allocation_failed: false,
            _collection_lease: output.lease,
        }
    }

    /// Call after tick to copy each buffer-writing adapter's output
    /// into its scene node. WASM adapters write through `mem::swap`
    /// inside `tick` and don't need this; frame and text-effect
    /// adapters paint here.
    pub fn paint_into_scene(&self, scene: &mut crate::compositor::scene::Scene) {
        for anim in &self.animations {
            if anim.paints_buffer()
                && matches!(
                    anim.state(),
                    AnimState::Running | AnimState::Paused | AnimState::Finished
                )
            {
                anim.paint(scene);
            }
        }
    }
}

/// Per-tick output of `AnimationRuntime::tick`. The caller applies
/// patches via `PatchApplier`, marks wrote-buffer rects for composite
/// invalidation, and fires `animation-end` events for `newly_finished`.
#[derive(Debug, Default)]
pub struct TickResult {
    pub changed: bool,
    pub newly_finished: Vec<String>,
    pub wrote_buffers: Vec<crate::compositor::scene::NodeId>,
    pub patches: Vec<crate::compositor::scene::Patch>,
    /// User-facing messages emitted by adapters this tick (e.g. an effect
    /// stopped by the WASM memory limit). Surfaced in the client HUD.
    pub notices: Vec<String>,
    pub allocation_failed: bool,
    _collection_lease: Option<BudgetLease>,
}

impl TickResult {
    fn allocation_failed() -> Self {
        Self {
            allocation_failed: true,
            ..Self::default()
        }
    }

    pub(crate) fn from_skip(skipped: SkipResult) -> Self {
        Self {
            changed: skipped.changed
                || !skipped.wrote_buffers.is_empty()
                || !skipped.patches.is_empty(),
            newly_finished: skipped.newly_finished,
            wrote_buffers: skipped.wrote_buffers,
            patches: skipped.patches,
            notices: Vec::new(),
            allocation_failed: skipped.allocation_failed,
            _collection_lease: skipped._collection_lease,
        }
    }
}

/// Output of `AnimationRuntime::skip_all`. The caller applies
/// `patches` via `PatchApplier` and feeds `wrote_buffers` rects into
/// the composite invalidation channel (same as the tick path).
#[derive(Debug, Default)]
pub struct SkipResult {
    /// Compositor-owned topology changed even when no authored adapter wrote
    /// a buffer or patch (for example, cancelling a page transition).
    pub changed: bool,
    pub patches: Vec<crate::compositor::scene::Patch>,
    pub wrote_buffers: Vec<crate::compositor::scene::NodeId>,
    /// IDs of animations that transitioned from running to finished
    /// as part of this skip. The viewer dispatches `animation-end`
    /// for each so event-driven page-load cascades (e.g. tagline
    /// finishes → reveal dir panel) proceed instead of stranding
    /// downstream panels forever.
    pub newly_finished: Vec<String>,
    pub allocation_failed: bool,
    _collection_lease: Option<BudgetLease>,
}

impl SkipResult {
    fn allocation_failed() -> Self {
        Self {
            allocation_failed: true,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ResizeProbe {
        id: &'static str,
        prepared: std::rc::Rc<std::cell::Cell<usize>>,
        committed: std::rc::Rc<std::cell::Cell<usize>>,
        reject: std::rc::Rc<std::cell::Cell<bool>>,
    }

    impl Animation for ResizeProbe {
        fn id(&self) -> &str {
            self.id
        }

        fn advance(&mut self, _ctx: &mut AdvanceCtx) -> super::super::AdvanceResult {
            super::super::AdvanceResult::none()
        }

        fn finished(&self) -> bool {
            false
        }

        fn state(&self) -> AnimState {
            AnimState::Running
        }

        fn prepare_resize(
            &self,
            _scene: &Scene,
        ) -> Result<Option<AnimationResizeCandidate>, super::super::AnimationResizeRejected>
        {
            self.prepared.set(self.prepared.get() + 1);
            if self.reject.get() {
                return Err(super::super::AnimationResizeRejected);
            }
            Ok(Some(AnimationResizeCandidate {
                width: 1,
                height: 1,
                swap_buffer: crate::compositor::layout::cell::CellBuffer::new(1, 1),
                output_buffer: crate::compositor::layout::cell::CellBuffer::new(1, 1),
            }))
        }

        fn commit_resize(&mut self, _scene: &mut Scene, _candidate: AnimationResizeCandidate) {
            self.committed.set(self.committed.get() + 1);
        }
    }

    #[test]
    fn resize_prepares_every_adapter_before_committing_any() {
        let first_prepared = std::rc::Rc::new(std::cell::Cell::new(0));
        let first_committed = std::rc::Rc::new(std::cell::Cell::new(0));
        let second_prepared = std::rc::Rc::new(std::cell::Cell::new(0));
        let second_committed = std::rc::Rc::new(std::cell::Cell::new(0));
        let reject = std::rc::Rc::new(std::cell::Cell::new(true));
        let mut runtime = AnimationRuntime::new(vec![
            Box::new(ResizeProbe {
                id: "first",
                prepared: first_prepared.clone(),
                committed: first_committed.clone(),
                reject: std::rc::Rc::new(std::cell::Cell::new(false)),
            }),
            Box::new(ResizeProbe {
                id: "second",
                prepared: second_prepared.clone(),
                committed: second_committed.clone(),
                reject: reject.clone(),
            }),
        ]);
        let mut scene = minimal_scene();

        assert!(runtime.prepare_resize(&scene).is_err());
        assert_eq!(first_prepared.get(), 1);
        assert_eq!(second_prepared.get(), 1);
        assert_eq!(first_committed.get(), 0);
        assert_eq!(second_committed.get(), 0);

        reject.set(false);
        let prepared = runtime.prepare_resize(&scene).unwrap();
        runtime.commit_resize(&mut scene, prepared);
        assert_eq!(first_committed.get(), 1);
        assert_eq!(second_committed.get(), 1);
    }

    #[test]
    fn size_free_resize_preparation_allocates_nothing() {
        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let mut runtime = AnimationRuntime::empty();
        runtime.output_governor = Some(governor.clone());
        let mut scene = minimal_scene();
        let baseline_count = governor.count(ResourceCategory::RemoteCollections);
        let baseline_used = governor.used(ResourceCategory::RemoteCollections);

        let prepared = runtime.prepare_resize(&scene).unwrap();
        runtime.commit_resize(&mut scene, prepared);
        assert_eq!(
            governor.count(ResourceCategory::RemoteCollections),
            baseline_count
        );
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            baseline_used
        );
        drop(blocker);
    }

    #[test]
    fn governed_runtime_vector_rejection_preserves_existing_usage() {
        let governor = ResourceGovernor::new();
        let existing = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();

        assert!(try_governed_vec::<u8>(&governor, 1).is_none());
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            crate::resource::MAX_REMOTE_MEMORY
        );

        drop(existing);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn prepared_runtime_returns_shared_governor_rejection() {
        let src = r#"[page mode=document][text]x[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let governor = ResourceGovernor::new();
        let mut scene = crate::compositor::scene::build::from_document_governed(&doc, &governor);
        let remaining = crate::resource::MAX_REMOTE_MEMORY - governor.total_used();
        let blocker = governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let result = executor.block_on(AnimationRuntime::from_scene_with_prepared_wasm(
            &mut scene,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
            &governor,
            &std::collections::HashMap::new(),
        ));

        assert!(matches!(result, Err(AnimationPreparationRejected)));
        assert!(scene.resource_limit_exceeded());
        drop(blocker);
    }

    fn minimal_scene() -> Scene {
        let src = r#"[page mode=document][text]x[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        crate::compositor::scene::build::from_document(&doc)
    }

    #[test]
    fn tick_output_preadmission_failure_does_not_advance_animation() {
        struct FinishOnAdvance {
            done: bool,
        }
        impl Animation for FinishOnAdvance {
            fn id(&self) -> &str {
                "remote-animation-id"
            }
            fn advance(&mut self, _ctx: &mut AdvanceCtx) -> super::super::AdvanceResult {
                self.done = true;
                super::super::AdvanceResult::none()
            }
            fn finished(&self) -> bool {
                self.done
            }
            fn state(&self) -> AnimState {
                if self.done {
                    AnimState::Finished
                } else {
                    AnimState::Running
                }
            }
        }

        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let before = governor.used(ResourceCategory::RemoteCollections);
        let mut runtime = AnimationRuntime::new(vec![Box::new(FinishOnAdvance { done: false })]);
        runtime.output_governor = Some(governor.clone());
        let mut scene = minimal_scene();

        let result = runtime.tick(&mut scene, Instant::now(), 0, 24);

        assert!(result.allocation_failed);
        assert_eq!(runtime.animations[0].state(), AnimState::Running);
        assert!(!scene.resource_limit_exceeded());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);
        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn tick_output_retains_exact_lease_until_result_drop() {
        struct FinishOnAdvance {
            done: bool,
        }
        impl Animation for FinishOnAdvance {
            fn id(&self) -> &str {
                "leased-animation-id"
            }
            fn advance(&mut self, _ctx: &mut AdvanceCtx) -> super::super::AdvanceResult {
                self.done = true;
                super::super::AdvanceResult::none()
            }
            fn finished(&self) -> bool {
                self.done
            }
            fn state(&self) -> AnimState {
                if self.done {
                    AnimState::Finished
                } else {
                    AnimState::Running
                }
            }
        }

        let governor = ResourceGovernor::new();
        let mut runtime = AnimationRuntime::new(vec![Box::new(FinishOnAdvance { done: false })]);
        runtime.output_governor = Some(governor.clone());
        let mut scene = minimal_scene();
        let result = runtime.tick(&mut scene, Instant::now(), 0, 24);
        assert!(!result.allocation_failed);
        assert_eq!(result.newly_finished, ["leased-animation-id"]);
        let expected = result
            .newly_finished
            .capacity()
            .saturating_mul(std::mem::size_of::<String>())
            .saturating_add(
                result
                    .wrote_buffers
                    .capacity()
                    .saturating_mul(std::mem::size_of::<crate::compositor::scene::NodeId>()),
            )
            .saturating_add(
                result
                    .patches
                    .capacity()
                    .saturating_mul(std::mem::size_of::<crate::compositor::scene::Patch>()),
            )
            .saturating_add(
                result
                    .notices
                    .capacity()
                    .saturating_mul(std::mem::size_of::<String>()),
            )
            .saturating_add(
                result
                    .newly_finished
                    .iter()
                    .chain(&result.notices)
                    .map(String::capacity)
                    .sum::<usize>(),
            );
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), expected);

        drop(result);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn skip_output_preadmission_failure_does_not_stop_animation() {
        struct StopOnSkip {
            stopped: bool,
        }
        impl Animation for StopOnSkip {
            fn id(&self) -> &str {
                "skippable-remote-animation"
            }
            fn advance(&mut self, _ctx: &mut AdvanceCtx) -> super::super::AdvanceResult {
                super::super::AdvanceResult::none()
            }
            fn finished(&self) -> bool {
                self.stopped
            }
            fn state(&self) -> AnimState {
                if self.stopped {
                    AnimState::Finished
                } else {
                    AnimState::Running
                }
            }
            fn trigger_stop(&mut self) {
                self.stopped = true;
            }
        }

        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let before = governor.used(ResourceCategory::RemoteCollections);
        let mut runtime = AnimationRuntime::new(vec![Box::new(StopOnSkip { stopped: false })]);
        runtime.output_governor = Some(governor.clone());
        let mut scene = minimal_scene();

        let result = runtime.skip_all(&mut scene);

        assert!(result.allocation_failed);
        assert_eq!(runtime.animations[0].state(), AnimState::Running);
        assert!(!scene.resource_limit_exceeded());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);
        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    struct StubAnim {
        id: String,
        state: AnimState,
    }

    impl Animation for StubAnim {
        fn id(&self) -> &str {
            &self.id
        }
        fn advance(&mut self, _ctx: &mut AdvanceCtx) -> super::super::AdvanceResult {
            super::super::AdvanceResult::none()
        }
        fn finished(&self) -> bool {
            self.state == AnimState::Finished
        }
        fn state(&self) -> AnimState {
            self.state
        }
        fn trigger_start(&mut self, _now: Instant) {
            self.state = AnimState::Running;
        }
        fn trigger_stop(&mut self) {
            self.state = AnimState::Finished;
        }
    }

    #[test]
    fn empty_runtime_has_no_animations() {
        let rt = AnimationRuntime::empty();
        assert!(!rt.has_animations());
        assert!(!rt.has_bg_animations());
    }

    #[test]
    fn trigger_start_and_stop_by_id() {
        let mut rt = AnimationRuntime::new(vec![
            Box::new(StubAnim {
                id: "a".into(),
                state: AnimState::Waiting,
            }),
            Box::new(StubAnim {
                id: "b".into(),
                state: AnimState::Waiting,
            }),
        ]);
        assert!(rt.trigger_start("a"));
        assert_eq!(rt.animations[0].state(), AnimState::Running);
        assert_eq!(rt.animations[1].state(), AnimState::Waiting);
        assert!(rt.trigger_stop("a"));
        assert_eq!(rt.animations[0].state(), AnimState::Finished);
    }

    // ─── Page-transition runtime helpers (Phase 3) ───────────────

    fn scene_with_page_transition() -> (
        crate::compositor::scene::Scene,
        crate::compositor::scene::NodeId,
    ) {
        use crate::compositor::layout::Rect;
        let src = r#"[page mode=document][text]x[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        assert!(scene.prepare_page_transition_overlay());
        let id = scene.page_transition_overlay_slot().unwrap();
        let buffer = crate::compositor::layout::cell::CellBuffer::try_new_opaque(10, 2).unwrap();
        assert_eq!(
            scene.activate_page_transition_overlay(Rect::new(0, 0, 10, 2), buffer),
            Some(id),
        );
        (scene, id)
    }

    #[test]
    fn has_page_transition_distinguishes_adapter_from_other_anims() {
        use crate::compositor::animate::page_transition::PageTransitionAdapter;
        use crate::compositor::layout::cell::CellBuffer;
        use crate::parser::ast::TransitionKind;

        let (_scene, overlay_id) = scene_with_page_transition();
        let mut rt = AnimationRuntime::new(vec![Box::new(StubAnim {
            id: "user_anim".into(),
            state: AnimState::Running,
        })]);
        assert!(!rt.has_page_transition(), "no adapter in rt");

        rt.animations.push(Box::new(PageTransitionAdapter::new(
            overlay_id,
            CellBuffer::new(10, 2),
            CellBuffer::new(10, 2),
            TransitionKind::Cut,
            66,
        )));
        assert!(
            rt.has_page_transition(),
            "adapter present → has_page_transition"
        );
        assert!(rt.has_animations(), "adapter also counts as an animation");
    }

    #[test]
    fn cancel_page_transition_drops_adapter_and_overlay() {
        use crate::compositor::animate::page_transition::PageTransitionAdapter;
        use crate::compositor::layout::cell::CellBuffer;
        use crate::compositor::scene::NodeKind;
        use crate::parser::ast::TransitionKind;

        let (mut scene, overlay_id) = scene_with_page_transition();
        let mut rt = AnimationRuntime::new(vec![
            Box::new(PageTransitionAdapter::new(
                overlay_id,
                CellBuffer::new(10, 2),
                CellBuffer::new(10, 2),
                TransitionKind::Fade,
                100,
            )),
            Box::new(StubAnim {
                id: "user_anim".into(),
                state: AnimState::Running,
            }),
        ]);

        rt.cancel_page_transition(&mut scene);

        assert!(scene.get(overlay_id).is_some(), "dormant slot is retained");
        assert!(scene.page_transition_overlay().is_none());
        assert!(!rt.has_page_transition(), "adapter removed from animations");
        assert_eq!(
            rt.animations.len(),
            1,
            "only the user animation remains — cancel is surgical",
        );
        assert_eq!(rt.animations[0].id(), "user_anim");
        assert!(matches!(
            scene.get(overlay_id).unwrap().kind(),
            NodeKind::Overlay(_)
        ));
    }

    #[test]
    fn natural_page_transition_completion_is_published_before_surgical_teardown() {
        use crate::compositor::animate::page_transition::{
            PAGE_TRANSITION_ID, PageTransitionAdapter,
        };
        use crate::compositor::layout::cell::CellBuffer;
        use crate::parser::ast::TransitionKind;

        let (mut scene, overlay_id) = scene_with_page_transition();
        let mut rt = AnimationRuntime::new(vec![Box::new(PageTransitionAdapter::new(
            overlay_id,
            CellBuffer::new(10, 2),
            CellBuffer::new(10, 2),
            TransitionKind::Cut,
            33,
        ))]);

        let tick = rt.tick(&mut scene, Instant::now(), 0, 2);
        assert_eq!(tick.newly_finished, [PAGE_TRANSITION_ID]);
        assert!(tick.patches.is_empty(), "teardown is not a generic patch");
        rt.paint_into_scene(&mut scene);
        rt.finish_page_transition(&mut scene);

        assert!(scene.get(overlay_id).is_some());
        assert!(scene.page_transition_overlay().is_none());
        assert!(
            rt.animations
                .iter()
                .all(|animation| animation.id() != PAGE_TRANSITION_ID)
        );
    }

    #[test]
    fn skip_all_leaves_background_animations_running() {
        struct BgAnim {
            id: String,
            state: AnimState,
        }
        impl Animation for BgAnim {
            fn id(&self) -> &str {
                &self.id
            }
            fn advance(&mut self, _c: &mut AdvanceCtx) -> super::super::AdvanceResult {
                super::super::AdvanceResult::none()
            }
            fn finished(&self) -> bool {
                self.state == AnimState::Finished
            }
            fn state(&self) -> AnimState {
                self.state
            }
            fn background(&self) -> bool {
                true
            }
            fn trigger_stop(&mut self) {
                self.state = AnimState::Finished;
            }
        }

        let (mut scene, _) = scene_with_page_transition();
        let mut rt = AnimationRuntime::new(vec![
            Box::new(BgAnim {
                id: "matrix".into(),
                state: AnimState::Running,
            }),
            Box::new(StubAnim {
                id: "fg".into(),
                state: AnimState::Running,
            }),
        ]);

        assert!(rt.has_foreground_animations(), "foreground present");
        rt.skip_all(&mut scene);

        // Background kept running, foreground finished.
        assert!(!rt.animations[0].finished(), "background survives skip");
        assert!(rt.animations[1].finished(), "foreground skipped");
        // And the global gate now reads false — only the background
        // is left, so spacebar reverts to page-down semantics.
        assert!(!rt.has_foreground_animations());
    }

    #[test]
    fn skip_all_finishes_animations_and_drops_page_transition() {
        use crate::compositor::animate::page_transition::PageTransitionAdapter;
        use crate::compositor::layout::cell::CellBuffer;
        use crate::parser::ast::TransitionKind;

        let (mut scene, overlay_id) = scene_with_page_transition();
        let mut rt = AnimationRuntime::new(vec![
            Box::new(PageTransitionAdapter::new(
                overlay_id,
                CellBuffer::new(10, 2),
                CellBuffer::new(10, 2),
                TransitionKind::Fade,
                200,
            )),
            Box::new(StubAnim {
                id: "a".into(),
                state: AnimState::Running,
            }),
            Box::new(StubAnim {
                id: "b".into(),
                state: AnimState::Running,
            }),
        ]);

        let result = rt.skip_all(&mut scene);

        // Page transition: overlay and adapter gone.
        assert!(scene.get(overlay_id).is_some());
        assert!(scene.page_transition_overlay().is_none());
        assert!(!rt.has_page_transition());
        assert!(result.changed);
        // Remaining animations are Finished.
        assert_eq!(rt.animations.len(), 2);
        assert!(rt.animations.iter().all(|a| a.finished()));
        // StubAnim returns no patches/buffers — but skip_all should
        // have produced a valid SkipResult.
        assert!(result.patches.is_empty());
        assert!(result.wrote_buffers.is_empty());
        // Both running StubAnims were skipped; names surface so the
        // viewer can fire animation-end and engage page-load
        // cascades. The PageTransitionAdapter is cancelled via
        // `cancel_page_transition`, not `skip()`, so it's not in
        // this list.
        let mut ids = result.newly_finished.clone();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn skip_all_paints_final_frame_of_panel_transition() {
        use crate::compositor::animate::transition::TransitionAdapter;
        use crate::compositor::layout::Rect;
        use crate::compositor::layout::cell::{CellBuffer, CellStyle};
        use crate::parser::ast::TransitionKind;

        let doc = {
            let src = r#"[page mode=document]
                [panel id="p" state="a"]
                    [state name="a"][text]A[/text][/state]
                    [state name="b"][text]B[/text][/state]
                [/panel]
            [/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let panel = scene.find_by_aml_id("p").unwrap();
        scene.allocate_buffer(panel, 5, 2);

        let rect = Rect::new(0, 0, 5, 2);
        let style = CellStyle::default();
        let mut old_buf = CellBuffer::new(5, 2);
        let mut new_buf = CellBuffer::new(5, 2);
        for y in 0..2 {
            for x in 0..5 {
                old_buf.put_char(x, y, 'A', &style);
                new_buf.put_char(x, y, 'B', &style);
            }
        }
        let trans = TransitionAdapter::new(
            "t".into(),
            panel,
            rect,
            old_buf,
            rect,
            new_buf,
            rect,
            TransitionKind::Dissolve,
            200,
        );

        let mut rt = AnimationRuntime::empty();
        rt.transition_animations.push(trans);

        let result = rt.skip_all(&mut scene);

        // Transition dropped, and the target panel's buffer now holds
        // the final 'B' frame (t=1.0 → new state wins for every cell).
        assert!(rt.transition_animations.is_empty());
        assert_eq!(result.wrote_buffers, vec![panel]);
        let buf = scene.panel_buffer_mut(panel).unwrap();
        for y in 0..2 {
            for x in 0..5 {
                assert_eq!(
                    buf.get(x, y).map(|c| c.ch),
                    Some('B'),
                    "panel cell ({}, {}) should hold the final new-state char",
                    x,
                    y,
                );
            }
        }
    }

    #[test]
    fn skip_all_emits_final_tween_transform_patch() {
        use crate::compositor::animate::tween::TweenAdapter;
        use crate::compositor::scene::Patch;
        use crate::parser::ast::{Easing, Keyframe, LoopBehavior};

        let doc = {
            let src = r#"[page mode=document][box w=5 h=5][/box][/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let root = scene.root();
        let box_id = scene.get(root).unwrap().children()[0];

        let tween = TweenAdapter::new(
            "t".into(),
            box_id,
            vec![
                Keyframe {
                    t_percent: 0.0,
                    x: Some(0),
                    y: Some(0),
                    fg: None,
                    bg: None,
                },
                Keyframe {
                    t_percent: 100.0,
                    x: Some(40),
                    y: Some(10),
                    fg: None,
                    bg: None,
                },
            ],
            1000,
            Easing::Linear,
            0,
            LoopBehavior::None,
        );
        let mut rt = AnimationRuntime::new(vec![Box::new(tween)]);

        // Before skip: tween is Running, no patch emitted yet.
        assert!(!rt.animations[0].finished());

        let result = rt.skip_all(&mut scene);

        // Exactly one SetTransform at the final keyframe coordinates.
        assert_eq!(result.patches.len(), 1);
        match &result.patches[0] {
            Patch::SetTransform { transform, .. } => {
                assert_eq!((transform.dx, transform.dy), (40, 10));
            }
            other => panic!("expected SetTransform, got {:?}", other),
        }
        assert!(rt.animations[0].finished());
    }

    #[test]
    fn cancel_page_transition_idempotent_when_no_transition_running() {
        let (mut scene, _overlay_id) = scene_with_page_transition();
        // Remove the preset overlay to get a "no transition" baseline.
        scene.remove_page_transition_overlay(_overlay_id);

        let mut rt = AnimationRuntime::new(vec![Box::new(StubAnim {
            id: "user_anim".into(),
            state: AnimState::Running,
        })]);
        rt.cancel_page_transition(&mut scene);
        // No panic, user animation untouched.
        assert_eq!(rt.animations.len(), 1);
    }

    #[test]
    fn tick_returns_newly_finished() {
        struct FinishOnce {
            id: String,
            done: bool,
        }
        impl Animation for FinishOnce {
            fn id(&self) -> &str {
                &self.id
            }
            fn advance(&mut self, _c: &mut AdvanceCtx) -> super::super::AdvanceResult {
                self.done = true;
                super::super::AdvanceResult::none()
            }
            fn finished(&self) -> bool {
                self.done
            }
            fn state(&self) -> AnimState {
                if self.done {
                    AnimState::Finished
                } else {
                    AnimState::Running
                }
            }
        }
        // Build a minimal scene for the tick() signature.
        let doc = {
            let src = r#"[page mode=document][text]x[/text][/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        let mut scene = crate::compositor::scene::build::from_document(&doc);

        let mut rt = AnimationRuntime::new(vec![Box::new(FinishOnce {
            id: "x".into(),
            done: false,
        })]);
        let tr = rt.tick(&mut scene, Instant::now(), 0, 24);
        assert_eq!(tr.newly_finished, vec!["x"]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn wasm_effect_receives_composed_multiline_child_content() {
        let src = r#"[page mode=screen cols=10 rows=4]
[animate id="fx" src="/effects/typewriter.wasm" fps=30 loop=false]
  [pre]AB
CD[/pre]
[/animate]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let layout =
            crate::compositor::layout::engine::layout_scene(&mut scene, 10, 4, color, wcfg);
        for placed in &layout.placed {
            if placed.is_animation() && !placed.rect.is_empty() {
                let node = scene.find_by_aml_id(&placed.id).unwrap();
                scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
            }
        }

        let wasm_dir = crate::repository_root().join("tests/fixtures/site");
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut runtime = executor.block_on(AnimationRuntime::from_scene(
            &mut scene,
            color,
            wcfg,
            Some(&wasm_dir),
        ));
        assert_eq!(runtime.animations.len(), 1);

        let start = Instant::now();
        for tick in 1..=8 {
            runtime.tick(
                &mut scene,
                start + std::time::Duration::from_millis(tick * 50),
                0,
                4,
            );
        }

        let node = scene.find_by_aml_id("fx").unwrap();
        let output = scene.buffer_of(node).unwrap();
        assert_eq!(output.get(0, 0).unwrap().ch, 'A');
        assert_eq!(output.get(1, 0).unwrap().ch, 'B');
        assert_eq!(output.get(0, 1).unwrap().ch, 'C');
        assert_eq!(output.get(1, 1).unwrap().ch, 'D');
    }

    /// With neither a directory nor a prepared batch, a `src=` animation is
    /// dropped without a word — no adapter, no build notice, no scene error.
    ///
    /// That silence is the mechanism behind a page coming back visibly broken.
    /// Dismissing a client-owned error page used to re-lay-out the cached AML
    /// through `load_cached`, which supplies neither source, so every WASM
    /// effect vanished; on a page that reveals content with
    /// `[on event="animation-end"]`, the content went with them, because the
    /// events belonged to effects that were never built. The restore path goes
    /// through the reducer now, which fetches the modules first. This pins the
    /// behaviour that makes doing it any other way wrong.
    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn a_wasm_effect_with_no_module_source_is_dropped_in_silence() {
        let src = r#"[page mode=screen cols=10 rows=4]
[animate id="fx" src="/effects/typewriter.wasm" fps=30 loop=false]
  [pre]AB
CD[/pre]
[/animate]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let layout =
            crate::compositor::layout::engine::layout_scene(&mut scene, 10, 4, color, wcfg);
        for placed in &layout.placed {
            if placed.is_animation() && !placed.rect.is_empty() {
                let node = scene.find_by_aml_id(&placed.id).unwrap();
                scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
            }
        }

        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let runtime =
            executor.block_on(AnimationRuntime::from_scene(&mut scene, color, wcfg, None));

        assert!(runtime.animations.is_empty());
        assert!(
            runtime.build_notices.is_empty(),
            "the drop is silent, which is what makes it dangerous"
        );
        assert!(!scene.resource_limit_exceeded());
    }

    #[test]
    #[cfg_attr(miri, ignore = "1,025-region boundary is covered by native tests")]
    fn from_scene_enforces_runtime_animation_budgets_if_parser_errors_are_ignored() {
        let mut src = String::from("[page mode=document]");
        for index in 0..=crate::parser::MAX_ANIMATE_REGIONS {
            src.push_str(&format!(
                "[animate id=\"a{index}\"][frame][text]x[/text][/frame][/animate]",
            ));
        }
        src.push_str("[/page]");
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let parsed = crate::parser::parse(tokens);
        assert!(parsed.has_errors(), "fixture must exceed the parser limit");
        let mut scene = crate::compositor::scene::build::from_document(
            &parsed
                .document
                .expect("parser retains an AST for diagnostics"),
        );
        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let layout =
            crate::compositor::layout::engine::layout_scene(&mut scene, 20, 20, color, wcfg);
        for placed in &layout.placed {
            if placed.is_animation() && !placed.rect.is_empty() {
                let node = scene.find_by_aml_id(&placed.id).unwrap();
                scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
                scene.update_placement(
                    node,
                    crate::compositor::layout::engine::Placement {
                        rect: placed.rect,
                        flow_advance: placed.rect.h,
                        bbox: placed.rect,
                    },
                );
            }
        }

        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let runtime =
            executor.block_on(AnimationRuntime::from_scene(&mut scene, color, wcfg, None));
        // Each region has one authored frame, so the independent frame budget
        // is exhausted before the larger region traversal ceiling.
        assert_eq!(
            runtime.animations.len(),
            crate::parser::MAX_ANIMATION_FRAMES
        );
    }

    /// An effect the page frame budget cannot admit is reported through the
    /// tick's notices, which is what the HUD Errors tab reads.
    ///
    /// The client owns the terminal for the life of the session, so the old
    /// `eprintln!` on this path was unreadable by construction: it landed on
    /// top of the page it was complaining about, underneath a background
    /// effect. The notice is raised while the page is being built, before any
    /// tick has run, so what this pins is the hand-off — the build keeps it
    /// until a tick can carry it out.
    #[test]
    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    fn wasm_effect_over_the_frame_budget_is_reported_through_tick_notices() {
        let effects = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("client crate is nested under <workspace>/crates")
            .join("effects/line_draw/target/wasm32-unknown-unknown/release");
        if !effects.join("line_draw.wasm").exists() {
            eprintln!("skipping: line_draw.wasm not built");
            return;
        }

        // line-draw declares one frame per painted cell plus a blank one, so a
        // solid block of dashes is the shortest way to author past the budget.
        let rows = (crate::parser::MAX_ANIMATION_FRAMES / 10) + 2;
        let mut src = String::from(
            "[page mode=document]\n[animate id=\"long\" src=\"/line_draw.wasm\"]\n[pre]",
        );
        for _ in 0..rows {
            src.push_str("──────────\n");
        }
        src.push_str("[/pre]\n[/animate]\n[/page]");

        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let height = u16::try_from(rows + 4).unwrap();
        let layout =
            crate::compositor::layout::engine::layout_scene(&mut scene, 20, height, color, wcfg);
        for placed in &layout.placed {
            if placed.is_animation() && !placed.rect.is_empty() {
                let node = scene.find_by_aml_id(&placed.id).unwrap();
                scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
                scene.update_placement(
                    node,
                    crate::compositor::layout::engine::Placement {
                        rect: placed.rect,
                        flow_advance: placed.rect.h,
                        bbox: placed.rect,
                    },
                );
            }
        }

        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut runtime = executor.block_on(AnimationRuntime::from_scene(
            &mut scene,
            color,
            wcfg,
            Some(effects.as_path()),
        ));
        assert!(
            runtime.animations.is_empty(),
            "the effect is refused, not truncated"
        );

        let result = runtime.tick(&mut scene, Instant::now(), 0, height);
        assert_eq!(result.notices.len(), 1, "one notice for one refusal");
        let notice = &result.notices[0];
        assert!(
            notice.contains("long") && notice.contains("frames"),
            "notice names the effect and the budget: {notice}"
        );

        // Delivered once: a notice repeated every tick would bury the log.
        let repeat = runtime.tick(&mut scene, Instant::now(), 0, height);
        assert!(repeat.notices.is_empty(), "notices are drained, not held");
    }

    /// Refusing the build-notice storage costs the notices, not the page.
    ///
    /// Every other admission failure here discards the candidate: a page whose
    /// frames or payload cannot be admitted is not a page. Notices are the
    /// exception, because they exist to report a failure -- dropping the page
    /// because its complaint would not fit is the wrong trade. The row in
    /// verification/allocation-owners.tsv claims that behaviour; this holds it.
    #[test]
    fn refused_effect_reaches_the_error_log() {
        use crate::compositor::animate::{AnimateAllocationSite, AnimateRejectionGuard};

        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let laid_out_scene = || {
            let src = r#"[page mode=document]
[animate id="a"][frame][text]x[/text][/frame][/animate]
[/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            let doc = crate::parser::parse(tokens).document.unwrap();
            let mut scene = crate::compositor::scene::build::from_document(&doc);
            let layout =
                crate::compositor::layout::engine::layout_scene(&mut scene, 20, 5, color, wcfg);
            for placed in &layout.placed {
                if placed.is_animation() && !placed.rect.is_empty() {
                    let node = scene.find_by_aml_id(&placed.id).unwrap();
                    scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
                    scene.update_placement(
                        node,
                        crate::compositor::layout::engine::Placement {
                            rect: placed.rect,
                            flow_advance: placed.rect.h,
                            bbox: placed.rect,
                        },
                    );
                }
            }
            scene
        };

        let governor = ResourceGovernor::new();
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let mut scene = laid_out_scene();
        let rejection = AnimateRejectionGuard::at(AnimateAllocationSite::BuildNotices);
        let refused = executor.block_on(AnimationRuntime::from_scene_with_prepared_wasm(
            &mut scene,
            color,
            wcfg,
            &governor,
            &std::collections::HashMap::new(),
        ));
        drop(rejection);
        assert!(
            refused.is_ok(),
            "refusing notice storage must not refuse the page"
        );

        let mut ordinary = laid_out_scene();
        assert!(
            executor
                .block_on(AnimationRuntime::from_scene_with_prepared_wasm(
                    &mut ordinary,
                    color,
                    wcfg,
                    &governor,
                    &std::collections::HashMap::new(),
                ))
                .is_ok(),
            "and the unrefused path still builds"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn frame_collection_rejection_refuses_the_runtime_and_recovers() {
        use crate::compositor::animate::{AnimateAllocationSite, AnimateRejectionGuard};

        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        // `record_resource_error` is sticky by design: a scene that failed an
        // admission is a discarded candidate, not something to retry in place.
        // So recovery is tested the way the runner does it — on a fresh scene.
        let laid_out_scene = || {
            let src = r#"[page mode=document]
[animate id="a" after="dependency"][frame][text]x[/text][/frame][/animate]
[/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            let doc = crate::parser::parse(tokens).document.unwrap();
            let mut scene = crate::compositor::scene::build::from_document(&doc);
            let layout =
                crate::compositor::layout::engine::layout_scene(&mut scene, 20, 5, color, wcfg);
            for placed in &layout.placed {
                if placed.is_animation() && !placed.rect.is_empty() {
                    let node = scene.find_by_aml_id(&placed.id).unwrap();
                    scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
                    scene.update_placement(
                        node,
                        crate::compositor::layout::engine::Placement {
                            rect: placed.rect,
                            flow_advance: placed.rect.h,
                            bbox: placed.rect,
                        },
                    );
                }
            }
            scene
        };

        let governor = ResourceGovernor::new();
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        for site in [
            AnimateAllocationSite::FrameCollection,
            AnimateAllocationSite::TransitionCollection,
            AnimateAllocationSite::Payload,
        ] {
            let mut scene = laid_out_scene();
            let rejection = AnimateRejectionGuard::at(site);
            let refused = executor.block_on(AnimationRuntime::from_scene_with_prepared_wasm(
                &mut scene,
                color,
                wcfg,
                &governor,
                &std::collections::HashMap::new(),
            ));
            assert!(refused.is_err(), "{site:?} must reject preparation");
            assert_eq!(
                governor.used(crate::resource::ResourceCategory::RemoteCollections),
                0,
                "{site:?} leaked budget"
            );
            assert!(
                scene.resource_limit_exceeded(),
                "the refused candidate must be marked, not silently reused"
            );
            drop(rejection);
            drop(scene);
        }

        let mut scene = laid_out_scene();
        let accepted = executor
            .block_on(AnimationRuntime::from_scene_with_prepared_wasm(
                &mut scene,
                color,
                wcfg,
                &governor,
                &std::collections::HashMap::new(),
            ))
            .expect("preparation must succeed once the site is disarmed");
        assert!(
            governor.used(crate::resource::ResourceCategory::RemoteCollections) > 0,
            "the recovered runtime must hold its frame and payload leases"
        );
        drop(accepted);
        assert_eq!(
            governor.used(crate::resource::ResourceCategory::RemoteCollections),
            0
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn from_scene_retains_exact_animation_frame_and_payload_leases() {
        let src = r#"[page mode=document]
[animate id="a" after="dependency"][frame][text]x[/text][/frame][/animate]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let layout =
            crate::compositor::layout::engine::layout_scene(&mut scene, 20, 5, color, wcfg);
        for placed in &layout.placed {
            if placed.is_animation() && !placed.rect.is_empty() {
                let node = scene.find_by_aml_id(&placed.id).unwrap();
                scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
                scene.update_placement(
                    node,
                    crate::compositor::layout::engine::Placement {
                        rect: placed.rect,
                        flow_advance: placed.rect.h,
                        bbox: placed.rect,
                    },
                );
            }
        }

        let governor = ResourceGovernor::new();
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let runtime = executor
            .block_on(AnimationRuntime::from_scene_with_prepared_wasm(
                &mut scene,
                color,
                wcfg,
                &governor,
                &std::collections::HashMap::new(),
            ))
            .unwrap();

        let payload_bytes = runtime
            ._animation_payload_lease
            .as_ref()
            .expect("installed adapter retains its authored id")
            .amount();
        assert_eq!(payload_bytes, "a".len() + "dependency".len());
        let retained_bytes = runtime
            .animations
            .capacity()
            .checked_mul(std::mem::size_of::<Box<dyn Animation>>())
            .unwrap()
            + std::mem::size_of::<crate::compositor::layout::cell::CellBuffer>()
            + payload_bytes;
        assert_eq!(
            governor.used(crate::resource::ResourceCategory::RemoteCollections),
            retained_bytes
        );
        drop(runtime);
        assert_eq!(
            governor.used(crate::resource::ResourceCategory::RemoteCollections),
            0
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn animation_snapshot_payload_rejection_rolls_back_without_installing_adapters() {
        let src = r#"[page mode=document]
[animate id="a"][frame][text]x[/text][/frame][/animate]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let layout =
            crate::compositor::layout::engine::layout_scene(&mut scene, 20, 5, color, wcfg);
        for placed in &layout.placed {
            if placed.is_animation() && !placed.rect.is_empty() {
                let node = scene.find_by_aml_id(&placed.id).unwrap();
                scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
                scene.update_placement(
                    node,
                    crate::compositor::layout::engine::Placement {
                        rect: placed.rect,
                        flow_advance: placed.rect.h,
                        bbox: placed.rect,
                    },
                );
            }
        }

        let governor = ResourceGovernor::new();
        let animation_vector_bytes = 2 * std::mem::size_of::<Box<dyn Animation>>();
        let snapshot_structural_bytes = std::mem::size_of::<AnimationNodeSnapshot>();
        let admitted_without_snapshot_payload = animation_vector_bytes
            .checked_add(snapshot_structural_bytes)
            .unwrap();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY - admitted_without_snapshot_payload,
            )
            .unwrap();

        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = executor.block_on(AnimationRuntime::from_scene_with_prepared_wasm(
            &mut scene,
            color,
            wcfg,
            &governor,
            &std::collections::HashMap::new(),
        ));

        assert!(matches!(result, Err(AnimationPreparationRejected)));
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            blocker.amount()
        );
        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn unused_wasm_snapshot_strings_are_released_after_construction() {
        let src = r#"[page mode=screen cols=2 rows=1]
[animate id="fx" w=2 h=1 src="/missing.wasm"/]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let layout = crate::compositor::layout::engine::layout_scene(&mut scene, 2, 1, color, wcfg);
        for placed in &layout.placed {
            if placed.is_animation() && !placed.rect.is_empty() {
                let node = scene.find_by_aml_id(&placed.id).unwrap();
                scene.ensure_buffer(node, placed.rect.w, placed.rect.h);
                scene.update_placement(
                    node,
                    crate::compositor::layout::engine::Placement {
                        rect: placed.rect,
                        flow_advance: placed.rect.h,
                        bbox: placed.rect,
                    },
                );
            }
        }

        let governor = ResourceGovernor::new();
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let runtime = executor
            .block_on(AnimationRuntime::from_scene_with_prepared_wasm(
                &mut scene,
                color,
                wcfg,
                &governor,
                &std::collections::HashMap::new(),
            ))
            .unwrap();

        assert!(runtime.animations.is_empty());
        assert!(runtime._animation_payload_lease.is_none());
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            runtime.animations.capacity() * std::mem::size_of::<Box<dyn Animation>>()
        );
        drop(runtime);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn panel_transition_collection_is_preadmitted_and_released() {
        let src = r#"[page mode=document]
[panel id="p" state="a"][state name="a"][text]A[/text][/state][/panel]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let governor = ResourceGovernor::new();
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let runtime = executor
            .block_on(AnimationRuntime::from_scene_with_prepared_wasm(
                &mut scene,
                crate::color::ColorSupport::Truecolor,
                crate::compositor::layout::text::WidthConfig::default(),
                &governor,
                &std::collections::HashMap::new(),
            ))
            .unwrap();

        assert_eq!(runtime.transition_animations.capacity(), 1);
        let expected = runtime.animations.capacity() * std::mem::size_of::<Box<dyn Animation>>()
            + runtime.transition_animations.capacity() * std::mem::size_of::<TransitionAdapter>();
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), expected);
        drop(runtime);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }
}
