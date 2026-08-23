//! WASM-driven animation adapter.
//!
//! Owns a `WasmInstance` whose `HostState.output` is an owned
//! `CellBuffer`. At tick entry, `advance` swaps that buffer with the
//! scene node's `wasm_buffer_mut(node)` via `std::mem::swap`; calls
//! `instance.tick(frame)` which writes through the swapped-in buffer;
//! swaps back. Net effect: the scene node's buffer contains the WASM
//! module's output at tick end, with zero extra copies and no
//! lifetime propagation through `wasmi::Store<'a>`.
//!
//! `mem::swap` achieves Invariant 2b (one subsystem writes the
//! scene buffer) with zero extra copies and no lifetime propagation
//! through `wasmi::Store`.

use std::fmt::{self, Write as _};
use std::io::Read as _;
use std::time::Instant;

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::scene::{NodeId, Scene};
use crate::compositor::wasm::{WasmInstance, WasmRuntime};
use crate::parser::ast::LoopBehavior;

/// Tick interval in milliseconds (~30fps). The page-transition and
/// animation subsystems use this as their timer period.
pub const TICK_MS: u64 = 33;

struct NoticeWriter {
    value: String,
    limit: usize,
}

impl fmt::Write for NoticeWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self
            .value
            .len()
            .checked_add(value.len())
            .ok_or(fmt::Error)?;
        if end > self.limit {
            return Err(fmt::Error);
        }
        self.value.push_str(value);
        Ok(())
    }
}

pub(crate) fn try_format_notice(limit: usize, args: fmt::Arguments<'_>) -> Option<String> {
    #[cfg(test)]
    if REJECT_NOTICE_ALLOCATION.with(std::cell::Cell::get) {
        return None;
    }
    let mut value = String::new();
    value.try_reserve_exact(limit).ok()?;
    let mut writer = NoticeWriter { value, limit };
    writer.write_fmt(args).ok()?;
    Some(writer.value)
}

#[cfg(test)]
thread_local! {
    static REJECT_NOTICE_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn load_wasm_bytes(
    region: Rect,
    bytes: &[u8],
    runtime: &WasmRuntime,
    content_buf: Option<CellBuffer>,
) -> Result<(WasmInstance, u32), crate::compositor::wasm::WasmError> {
    let module = runtime.compile(bytes)?;
    let mut instance = runtime.instantiate(&module, region.w, region.h, content_buf)?;
    let frame_count = instance.init(region.w, region.h)?;
    Ok((instance, frame_count))
}

/// Load a WASM animation from an explicitly local filesystem root.
pub(crate) fn load_wasm_from_file(
    region: Rect,
    src_path: &str,
    base_dir: &std::path::Path,
    runtime: &WasmRuntime,
    content_buf: Option<CellBuffer>,
) -> Result<(WasmInstance, u32), crate::compositor::wasm::WasmError> {
    let relative = src_path.trim_start_matches('/');
    let base = base_dir.as_os_str();
    let separator = usize::from(
        !base
            .as_encoded_bytes()
            .ends_with(std::path::MAIN_SEPARATOR_STR.as_bytes()),
    );
    let path_capacity = base
        .as_encoded_bytes()
        .len()
        .checked_add(separator)
        .and_then(|size| size.checked_add(relative.len()))
        .ok_or(crate::compositor::wasm::WasmError::ResourceRejected)?;
    let mut path = std::ffi::OsString::new();
    path.try_reserve_exact(path_capacity)
        .map_err(|_| crate::compositor::wasm::WasmError::ResourceRejected)?;
    path.push(base);
    if separator != 0 {
        path.push(std::path::MAIN_SEPARATOR_STR);
    }
    path.push(relative);

    let mut file = std::fs::File::open(std::path::PathBuf::from(path))
        .map_err(crate::compositor::wasm::WasmError::Read)?;
    let metadata = file
        .metadata()
        .map_err(crate::compositor::wasm::WasmError::Read)?;
    let expected = usize::try_from(metadata.len())
        .map_err(|_| crate::compositor::wasm::WasmError::ModuleTooLarge(usize::MAX))?;
    let max = crate::protocol::MAX_WASM_MODULE_SIZE;
    if expected > max {
        return Err(crate::compositor::wasm::WasmError::ModuleTooLarge(expected));
    }
    let allocation = expected
        .checked_add(1)
        .ok_or(crate::compositor::wasm::WasmError::ResourceRejected)?;
    #[cfg(test)]
    if REJECT_LOCAL_WASM_READ_ALLOCATION.with(|reject| reject.replace(false)) {
        return Err(crate::compositor::wasm::WasmError::ResourceRejected);
    }
    let mut wasm_bytes = Vec::new();
    wasm_bytes
        .try_reserve_exact(allocation)
        .map_err(|_| crate::compositor::wasm::WasmError::ResourceRejected)?;
    wasm_bytes.resize(allocation, 0);
    let mut filled = 0;
    while filled < allocation {
        let Some(unfilled) = wasm_bytes.get_mut(filled..) else {
            break;
        };
        match file.read(unfilled) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(crate::compositor::wasm::WasmError::Read(error)),
        }
    }
    if filled > expected {
        return Err(crate::compositor::wasm::WasmError::ModuleTooLarge(filled));
    }
    wasm_bytes.truncate(filled);
    load_wasm_bytes(region, &wasm_bytes, runtime, content_buf)
}

#[cfg(test)]
thread_local! {
    static REJECT_LOCAL_WASM_READ_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
use super::runtime::AnimState;
use super::{
    AdvanceCtx, AdvanceResult, Animation, AnimationResizeCandidate, AnimationResizeRejected,
};

pub struct WasmAnimationAdapter {
    id: String,
    node: NodeId,
    instance: WasmInstance,
    fps: u8,
    loop_behavior: LoopBehavior,
    state: AnimState,
    current_frame: u32,
    last_advance: Instant,
    delay_ms: u32,
    after: Option<String>,
    started_at: Option<Instant>,
    frame_count: u32,
    loops_done: u32,
    background: bool,
    autoplay: bool,
    /// A user-facing notice pending delivery to client chrome (set when this
    /// effect is stopped by a WASM resource limit). Drained by
    /// `advance_with_scene` into the `AdvanceResult`.
    pending_notice: Option<String>,
    /// Scratch buffer used for the mem::swap at tick entry/exit. Sized
    /// once at construction to match the scene node's rect.
    /// (Reserved for future use when ticks happen outside `tick_with_scene`.)
    #[allow(dead_code)]
    swap_buf: CellBuffer,
}

impl WasmAnimationAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        id: String,
        node: NodeId,
        instance: WasmInstance,
        fps: u8,
        loop_behavior: LoopBehavior,
        delay_ms: u32,
        after: Option<String>,
        autoplay: bool,
        background: bool,
        frame_count: u32,
        region_w: u16,
        region_h: u16,
    ) -> Result<Self, crate::compositor::layout::cell::BufferLimitExceeded> {
        let initial_state = if delay_ms > 0 || after.is_some() || !autoplay {
            AnimState::Waiting
        } else {
            AnimState::Running
        };
        let governor = instance.buffer_governor();
        if super::reject_animate_allocation(super::AnimateAllocationSite::WasmSwapBuffer) {
            return Err(crate::compositor::layout::cell::BufferLimitExceeded {
                width: region_w.max(1),
                height: region_h.max(1),
            });
        }
        let swap_buf = CellBuffer::try_new_governed(
            region_w.max(1),
            region_h.max(1),
            &governor,
            crate::resource::ResourceCategory::CompositorCells,
        )
        .map_err(|_| crate::compositor::layout::cell::BufferLimitExceeded {
            width: region_w.max(1),
            height: region_h.max(1),
        })?;
        Ok(Self {
            id,
            node,
            instance,
            fps: fps.max(1),
            loop_behavior,
            state: initial_state,
            current_frame: 0,
            last_advance: Instant::now(),
            delay_ms,
            after,
            started_at: None,
            frame_count,
            loops_done: 0,
            background,
            autoplay,
            pending_notice: None,
            swap_buf,
        })
    }

    pub fn node(&self) -> NodeId {
        self.node
    }
    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }
    pub fn current_frame(&self) -> u32 {
        self.current_frame
    }
}

impl WasmAnimationAdapter {
    /// Perform the mem::swap dance: bring the scene node's buffer into
    /// `HostState.output`, tick the WASM module (which writes cells
    /// through the host functions into that buffer), swap back so the
    /// scene buffer owns the tick's output.
    ///
    /// Called by `advance` when the animation is Running and the
    /// fps gate has elapsed. Returns `Ok(finished)` from the WASM
    /// tick; on error (module trap, fuel exhaustion) the animation
    /// is forced to Finished.
    fn tick_through_scene(
        &mut self,
        scene: &mut Scene,
    ) -> Result<bool, crate::compositor::wasm::WasmError> {
        // The swap source must be the HostState.output, which lives
        // inside `self.instance`. Use the instance's public take/set
        // helpers (added to WasmInstance in this phase).
        let Some(scene_buf) = scene.wasm_buffer_mut(self.node) else {
            // Scene node missing buffer — can't honor 2b; fall back to
            // ticking in place. The scene will just not reflect this
            // animation's output this tick.
            return self.instance.tick(self.current_frame);
        };
        // mem::swap: scene_buf <-> instance.output
        self.instance.swap_output(scene_buf);
        let result = self.instance.tick(self.current_frame);
        self.instance.swap_output(scene_buf);
        result
    }
}

impl Animation for WasmAnimationAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn advance(&mut self, _ctx: &mut AdvanceCtx) -> AdvanceResult {
        // No-op when called without scene access. The runtime always
        // calls `advance_with_scene` for WasmAnimationAdapter — this
        // `advance` exists only to satisfy the trait.
        AdvanceResult::none()
    }

    fn advance_with_scene(&mut self, ctx: &mut AdvanceCtx, scene: &mut Scene) -> AdvanceResult {
        let changed = self.tick_with_scene(ctx, scene);
        let mut result = if changed {
            AdvanceResult::with_buffer(self.node)
        } else {
            AdvanceResult::none()
        };
        result.notice = self.pending_notice.take();
        result
    }

    fn finished(&self) -> bool {
        matches!(self.state, AnimState::Finished)
    }

    fn state(&self) -> AnimState {
        self.state
    }

    fn background(&self) -> bool {
        self.background
    }

    fn fast_forwardable(&self) -> bool {
        // An animation nobody has started yet has no run to fast-forward, and
        // starting one here is not free. A guest may carry state across runs:
        // the title lifecycle counts them, materialising the logo on its first
        // run and atomising it on every one after, which is how a single
        // module serves both the page-load reveal and the `defer` exit. Frame
        // zero is what advances that count, so a skip that executes it unasked
        // spends the entrance and promotes the author's real start to the
        // exit — `f` made the DUSTNET title leave instead of arrive.
        //
        // Leaving it waiting costs nothing: fast-forward flushes the authored
        // delays too, so the `animate` that starts it fires in the same
        // cascade and the next pass skips the real run to its final frame.
        if !self.autoplay && self.started_at.is_none() {
            return false;
        }
        // A non-zero init result gives the guest an authored terminal frame.
        // That remains skippable even when it is composited on the background
        // layer. A zero-frame background is an endless ambient effect.
        !self.background || self.frame_count > 0
    }

    fn paints_buffer(&self) -> bool {
        // WASM writes via mem::swap inside `tick_with_scene`, so the
        // runtime doesn't need to call paint() afterward.
        false
    }

    fn memory_bytes(&self) -> usize {
        self.instance.memory_bytes()
    }

    fn next_notice_capacity_bound(&self) -> usize {
        self.pending_notice
            .as_ref()
            .map_or(0, String::capacity)
            .max(self.id.len().saturating_add(160))
    }

    fn trigger_start(&mut self, now: Instant) {
        self.state = AnimState::Running;
        self.current_frame = 0;
        self.last_advance = now;
        self.started_at = Some(now);
        self.loops_done = 0;
    }

    fn trigger_stop(&mut self) {
        self.state = AnimState::Finished;
    }

    fn skip_with_scene(&mut self, scene: &mut Scene) -> AdvanceResult {
        // Infinite guests have no authored end state. Stop them where they
        // are; foreground infinite animations should not keep `f` engaged.
        if self.frame_count == 0 {
            self.state = AnimState::Finished;
            return AdvanceResult::none();
        }

        // A guest may establish per-run state on frame zero (the reusable
        // title lifecycle effect uses this to choose materialise vs atomise).
        // If it has not advanced yet, execute frame zero before seeking.
        let mut wrote = false;
        if self.current_frame == 0 {
            match self.tick_through_scene(scene) {
                Ok(_) => wrote = true,
                Err(e) => eprintln!(
                    "WASM animation '{}' trapped while starting skip: {e}",
                    self.id
                ),
            }
        }

        self.current_frame = self.frame_count.saturating_sub(1);
        if self.current_frame > 0 {
            match self.tick_through_scene(scene) {
                Ok(_) => wrote = true,
                Err(e) => eprintln!("WASM animation '{}' trapped while skipping: {e}", self.id),
            }
        }
        self.state = AnimState::Finished;

        if wrote {
            AdvanceResult::with_buffer(self.node)
        } else {
            AdvanceResult::none()
        }
    }

    fn prepare_resize(
        &self,
        scene: &Scene,
    ) -> Result<Option<AnimationResizeCandidate>, AnimationResizeRejected> {
        let Some(rect) = scene.get(self.node).map(|node| node.placement().rect) else {
            return Ok(None);
        };
        let width = rect.w.max(1);
        let height = rect.h.max(1);
        if self.swap_buf.width == width && self.swap_buf.height == height {
            return Ok(None);
        }

        let governor = self.instance.buffer_governor();
        if super::reject_animate_allocation(super::AnimateAllocationSite::WasmSwapBuffer) {
            return Err(AnimationResizeRejected);
        }
        let swap_buffer = CellBuffer::try_new_governed(
            width,
            height,
            &governor,
            crate::resource::ResourceCategory::CompositorCells,
        )
        .map_err(|_| AnimationResizeRejected)?;
        let output_buffer = CellBuffer::try_new_governed(
            width,
            height,
            &governor,
            crate::resource::ResourceCategory::CompositorCells,
        )
        .map_err(|_| AnimationResizeRejected)?;
        Ok(Some(AnimationResizeCandidate {
            width,
            height,
            swap_buffer,
            output_buffer,
        }))
    }

    fn commit_resize(&mut self, _scene: &mut Scene, candidate: AnimationResizeCandidate) {
        let AnimationResizeCandidate {
            width,
            height,
            swap_buffer,
            output_buffer,
        } = candidate;
        let resize_result = self
            .instance
            .resize_with_output(width, height, output_buffer);
        if resize_result.is_ok() {
            self.swap_buf = swap_buffer;
        }
        if let Err(error) = resize_result {
            self.state = AnimState::Finished;
            self.pending_notice = try_format_notice(
                self.id.len().saturating_add(160),
                format_args!("animation '{}' stopped while resizing: {error}", self.id),
            );
        }
    }
}

impl WasmAnimationAdapter {
    /// Runtime-only entry for scene-aware ticking. Called by
    /// `AnimationRuntime::tick` alongside the trait `advance` — the
    /// runtime handles state transitions via a separate helper and
    /// invokes this only when a WASM tick is actually needed.
    pub fn tick_with_scene(&mut self, ctx: &AdvanceCtx, scene: &mut Scene) -> bool {
        if matches!(self.state, AnimState::Finished) {
            return false;
        }

        // Waiting → Running transitions (mirrors the frame adapter).
        if matches!(self.state, AnimState::Waiting) {
            if !self.autoplay {
                return false;
            }
            if let Some(dep) = &self.after
                && !ctx.finished_ids.iter().any(|id| id == dep)
            {
                return false;
            }
            match self.started_at {
                None => {
                    self.started_at = Some(ctx.now);
                    if self.delay_ms > 0 {
                        return false;
                    }
                }
                Some(t) => {
                    if ctx.now.duration_since(t).as_millis() < self.delay_ms as u128 {
                        return false;
                    }
                }
            }
            self.state = AnimState::Running;
        }

        if !matches!(self.state, AnimState::Running) {
            return false;
        }

        let interval_ms = 1000u128 / self.fps as u128;
        let elapsed = ctx.now.duration_since(self.last_advance).as_millis();
        if elapsed < interval_ms {
            return false;
        }
        self.last_advance = ctx.now;

        match self.tick_through_scene(scene) {
            Ok(finished) => {
                if finished {
                    match self.loop_behavior {
                        LoopBehavior::None => self.state = AnimState::Finished,
                        LoopBehavior::Infinite => self.current_frame = 0,
                        LoopBehavior::Count(max) => {
                            if self.loops_done + 1 < max {
                                self.current_frame = 0;
                                self.loops_done += 1;
                            } else {
                                self.state = AnimState::Finished;
                            }
                        }
                        LoopBehavior::Bounce => {
                            // WASM bounces are author-controlled; for
                            // the adapter, treat as infinite loop.
                            self.current_frame = 0;
                        }
                    }
                } else {
                    self.current_frame += 1;
                }
                true
            }
            Err(e) => {
                // A memory-limit kill is an abuse signal the user should see,
                // not a silent stop — surface it via client chrome. Other
                // traps are effect bugs; log them to stderr as before.
                if matches!(
                    e,
                    crate::compositor::wasm::WasmError::MemoryLimit
                        | crate::compositor::wasm::WasmError::TableLimit
                ) {
                    self.pending_notice = try_format_notice(
                        self.id.len().saturating_add(160),
                        format_args!("effect '{}' stopped: {e}", self.id),
                    );
                }
                eprintln!("WASM animation '{}' stopped: {e}", self.id);
                self.state = AnimState::Finished;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::layout::cell::{CellBuffer, CellStyle};
    use crate::compositor::scene;

    fn animation_scene() -> (Scene, NodeId) {
        let src = r#"[page mode=document]
            [animate id="type" w=10 h=1 src="/effects/typewriter.wasm"]
                [pre]Hello[/pre]
            [/animate]
        [/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = scene::build::from_document(&doc);
        let node = scene.find_by_aml_id("type").unwrap();
        scene.allocate_buffer(node, 10, 1);
        (scene, node)
    }

    #[test]
    fn initial_state_respects_autoplay() {
        // We can't construct a real WasmInstance without bytes, so this
        // test just documents the state-selection rule for the adapter's
        // constructor. See integration tests in src/render/animation.rs
        // for end-to-end verification.
        //
        // Logic covered: delay_ms > 0 || after.is_some() || !autoplay
        //                → initial state is Waiting
        //                else → Running
        //
        // Explicit test of the logic lives in frame.rs
        // (`autoplay_starts_immediately_without_delay`,
        // `non_autoplay_waits_for_trigger`).
    }

    #[test]
    fn wasm_notice_formatting_is_bounded_and_fallible() {
        let notice = try_format_notice(32, format_args!("effect '{}' stopped", "x")).unwrap();
        assert_eq!(notice, "effect 'x' stopped");
        assert!(try_format_notice(4, format_args!("too long")).is_none());

        REJECT_NOTICE_ALLOCATION.with(|rejected| rejected.set(true));
        assert!(try_format_notice(32, format_args!("effect stopped")).is_none());
        REJECT_NOTICE_ALLOCATION.with(|rejected| rejected.set(false));
    }

    #[test]
    fn local_wasm_read_allocation_rejection_is_recoverable_and_bounded() {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/site/effects");
        let runtime = WasmRuntime::new();
        REJECT_LOCAL_WASM_READ_ALLOCATION.with(|reject| reject.set(true));
        let rejected = load_wasm_from_file(
            Rect::new(0, 0, 10, 1),
            "typewriter.wasm",
            &fixture_dir,
            &runtime,
            None,
        );
        assert!(matches!(
            rejected,
            Err(crate::compositor::wasm::WasmError::ResourceRejected)
        ));

        let accepted = load_wasm_from_file(
            Rect::new(0, 0, 10, 1),
            "typewriter.wasm",
            &fixture_dir,
            &runtime,
            None,
        );
        assert!(accepted.is_ok());
    }

    #[test]
    fn skip_renders_finite_wasm_final_frame() {
        let (mut scene, node) = animation_scene();
        let mut content = CellBuffer::new(10, 1);
        content.put_str(0, 0, "Hello", &CellStyle::default());

        let bytes = include_bytes!("../../../../../tests/fixtures/site/effects/typewriter.wasm");
        let runtime = WasmRuntime::new();
        let module = runtime.compile(bytes).expect("compile typewriter fixture");
        let mut instance = runtime
            .instantiate(&module, 10, 1, Some(content))
            .expect("instantiate typewriter fixture");
        let frame_count = instance.init(10, 1).expect("init typewriter fixture");
        assert!(frame_count > 1);

        let mut adapter = WasmAnimationAdapter::try_new(
            "type".into(),
            node,
            instance,
            15,
            LoopBehavior::None,
            0,
            None,
            false,
            true,
            frame_count,
            10,
            1,
        )
        .unwrap();
        assert!(
            !adapter.fast_forwardable(),
            "an animation waiting for its authored trigger has no run to fast-forward yet"
        );
        // Skippability here is about the *background* distinction — a finite
        // cinematic settles, an endless ambient one does not — so start the
        // run the author would have started before asserting it.
        adapter.trigger_start(Instant::now());
        assert!(
            adapter.fast_forwardable(),
            "finite background cinematic must be skippable"
        );
        let result = adapter.skip_with_scene(&mut scene);

        assert!(adapter.finished());
        assert_eq!(result.wrote_buffer, Some(node));
        let rendered: String = (0..5)
            .map(|x| scene.buffer_of(node).unwrap().get(x, 0).unwrap().ch)
            .collect();
        assert_eq!(rendered, "Hello");
    }

    #[test]
    fn resize_reaccounts_exact_retained_wasm_cell_storage() {
        use crate::resource::{ResourceCategory, ResourceGovernor};

        let (mut scene, node) = animation_scene();
        let governor = ResourceGovernor::new();
        let content =
            CellBuffer::try_new_governed(10, 1, &governor, ResourceCategory::CompositorCells)
                .unwrap();
        let bytes = include_bytes!("../../../../../tests/fixtures/site/effects/typewriter.wasm");
        let runtime = WasmRuntime::with_governor(governor.clone());
        let module = runtime.compile(bytes).expect("compile typewriter fixture");
        let instance = runtime
            .instantiate(&module, 10, 1, Some(content))
            .expect("instantiate typewriter fixture");
        let cell_bytes = std::mem::size_of::<crate::compositor::layout::cell::Cell>();
        let mut adapter = WasmAnimationAdapter::try_new(
            "type".into(),
            node,
            instance,
            15,
            LoopBehavior::None,
            0,
            None,
            true,
            false,
            1,
            10,
            1,
        )
        .unwrap();

        let mut placement = *scene.get(node).unwrap().placement();
        placement.rect.w = 20;
        placement.rect.h = 2;
        scene.update_placement(node, placement);
        let candidate = adapter.prepare_resize(&scene).unwrap().unwrap();
        adapter.commit_resize(&mut scene, candidate);
        // Ten retained content cells plus 40 output and 40 swap cells.
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            90 * cell_bytes
        );

        placement.rect.w = 5;
        placement.rect.h = 1;
        scene.update_placement(node, placement);
        let candidate = adapter.prepare_resize(&scene).unwrap().unwrap();
        adapter.commit_resize(&mut scene, candidate);
        // Content remains ten cells; output and swap shrink to five each.
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            20 * cell_bytes
        );
    }

    #[test]
    fn resize_pressure_preserves_adapter_and_exact_storage_until_retry() {
        use crate::resource::{ResourceCategory, ResourceGovernor};

        let (mut scene, node) = animation_scene();
        let governor = ResourceGovernor::new();
        let content =
            CellBuffer::try_new_governed(10, 1, &governor, ResourceCategory::CompositorCells)
                .unwrap();
        let bytes = include_bytes!("../../../../../tests/fixtures/site/effects/typewriter.wasm");
        let runtime = WasmRuntime::with_governor(governor.clone());
        let module = runtime.compile(bytes).unwrap();
        let instance = runtime.instantiate(&module, 10, 1, Some(content)).unwrap();
        let mut adapter = WasmAnimationAdapter::try_new(
            "type".into(),
            node,
            instance,
            15,
            LoopBehavior::None,
            0,
            None,
            true,
            false,
            1,
            10,
            1,
        )
        .unwrap();
        let baseline = governor.total_used();
        let cell_bytes = std::mem::size_of::<crate::compositor::layout::cell::Cell>();
        let one_candidate = 40 * cell_bytes;
        let blocker = governor
            .reserve(
                ResourceCategory::CompositorCells,
                crate::resource::MAX_REMOTE_MEMORY - baseline - one_candidate,
            )
            .unwrap();
        let mut placement = *scene.get(node).unwrap().placement();
        placement.rect.w = 20;
        placement.rect.h = 2;
        scene.update_placement(node, placement);

        // Budget pressure refuses whichever buffer the governor reaches
        // first. Naming the site refuses the swap buffer specifically, which
        // is the one whose old storage has to survive the failed candidate.
        {
            let _rejection = crate::compositor::animate::AnimateRejectionGuard::at(
                crate::compositor::animate::AnimateAllocationSite::WasmSwapBuffer,
            );
            assert!(adapter.prepare_resize(&scene).is_err());
            assert_eq!((adapter.swap_buf.width, adapter.swap_buf.height), (10, 1));
            assert!(adapter.pending_notice.is_none());
        }

        assert!(adapter.prepare_resize(&scene).is_err());
        assert_eq!((adapter.swap_buf.width, adapter.swap_buf.height), (10, 1));
        assert_eq!(
            (
                adapter.instance.buffer().width,
                adapter.instance.buffer().height
            ),
            (10, 1)
        );
        assert!(!adapter.finished());
        assert!(adapter.pending_notice.is_none());
        assert_eq!(governor.total_used(), baseline + blocker.amount());

        drop(blocker);
        let candidate = adapter.prepare_resize(&scene).unwrap().unwrap();
        adapter.commit_resize(&mut scene, candidate);
        assert_eq!((adapter.swap_buf.width, adapter.swap_buf.height), (20, 2));
        assert_eq!(
            (
                adapter.instance.buffer().width,
                adapter.instance.buffer().height
            ),
            (20, 2)
        );
    }
}
