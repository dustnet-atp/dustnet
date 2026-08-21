//! Sandboxed WASM execution for procedural animation effects.
//!
//! This file is the **authoritative WASM ABI reference** for the Dustnet reference client.
//! `docs/spec/04-rendering.md` specifies the *semantics* of WASM animation
//! host operations; the byte-exact calling convention — function
//! signatures, argument packing, return encoding, resource constants —
//! is documented here so that any module compiled against this ABI
//! runs unmodified on a sibling client that matches it.
//!
//! # Guest exports
//!
//! A conforming guest module exports:
//! - `init(w: i32, h: i32) -> i32` — called once after instantiation.
//!   Returns a frame count (for frame-based animations) or `0` for
//!   continuous animations. Given 10× the normal fuel budget.
//! - `tick(frame: i32) -> i32` — called per render tick. Returns
//!   a status code (0 = running, nonzero = finished). Writes cells
//!   via host imports during the call.
//! - `resize(w: i32, h: i32)` — optional. Called when the region's
//!   dimensions change.
//!
//! # Host imports
//!
//! The host provides exactly six functions in the `env` module:
//!
//! | Name | Signature | Returns | Notes |
//! |---|---|---|---|
//! | `set_cell` | `(x, y, codepoint, fg, bg, style: i32) -> ()` | — | Writes at region-relative `(x, y)`. Out-of-bounds silently discarded. |
//! | `clear` | `() -> ()` | — | Resets every cell in the region to absent. |
//! | `get_width` | `() -> i32` | region width in cells | |
//! | `get_height` | `() -> i32` | region height in cells | |
//! | `random` | `() -> i32` | pseudo-random `u32` as `i32` | splitmix64 under the hood. |
//! | `get_content_cell` | `(x, y: i32) -> i64` | packed content cell, or `0` | See packing below. |
//!
//! Adding or omitting any of these makes the host non-conforming.
//!
//! # Color encoding (packed `u32`, passed in `i32` params)
//!
//! High byte is a tag selecting the color space:
//!
//! - `0x00000000` — **absent / transparent** (cell passes through).
//! - `0x01RRGGBB` — **RGB truecolor**. `R`, `G`, `B` in bits 16–23,
//!   8–15, 0–7 respectively.
//! - `0x020000NN` — **named color**. `NN` is a `NamedColor` index.
//!
//! Other tag bytes are reserved and decode as transparent.
//!
//! # Style encoding (bitfield `u32`, passed in an `i32` param)
//!
//! Six flags at bit positions 0–5:
//!
//! - bit 0 = bold
//! - bit 1 = italic
//! - bit 2 = underline
//! - bit 3 = strikethrough
//! - bit 4 = dim
//! - bit 5 = blink
//!
//! Bits 6+ are reserved and MUST be zero.
//!
//! # `get_content_cell` return encoding (packed `i64`)
//!
//! Returns `0` when the cell is absent, out of bounds, or there is
//! no underlying content buffer. Otherwise:
//!
//! - bits 0–20 (21 bits): Unicode codepoint.
//! - bits 21–52 (32 bits): foreground color in the packing above.
//! - bits 53–58 (6 bits): style flags.
//!
//! Background color is not returned; readers that need it must
//! track it via other means (typically not needed, since
//! `get_content_cell` is used for effects that transform the
//! foreground glyph of an existing content buffer).
//!
//! # Resource limits (current constants; subject to minor-version bumps)
//!
//! - **Per-tick fuel budget**: `100_000` instructions for an 80×24
//!   region, scaling linearly with region area:
//!   `max(100_000, 100_000 × width × height / (80 × 24))`.
//! - **`init` fuel budget**: 10× the per-tick budget.
//! - **Linear memory cap**: 64 wasm pages × 64 KiB = 4 MiB, enforced by a
//!   [`wasmi::ResourceLimiter`] on both the initial reservation and every
//!   `memory.grow`. A module that declares or grows past the cap is denied:
//!   an oversized initial reservation fails instantiation, and a runtime
//!   grow returns -1 (aborting the guest allocator). Both surface as
//!   [`WasmError::MemoryLimit`].
//! - **Tables**: at most 4 tables with 10,000 reference elements each,
//!   enforced on initial reservations and every `table.grow`.
//! - **Fuel metering**: enabled at engine configuration time; a module that
//!   runs out of fuel traps and the animation is stopped. Writes performed so
//!   far during the failed tick are retained.
//!
//! The area-linear fuel rule is normative per `04-rendering.md`.
//! The specific `100_000` base and `64`-page memory cap are this
//! implementation's chosen constants — a sibling client MAY pick
//! different numbers so long as it preserves the scaling rule.

use crate::color::{NamedColor, ResolvedColor};
use crate::compositor::layout::cell::{Cell, CellBuffer, CellStyle};
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

/// Base fuel (instruction steps) per tick call for an 80×24 canvas.
/// Scales linearly with canvas area for larger terminals.
const FUEL_BASE: u64 = 100_000;
const FUEL_BASE_AREA: u64 = 80 * 24;

/// Scale fuel budget linearly with canvas area so larger terminals don't starve.
fn fuel_for_area(width: u16, height: u16) -> u64 {
    let area = width as u64 * height as u64;
    FUEL_BASE.max(FUEL_BASE * area / FUEL_BASE_AREA)
}

/// Maximum WASM module size before compilation (512 KiB).
const MAX_MODULE_SIZE: usize = crate::protocol::MAX_WASM_MODULE_SIZE;

/// Size of a single WASM linear-memory page (64 KiB).
const WASM_PAGE_SIZE: usize = 64 * 1024;

/// Maximum WASM linear memory: 4 MiB = 64 wasm pages (64 KiB each).
///
/// Enforced by [`HostState`]'s [`wasmi::ResourceLimiter`] impl, which the
/// engine consults on both the initial memory reservation (at instantiation)
/// and every `memory.grow`. The largest committed effect declares roughly 33
/// pages, so this leaves headroom. The parser and runtime admit at most sixteen
/// WASM regions, enforcing a 64 MiB aggregate linear-memory ceiling.
const MAX_MEMORY_PAGES: u32 = 64;

/// Maximum number of reference elements in any one WASM table.
const MAX_TABLE_ELEMENTS: u32 = 10_000;

/// Maximum number of tables and linear memories owned by one guest store.
const MAX_TABLES: usize = 4;
const MAX_MEMORIES: usize = 1;

/// Shared WASM engine with fuel metering enabled.
pub struct WasmRuntime {
    engine: wasmi::Engine,
    governor: ResourceGovernor,
}

impl Default for WasmRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmRuntime {
    pub fn new() -> Self {
        Self::with_governor(ResourceGovernor::new())
    }

    pub fn with_governor(governor: ResourceGovernor) -> Self {
        let mut config = wasmi::Config::default();
        config.consume_fuel(true);
        WasmRuntime {
            engine: wasmi::Engine::new(&config),
            governor,
        }
    }

    /// Compile a WASM module from raw bytes.
    pub fn compile(&self, wasm_bytes: &[u8]) -> Result<wasmi::Module, WasmError> {
        if wasm_bytes.len() > MAX_MODULE_SIZE {
            return Err(WasmError::ModuleTooLarge(wasm_bytes.len()));
        }
        wasmi::Module::new(&self.engine, wasm_bytes).map_err(|e| WasmError::Compile(e.to_string()))
    }

    /// Instantiate a compiled module with host imports and initial state.
    pub fn instantiate(
        &self,
        module: &wasmi::Module,
        width: u16,
        height: u16,
        content: Option<CellBuffer>,
    ) -> Result<WasmInstance, WasmError> {
        let wasm_lease = self
            .governor
            .reserve(ResourceCategory::Wasm, 0)
            .map_err(|_| WasmError::ResourceRejected)?;
        let host_state = HostState {
            output: CellBuffer::try_new_governed(
                width,
                height,
                &self.governor,
                ResourceCategory::CompositorCells,
            )
            .map_err(|_| WasmError::ResourceRejected)?,
            content,
            width,
            height,
            rng_state: 0xDEAD_BEEF_CAFE_BABE,
            mem_max_pages: MAX_MEMORY_PAGES,
            mem_limit_hit: false,
            resource_limit_hit: false,
            table_limit_hit: false,
            wasm_lease,
        };

        let mut store = wasmi::Store::new(&self.engine, host_state);
        // Enforce the linear-memory cap on this instance's initial reservation
        // and every subsequent `memory.grow`.
        store.limiter(|state| state);
        let fuel = fuel_for_area(width, height) * 10; // extra fuel for init
        store
            .set_fuel(fuel)
            .map_err(|e| WasmError::Fuel(e.to_string()))?;

        let mut linker = wasmi::Linker::<HostState>::new(&self.engine);
        register_host_imports(&mut linker)?;

        // A module whose declared initial memory exceeds the cap is denied by
        // the limiter during reservation (inside `instantiate`) — surface that
        // as a memory-limit kill rather than a generic instantiate error.
        let pre = match linker.instantiate(&mut store, module) {
            Ok(pre) => pre,
            Err(_) if store.data().resource_limit_hit => {
                return Err(WasmError::ResourceRejected);
            }
            Err(_) if store.data().mem_limit_hit => return Err(WasmError::MemoryLimit),
            Err(_) if store.data().table_limit_hit => return Err(WasmError::TableLimit),
            Err(e) => return Err(WasmError::Instantiate(e.to_string())),
        };
        let instance = match pre.ensure_no_start(&mut store) {
            Ok(instance) => instance,
            Err(e) => return Err(WasmError::Instantiate(e.to_string())),
        };

        // Look up exports
        let init_fn = instance
            .get_typed_func::<(i32, i32), i32>(&store, "init")
            .map_err(|_| WasmError::MissingExport("init"))?;

        let tick_fn = instance
            .get_typed_func::<i32, i32>(&store, "tick")
            .map_err(|_| WasmError::MissingExport("tick"))?;

        let resize_fn = instance
            .get_typed_func::<(i32, i32), ()>(&store, "resize")
            .ok();

        Ok(WasmInstance {
            store,
            instance,
            init_fn,
            tick_fn,
            resize_fn,
        })
    }
}

/// A compiled and instantiated WASM animation module.
pub struct WasmInstance {
    store: wasmi::Store<HostState>,
    instance: wasmi::Instance,
    init_fn: wasmi::TypedFunc<(i32, i32), i32>,
    tick_fn: wasmi::TypedFunc<i32, i32>,
    resize_fn: Option<wasmi::TypedFunc<(i32, i32), ()>>,
}

impl WasmInstance {
    /// Call the guest's `init(w, h)` function. Returns the frame count (0 = infinite).
    pub fn init(&mut self, width: u16, height: u16) -> Result<u32, WasmError> {
        self.store.data_mut().mem_limit_hit = false;
        self.store.data_mut().resource_limit_hit = false;
        self.store.data_mut().table_limit_hit = false;
        let result = self
            .init_fn
            .call(&mut self.store, (width as i32, height as i32))
            .map_err(|e| self.classify_trap(e))?;
        Ok(result.max(0) as u32)
    }

    /// Call the guest's `tick(frame)` function. Returns 0 = continue, 1 = finished.
    /// Refuels the store before each call.
    pub fn tick(&mut self, frame: u32) -> Result<bool, WasmError> {
        let fuel = fuel_for_area(self.store.data().width, self.store.data().height);
        self.store
            .set_fuel(fuel)
            .map_err(|e| WasmError::Fuel(e.to_string()))?;
        // Clear the limiter flag so `classify_trap` reflects only this tick's
        // denials, not a benign denied-grow the guest survived earlier.
        self.store.data_mut().mem_limit_hit = false;
        self.store.data_mut().resource_limit_hit = false;
        self.store.data_mut().table_limit_hit = false;
        let result = self
            .tick_fn
            .call(&mut self.store, frame as i32)
            .map_err(|e| self.classify_trap(e))?;
        Ok(result != 0)
    }

    /// Map a guest trap to the right [`WasmError`]. A denied `memory.grow`
    /// makes the guest allocator abort (a trap), so consult the limiter flag
    /// to distinguish an out-of-memory kill from an ordinary trap.
    fn classify_trap(&self, e: wasmi::Error) -> WasmError {
        if self.store.data().mem_limit_hit {
            WasmError::MemoryLimit
        } else if self.store.data().resource_limit_hit {
            WasmError::ResourceRejected
        } else if self.store.data().table_limit_hit {
            WasmError::TableLimit
        } else {
            WasmError::Trap(e.to_string())
        }
    }

    /// Current linear-memory footprint of this instance in bytes.
    /// Zero if the module exports no memory named `memory`.
    pub fn memory_bytes(&self) -> usize {
        self.instance
            .get_memory(&self.store, "memory")
            .map(|m| m.data_size(&self.store))
            .unwrap_or(0)
    }

    /// Call the guest's optional `resize(w, h)` function.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), WasmError> {
        let governor = self.store.data().wasm_lease.governor();
        let replacement = CellBuffer::try_new_governed(
            width,
            height,
            &governor,
            ResourceCategory::CompositorCells,
        )
        .map_err(|_| WasmError::BufferLimit)?;
        self.resize_with_output(width, height, replacement)
    }

    /// Resize using output storage allocated by the caller's larger
    /// transaction. Once called, no host buffer admission remains.
    pub(crate) fn resize_with_output(
        &mut self,
        width: u16,
        height: u16,
        replacement: CellBuffer,
    ) -> Result<(), WasmError> {
        let old_width = self.store.data().width;
        let old_height = self.store.data().height;
        let old_output = std::mem::replace(&mut self.store.data_mut().output, replacement);
        self.store.data_mut().width = width;
        self.store.data_mut().height = height;

        if let Some(resize_fn) = self.resize_fn {
            let fuel = fuel_for_area(width, height) * 10;
            if let Err(error) = self.store.set_fuel(fuel) {
                self.store.data_mut().output = old_output;
                self.store.data_mut().width = old_width;
                self.store.data_mut().height = old_height;
                return Err(WasmError::Fuel(error.to_string()));
            }
            self.store.data_mut().mem_limit_hit = false;
            self.store.data_mut().resource_limit_hit = false;
            self.store.data_mut().table_limit_hit = false;
            if let Err(error) = resize_fn.call(&mut self.store, (width as i32, height as i32)) {
                self.store.data_mut().output = old_output;
                self.store.data_mut().width = old_width;
                self.store.data_mut().height = old_height;
                return Err(self.classify_trap(error));
            }
        }
        Ok(())
    }

    /// Get a reference to the current output buffer.
    pub fn buffer(&self) -> &CellBuffer {
        &self.store.data().output
    }

    pub(crate) fn buffer_governor(&self) -> ResourceGovernor {
        self.store.data().wasm_lease.governor()
    }

    /// Swap `HostState.output` with the provided external buffer.
    /// Used by `WasmAnimationAdapter::tick_through_scene` to lend
    /// the scene node's buffer to the WASM host for the duration of
    /// a `tick` call so `set_cell` host-fn writes land directly in
    /// the scene — Invariant 2b without lifetime propagation through
    /// `wasmi::Store`.
    pub fn swap_output(&mut self, other: &mut CellBuffer) {
        std::mem::swap(&mut self.store.data_mut().output, other);
    }
}

/// Host state accessible to WASM imported functions via Caller.
struct HostState {
    /// Output buffer — WASM writes here via set_cell().
    output: CellBuffer,
    /// Optional content buffer for effects that transform existing content.
    content: Option<CellBuffer>,
    width: u16,
    height: u16,
    /// Splitmix64 PRNG state.
    rng_state: u64,
    /// Linear-memory ceiling in wasm pages, enforced via [`wasmi::ResourceLimiter`].
    mem_max_pages: u32,
    /// Set when authored memory demand exceeded the per-guest ceiling. Lets
    /// callers distinguish a guest limit violation from shared-governor pressure.
    mem_limit_hit: bool,
    /// Set when the shared governor, rather than an authored guest limit,
    /// denied memory admission.
    resource_limit_hit: bool,
    /// Set when a table reservation or growth exceeds [`MAX_TABLE_ELEMENTS`].
    table_limit_hit: bool,
    /// Shared aggregate lease, expanded before every committed linear-memory growth.
    wasm_lease: BudgetLease,
}

impl wasmi::ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmi::errors::MemoryError> {
        let limit = self.mem_max_pages as usize * WASM_PAGE_SIZE;
        if desired > limit {
            // Deny gracefully (guest `memory.grow` sees -1; an initial
            // reservation over the cap fails instantiation) and record the
            // cause so the caller can report a memory-limit kill.
            self.mem_limit_hit = true;
            return Ok(false);
        }
        if self.wasm_lease.try_grow(desired).is_err() {
            self.resource_limit_hit = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: u32,
        desired: u32,
        _maximum: Option<u32>,
    ) -> Result<bool, wasmi::errors::TableError> {
        if desired > MAX_TABLE_ELEMENTS {
            self.table_limit_hit = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES
    }
}

/// Errors from the WASM runtime.
#[derive(Debug)]
pub enum WasmError {
    ModuleTooLarge(usize),
    Read(std::io::Error),
    Compile(String),
    Instantiate(String),
    MissingExport(&'static str),
    Trap(String),
    Fuel(String),
    /// Shared client-governor admission failed. Unlike authored guest limits,
    /// this may succeed after reducer-ordered pressure recovery.
    ResourceRejected,
    /// The module's memory reservation or growth exceeded [`MAX_MEMORY_PAGES`].
    MemoryLimit,
    /// The module's table reservation or growth exceeded [`MAX_TABLE_ELEMENTS`].
    TableLimit,
    /// A host-side cell buffer could not be admitted or allocated.
    BufferLimit,
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmError::ModuleTooLarge(size) => {
                write!(
                    f,
                    "WASM module too large: {size} bytes (max {MAX_MODULE_SIZE})"
                )
            }
            WasmError::Read(error) => write!(f, "WASM module read error: {error}"),
            WasmError::Compile(e) => write!(f, "WASM compile error: {e}"),
            WasmError::Instantiate(e) => write!(f, "WASM instantiation error: {e}"),
            WasmError::MissingExport(name) => write!(f, "WASM module missing export: {name}"),
            WasmError::Trap(e) => write!(f, "WASM trap: {e}"),
            WasmError::Fuel(e) => write!(f, "WASM fuel error: {e}"),
            WasmError::ResourceRejected => write!(f, "WASM resource admission rejected"),
            WasmError::MemoryLimit => write!(
                f,
                "WASM memory limit exceeded (max {} MiB)",
                MAX_MEMORY_PAGES as usize * WASM_PAGE_SIZE / (1024 * 1024)
            ),
            WasmError::TableLimit => write!(
                f,
                "WASM table limit exceeded (max {MAX_TABLE_ELEMENTS} elements)",
            ),
            WasmError::BufferLimit => write!(f, "WASM render buffer limit exceeded"),
        }
    }
}

impl std::error::Error for WasmError {}

// ─── Host Import Registration ───────────────────────────────

fn register_host_imports(linker: &mut wasmi::Linker<HostState>) -> Result<(), WasmError> {
    // set_cell(x, y, codepoint, fg, bg, style)
    linker
        .func_wrap(
            "env",
            "set_cell",
            |mut caller: wasmi::Caller<HostState>,
             x: i32,
             y: i32,
             codepoint: i32,
             fg: i32,
             bg: i32,
             style: i32| {
                let state = caller.data_mut();
                if x < 0 || y < 0 || x >= state.width as i32 || y >= state.height as i32 {
                    return;
                }
                let ch = char::from_u32(codepoint as u32).unwrap_or(' ');
                let cell_style = decode_style(fg as u32, bg as u32, style as u32);
                state
                    .output
                    .set(x as u16, y as u16, Cell::new(ch, cell_style));
            },
        )
        .map_err(|e| WasmError::Instantiate(e.to_string()))?;

    // clear()
    linker
        .func_wrap("env", "clear", |mut caller: wasmi::Caller<HostState>| {
            let state = caller.data_mut();
            state.output.clear_transparent();
        })
        .map_err(|e| WasmError::Instantiate(e.to_string()))?;

    // get_width() -> i32
    linker
        .func_wrap(
            "env",
            "get_width",
            |caller: wasmi::Caller<HostState>| -> i32 { caller.data().width as i32 },
        )
        .map_err(|e| WasmError::Instantiate(e.to_string()))?;

    // get_height() -> i32
    linker
        .func_wrap(
            "env",
            "get_height",
            |caller: wasmi::Caller<HostState>| -> i32 { caller.data().height as i32 },
        )
        .map_err(|e| WasmError::Instantiate(e.to_string()))?;

    // random() -> i32
    linker
        .func_wrap(
            "env",
            "random",
            |mut caller: wasmi::Caller<HostState>| -> i32 {
                let state = caller.data_mut();
                state.rng_state = splitmix64(state.rng_state);
                state.rng_state as i32
            },
        )
        .map_err(|e| WasmError::Instantiate(e.to_string()))?;

    // get_content_cell(x, y) -> i64
    // Returns 0 if cell is empty/transparent/out-of-bounds.
    // Otherwise: bits 0-20 = codepoint, bits 21-52 = fg color packed, bits 53-58 = style flags.
    linker
        .func_wrap(
            "env",
            "get_content_cell",
            |caller: wasmi::Caller<HostState>, x: i32, y: i32| -> i64 {
                let state = caller.data();
                let content = match &state.content {
                    Some(buf) => buf,
                    None => return 0,
                };
                if x < 0 || y < 0 {
                    return 0;
                }
                let cell = match content.get(x as u16, y as u16) {
                    Some(c) => c,
                    None => return 0,
                };
                if cell.ch == ' ' && cell.style == CellStyle::default() {
                    return 0;
                }
                pack_content_cell(cell)
            },
        )
        .map_err(|e| WasmError::Instantiate(e.to_string()))?;

    Ok(())
}

// ─── Color/Style Encoding Helpers ───────────────────────────

/// Decode a packed u32 color into an Option<ResolvedColor>.
fn decode_color(packed: u32) -> Option<ResolvedColor> {
    let tag = (packed >> 24) & 0xFF;
    match tag {
        0x00 => None, // transparent
        0x01 => {
            let r = ((packed >> 16) & 0xFF) as u8;
            let g = ((packed >> 8) & 0xFF) as u8;
            let b = (packed & 0xFF) as u8;
            Some(ResolvedColor::Rgb(r, g, b))
        }
        0x02 => {
            let index = (packed & 0xFF) as u8;
            named_color_from_index(index).map(ResolvedColor::Named)
        }
        _ => None,
    }
}

/// Decode a packed u32 style into CellStyle fields.
fn decode_style(fg_packed: u32, bg_packed: u32, style_bits: u32) -> CellStyle {
    CellStyle {
        fg: decode_color(fg_packed),
        bg: decode_color(bg_packed),
        bold: style_bits & (1 << 0) != 0,
        italic: style_bits & (1 << 1) != 0,
        underline: style_bits & (1 << 2) != 0,
        strikethrough: style_bits & (1 << 3) != 0,
        dim: style_bits & (1 << 4) != 0,
        blink: style_bits & (1 << 5) != 0,
    }
}

/// Encode a CellStyle's flags into a 6-bit value.
fn encode_style_flags(style: &CellStyle) -> u32 {
    let mut bits = 0u32;
    if style.bold {
        bits |= 1 << 0;
    }
    if style.italic {
        bits |= 1 << 1;
    }
    if style.underline {
        bits |= 1 << 2;
    }
    if style.strikethrough {
        bits |= 1 << 3;
    }
    if style.dim {
        bits |= 1 << 4;
    }
    if style.blink {
        bits |= 1 << 5;
    }
    bits
}

/// Encode an Option<ResolvedColor> into the packed u32 format.
fn encode_color(color: &Option<ResolvedColor>) -> u32 {
    match color {
        None => 0x00000000,
        Some(ResolvedColor::Rgb(r, g, b)) => {
            0x01000000 | ((*r as u32) << 16) | ((*g as u32) << 8) | (*b as u32)
        }
        Some(ResolvedColor::Named(named)) => 0x02000000 | named_color_index(named) as u32,
        Some(ResolvedColor::Palette(idx)) => 0x02000000 | *idx as u32,
    }
}

/// Pack a cell's content into an i64 for get_content_cell.
/// Layout: bits 0-20 = codepoint, bits 21-52 = fg packed, bits 53-58 = style flags.
fn pack_content_cell(cell: &Cell) -> i64 {
    let codepoint = (cell.ch as u32 & 0x1FFFFF) as u64;
    let fg = encode_color(&cell.style.fg) as u64;
    let style = (encode_style_flags(&cell.style) & 0x3F) as u64;
    (codepoint | (fg << 21) | (style << 53)) as i64
}

/// Map a NamedColor to a numeric index.
fn named_color_index(color: &NamedColor) -> u8 {
    match color {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
    }
}

/// Map a numeric index back to a NamedColor.
fn named_color_from_index(index: u8) -> Option<NamedColor> {
    match index {
        0 => Some(NamedColor::Black),
        1 => Some(NamedColor::Red),
        2 => Some(NamedColor::Green),
        3 => Some(NamedColor::Yellow),
        4 => Some(NamedColor::Blue),
        5 => Some(NamedColor::Magenta),
        6 => Some(NamedColor::Cyan),
        7 => Some(NamedColor::White),
        8 => Some(NamedColor::BrightBlack),
        9 => Some(NamedColor::BrightRed),
        10 => Some(NamedColor::BrightGreen),
        11 => Some(NamedColor::BrightYellow),
        12 => Some(NamedColor::BrightBlue),
        13 => Some(NamedColor::BrightMagenta),
        14 => Some(NamedColor::BrightCyan),
        15 => Some(NamedColor::BrightWhite),
        _ => None,
    }
}

/// Splitmix64 PRNG — deterministic, fast, no external crate needed.
fn splitmix64(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9e3779b97f4a7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_roundtrip_none() {
        let packed = encode_color(&None);
        assert_eq!(packed, 0);
        assert_eq!(decode_color(packed), None);
    }

    #[test]
    fn color_roundtrip_rgb() {
        let color = Some(ResolvedColor::Rgb(0xAB, 0xCD, 0xEF));
        let packed = encode_color(&color);
        assert_eq!(packed, 0x01ABCDEF);
        assert_eq!(decode_color(packed), color);
    }

    #[test]
    fn color_roundtrip_named() {
        let color = Some(ResolvedColor::Named(NamedColor::BrightGreen));
        let packed = encode_color(&color);
        assert_eq!(decode_color(packed), color);
    }

    #[test]
    fn limiter_denies_growth_past_cap_and_flags_it() {
        use wasmi::ResourceLimiter;
        let mut state = HostState {
            output: CellBuffer::new(1, 1),
            content: None,
            width: 1,
            height: 1,
            rng_state: 0,
            mem_max_pages: MAX_MEMORY_PAGES,
            mem_limit_hit: false,
            resource_limit_hit: false,
            table_limit_hit: false,
            wasm_lease: ResourceGovernor::new()
                .reserve(ResourceCategory::Wasm, 0)
                .unwrap(),
        };
        let cap_bytes = MAX_MEMORY_PAGES as usize * WASM_PAGE_SIZE;
        // Exactly at the cap is allowed; one byte over is denied and flagged.
        assert!(state.memory_growing(0, cap_bytes, None).unwrap());
        assert!(!state.mem_limit_hit);
        assert!(!state.memory_growing(0, cap_bytes + 1, None).unwrap());
        assert!(state.mem_limit_hit);
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn oversized_initial_memory_is_denied_as_memory_limit() {
        // Minimal module declaring a single 200-page (12.8 MiB) memory and
        // nothing else — over the 8 MiB cap. The limiter denies it during the
        // reservation inside `instantiate`, before export lookup, so the error
        // is a MemoryLimit, not a generic instantiate/missing-export failure.
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x05, 0x04, 0x01, 0x00, 0xC8, 0x01, // memory section: 1 mem, min=200
        ];
        let rt = WasmRuntime::new();
        let module = rt.compile(wasm).expect("valid wasm module");
        let err = match rt.instantiate(&module, 80, 24, None) {
            Ok(_) => panic!("expected instantiation to be denied by the memory cap"),
            Err(e) => e,
        };
        assert!(matches!(err, WasmError::MemoryLimit), "got {err:?}");
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn shared_governor_denial_is_distinct_from_authored_memory_limit() {
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section: 1 mem, min=1
        ];
        let governor = ResourceGovernor::new();
        let _pressure = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let rt = WasmRuntime::with_governor(governor);
        let module = rt.compile(wasm).expect("valid wasm module");
        let err = match rt.instantiate(&module, 80, 24, None) {
            Ok(_) => panic!("expected shared governor rejection"),
            Err(error) => error,
        };
        assert!(matches!(err, WasmError::ResourceRejected), "got {err:?}");
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn oversized_initial_table_is_denied_as_table_limit() {
        // Minimal module declaring one 10,001-element funcref table. The
        // limiter must reject it before wasmi allocates the backing vector.
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x04, 0x05, 0x01, 0x70, 0x00, 0x91, 0x4e, // table min=10,001
        ];
        let rt = WasmRuntime::new();
        let module = rt.compile(wasm).expect("valid wasm module");
        let err = match rt.instantiate(&module, 80, 24, None) {
            Ok(_) => panic!("expected instantiation to be denied by the table cap"),
            Err(error) => error,
        };
        assert!(matches!(err, WasmError::TableLimit), "got {err:?}");
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn runtime_grow_past_cap_is_classified_as_memory_limit() {
        // Hand-built module: memory(min 1), exports memory/init/tick.
        // `init` returns 0. `tick` does `i32.const 2000; memory.grow; drop;
        // unreachable` — the grow (2000 pages ≫ 128-page cap) is denied by the
        // limiter (setting the flag), then `unreachable` traps. The trap must
        // be classified as MemoryLimit, not a generic trap.
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            // Type section: [ (i32,i32)->i32 , (i32)->i32 ]
            0x01, 0x0c, 0x02, //
            0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f, // type0: init
            0x60, 0x01, 0x7f, 0x01, 0x7f, // type1: tick
            // Function section: func0->type0, func1->type1
            0x03, 0x03, 0x02, 0x00, 0x01, //
            // Memory section: 1 memory, min=1
            0x05, 0x03, 0x01, 0x00, 0x01, //
            // Export section (body 24 bytes): memory, init, tick
            0x07, 0x18, 0x03, //
            0x06, b'm', b'e', b'm', b'o', b'r', b'y', 0x02, 0x00, //
            0x04, b'i', b'n', b'i', b't', 0x00, 0x00, //
            0x04, b't', b'i', b'c', b'k', 0x00, 0x01, //
            // Code section (body 16 bytes): 2 bodies
            0x0a, 0x10, 0x02, //
            0x04, 0x00, 0x41, 0x00, 0x0b, // init: (locals 0) i32.const 0; end
            0x09, 0x00, // tick body: size 9, locals 0
            0x41, 0xd0, 0x0f, // i32.const 2000
            0x40, 0x00, // memory.grow (mem 0)
            0x1a, // drop
            0x00, // unreachable
            0x0b, // end
        ];
        let rt = WasmRuntime::new();
        let module = rt.compile(wasm).expect("valid wasm module");
        let mut instance = rt
            .instantiate(&module, 80, 24, None)
            .expect("small initial memory instantiates");
        assert_eq!(instance.init(80, 24).expect("init"), 0);
        let err = match instance.tick(0) {
            Ok(_) => panic!("expected the grow-then-trap tick to fail"),
            Err(e) => e,
        };
        assert!(matches!(err, WasmError::MemoryLimit), "got {err:?}");
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn within_cap_memory_is_allowed() {
        // Single 1-page memory, no exports. The limiter permits the reservation,
        // so instantiation proceeds past memory and only then fails on the
        // missing `init` export — proving the cap does not reject small modules.
        let wasm: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // header
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section: 1 mem, min=1
        ];
        let rt = WasmRuntime::new();
        let module = rt.compile(wasm).expect("valid wasm module");
        let err = match rt.instantiate(&module, 80, 24, None) {
            Ok(_) => panic!("expected instantiation to fail on the missing init export"),
            Err(e) => e,
        };
        assert!(matches!(err, WasmError::MissingExport(_)), "got {err:?}");
    }

    #[test]
    fn style_flags_roundtrip() {
        let style = CellStyle {
            bold: true,
            dim: true,
            ..Default::default()
        };
        let bits = encode_style_flags(&style);
        assert_eq!(bits & 1, 1); // bold
        assert_eq!((bits >> 4) & 1, 1); // dim
        assert_eq!((bits >> 1) & 1, 0); // italic = false
    }

    #[test]
    fn decode_style_from_packed() {
        let fg = encode_color(&Some(ResolvedColor::Named(NamedColor::Green)));
        let bg = encode_color(&None);
        let style_bits = 0b000001; // bold only
        let result = decode_style(fg, bg, style_bits);
        assert_eq!(result.fg, Some(ResolvedColor::Named(NamedColor::Green)));
        assert_eq!(result.bg, None);
        assert!(result.bold);
        assert!(!result.italic);
    }

    #[test]
    fn pack_content_cell_space_is_nonzero() {
        // pack_content_cell encodes any cell. The caller (get_content_cell host import)
        // is responsible for checking empty cells and returning 0.
        let cell = Cell::empty();
        // Space character has codepoint 32, so packed value is nonzero
        assert_eq!(pack_content_cell(&cell) & 0x1FFFFF, ' ' as i64);
    }

    #[test]
    fn pack_content_cell_char() {
        let cell = Cell::new(
            'A',
            CellStyle {
                fg: Some(ResolvedColor::Named(NamedColor::Red)),
                bold: true,
                ..Default::default()
            },
        );
        let packed = pack_content_cell(&cell);
        assert_ne!(packed, 0);
        // Extract codepoint
        let cp = (packed as u64) & 0x1FFFFF;
        assert_eq!(cp, 'A' as u64);
        // Extract style flags
        let style = ((packed as u64) >> 53) & 0x3F;
        assert_eq!(style & 1, 1); // bold
    }

    #[test]
    fn splitmix64_deterministic() {
        let a = splitmix64(42);
        let b = splitmix64(42);
        assert_eq!(a, b);
        assert_ne!(a, 42);
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn real_effect_memory_is_reported_and_under_cap() {
        // The committed typewriter fixture always exists (unlike effects/
        // build outputs). Its linear memory must be reported nonzero and sit
        // well under the cap — proof the 8 MiB limit rejects no real effect.
        let bytes = include_bytes!("../../../../tests/fixtures/site/effects/typewriter.wasm");
        let rt = WasmRuntime::new();
        let module = rt.compile(bytes).expect("compile typewriter fixture");
        let mut inst = rt.instantiate(&module, 20, 1, None).expect("instantiate");
        inst.init(20, 1).expect("init");
        let used = inst.memory_bytes();
        assert!(used > 0, "memory_bytes should report a nonzero footprint");
        let cap = MAX_MEMORY_PAGES as usize * WASM_PAGE_SIZE;
        assert!(used <= cap, "real effect uses {used} bytes, over cap {cap}");
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn runtime_creates_engine() {
        let rt = WasmRuntime::new();
        // Smoke test: compile an empty module
        // Minimal valid WASM: magic + version + no sections
        let minimal_wasm = b"\x00asm\x01\x00\x00\x00";
        let result = rt.compile(minimal_wasm);
        assert!(result.is_ok());
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn module_too_large_rejected() {
        let rt = WasmRuntime::new();
        let big = vec![0u8; MAX_MODULE_SIZE + 1];
        let result = rt.compile(&big);
        assert!(matches!(result, Err(WasmError::ModuleTooLarge(_))));
    }

    // ─── Integration tests with real WASM modules ───────────

    /// Workspace root, resolved from this crate's manifest at compile time.
    /// Test processes run with the crate directory as their working directory,
    /// so a bare `effects/...` path never resolved and every effect test
    /// silently skipped.
    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("client crate is nested under <workspace>/crates")
            .to_path_buf()
    }

    /// Helper to load a WASM module from the effects/ build directory.
    fn load_effect_wasm(name: &str) -> Option<Vec<u8>> {
        let path = workspace_root().join(format!(
            "effects/{name}/target/wasm32-unknown-unknown/release/{name}.wasm"
        ));
        std::fs::read(&path).ok()
    }

    /// Load one feature variant of an effect crate. `make effects` builds each
    /// variant into its own `target-<variant>` directory precisely so these
    /// have a stable path inside the repository; they used to be read from a
    /// site checkout, which meant they silently skipped for everyone else.
    fn load_effect_variant_wasm(krate: &str, variant: &str) -> Option<Vec<u8>> {
        let path = workspace_root().join(format!(
            "effects/{krate}/target-{variant}/wasm32-unknown-unknown/release/{krate}.wasm"
        ));
        std::fs::read(&path).ok()
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn matrix_rain_wasm_init_and_tick() {
        let wasm_bytes = match load_effect_wasm("matrix_rain") {
            Some(b) => b,
            None => {
                eprintln!("skipping: matrix_rain.wasm not built");
                return;
            }
        };

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt.instantiate(&module, 20, 10, None).expect("instantiate");

        // init should return 0 (infinite)
        let frame_count = inst.init(20, 10).expect("init");
        assert_eq!(frame_count, 0);

        // Tick several frames — should succeed
        for frame in 0..30 {
            let finished = inst.tick(frame).expect("tick");
            assert!(!finished, "matrix rain should never signal finished");
        }

        // Buffer should have content
        let buf = inst.buffer();
        assert_eq!(buf.width, 20);
        assert_eq!(buf.height, 10);

        // At least some cells should be non-space after 30 frames
        let mut non_space = 0;
        for y in 0..10 {
            for x in 0..20 {
                if let Some(cell) = buf.get(x, y) {
                    assert_eq!(
                        cell.style.bg,
                        Some(ResolvedColor::Rgb(0, 0, 0)),
                        "matrix rain must paint every background cell as literal black"
                    );
                    if cell.ch != ' ' {
                        non_space += 1;
                    }
                }
            }
        }
        assert!(
            non_space > 0,
            "matrix rain should produce visible characters"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn dense_backgrounds_fit_the_runtime_fuel_budget() {
        let effects = [
            "matrix_rain",
            "starfield",
            "plasma",
            "lava",
            "aurora",
            "vortex",
            "caustics",
            "orbitals",
            "kaleidoscope",
        ];

        for name in effects {
            let loaded = if name == "matrix_rain" {
                load_effect_wasm(name)
            } else {
                load_effect_variant_wasm("procedural_backgrounds", name)
            };
            let Some(wasm_bytes) = loaded else {
                eprintln!("skipping {name}: run `make effects` to build it");
                continue;
            };

            let rt = WasmRuntime::new();
            let module = rt
                .compile(&wasm_bytes)
                .unwrap_or_else(|e| panic!("{name} failed to compile: {e}"));
            let mut inst = rt
                .instantiate(&module, 80, 24, None)
                .unwrap_or_else(|e| panic!("{name} failed to instantiate: {e}"));
            assert_eq!(inst.init(80, 24).expect("init"), 0);

            for frame in 0..9 {
                let finished = inst
                    .tick(frame)
                    .unwrap_or_else(|e| panic!("{name} trapped on frame {frame}: {e}"));
                assert!(!finished, "{name} should be continuous");
            }

            let mut visible = 0;
            for y in 0..24 {
                for x in 0..80 {
                    if inst.buffer().get(x, y).is_some_and(|cell| cell.ch != ' ') {
                        visible += 1;
                    }
                }
            }
            assert!(visible > 0, "{name} should paint visible cells");
        }
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn matrix_rain_wasm_has_green_styled_cells() {
        let wasm_bytes = match load_effect_wasm("matrix_rain") {
            Some(b) => b,
            None => return,
        };

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt.instantiate(&module, 40, 20, None).expect("instantiate");
        inst.init(40, 20).expect("init");

        // Tick enough frames for rain to appear
        for f in 0..60 {
            inst.tick(f).unwrap();
        }

        let buf = inst.buffer();
        let mut has_green = false;
        let mut has_bold_white = false;
        for y in 0..20 {
            for x in 0..40 {
                if let Some(cell) = buf.get(x, y) {
                    if cell.ch == ' ' {
                        continue;
                    }
                    if cell.style.fg == Some(ResolvedColor::Named(NamedColor::Green))
                        || cell.style.fg == Some(ResolvedColor::Named(NamedColor::BrightGreen))
                    {
                        has_green = true;
                    }
                    if cell.style.fg == Some(ResolvedColor::Named(NamedColor::BrightWhite))
                        && cell.style.bold
                    {
                        has_bold_white = true;
                    }
                }
            }
        }
        assert!(has_green, "matrix rain should use green colors");
        assert!(
            has_bold_white,
            "matrix rain should have bright white bold heads"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn typewriter_wasm_init_and_tick() {
        let wasm_bytes = match load_effect_wasm("typewriter") {
            Some(b) => b,
            None => {
                eprintln!("skipping: typewriter.wasm not built");
                return;
            }
        };

        // Create a content buffer with some text
        let mut content = CellBuffer::new(20, 2);
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Green)),
            ..Default::default()
        };
        content.put_str(0, 0, "Hello", &style);
        content.put_str(0, 1, "World", &style);

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt
            .instantiate(&module, 20, 2, Some(content))
            .expect("instantiate");

        let frame_count = inst.init(20, 2).expect("init");
        // Should return non-zero (finite frames: 1 + 10 chars)
        assert!(
            frame_count > 0,
            "typewriter should return finite frame count, got {frame_count}"
        );

        // Frame 0: blank
        let finished = inst.tick(0).expect("tick 0");
        assert!(!finished);
        let buf = inst.buffer();
        // Should be mostly empty
        let mut visible = 0;
        for y in 0..2 {
            for x in 0..20 {
                if let Some(cell) = buf.get(x, y)
                    && cell.ch != ' '
                {
                    visible += 1;
                }
            }
        }
        assert_eq!(visible, 0, "frame 0 should be blank");

        // Tick to final frame — should be finished and show all content
        let last = frame_count - 1;
        let finished = inst.tick(last as i32 as u32).expect("tick last");
        assert!(finished, "last frame should signal finished");

        let buf = inst.buffer();
        assert_eq!(buf.get(0, 0).map(|c| c.ch), Some('H'));
        assert_eq!(buf.get(4, 0).map(|c| c.ch), Some('o'));
        assert_eq!(buf.get(0, 1).map(|c| c.ch), Some('W'));
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn typewriter_wasm_preserves_fg_color() {
        let wasm_bytes = match load_effect_wasm("typewriter") {
            Some(b) => b,
            None => return,
        };

        let mut content = CellBuffer::new(10, 1);
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Cyan)),
            bold: true,
            ..Default::default()
        };
        content.put_str(0, 0, "Hi", &style);

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt
            .instantiate(&module, 10, 1, Some(content))
            .expect("instantiate");
        let frame_count = inst.init(10, 1).expect("init");

        // Tick to the last frame
        inst.tick(frame_count - 1).expect("tick");

        let buf = inst.buffer();
        let cell = buf.get(0, 0).expect("cell at 0,0");
        assert_eq!(cell.ch, 'H');
        assert_eq!(cell.style.fg, Some(ResolvedColor::Named(NamedColor::Cyan)));
        assert!(cell.style.bold);
    }

    /// A lifecycle guest counts its runs — run one materialises the title,
    /// every run after atomises it for deferred navigation — so executing
    /// frame zero is what starts a run, not merely what draws one.
    ///
    /// Fast-forward used to execute frame zero on animations the author had
    /// never started, which spent the entrance on nothing and promoted the
    /// real `animate` that followed to the exit: pressing `f` during page load
    /// made the DUSTNET title dissolve away and never come back. An animation
    /// waiting for its trigger has no run to fast-forward.
    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn fast_forward_leaves_an_untriggered_lifecycle_its_entrance() {
        use crate::compositor::animate::{Animation, WasmAnimationAdapter};

        let Some(wasm_bytes) = load_effect_variant_wasm("content_particles", "lifecycle") else {
            eprintln!("skipping title-lifecycle fast-forward: run `make effects` to build it");
            return;
        };

        let src = r#"[page mode=document]
            [animate id="title" w=8 h=2 src="/effects/title-lifecycle.wasm" autoplay=false loop=false]
                [pre]@@@@@@@@
@@@@@@@@[/pre]
            [/animate]
        [/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let node = scene.find_by_aml_id("title").unwrap();
        scene.allocate_buffer(node, 8, 2);

        let authored = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::BrightWhite)),
            bold: true,
            ..Default::default()
        };
        let mut content = CellBuffer::new(8, 2);
        content.put_str(0, 0, "@@@@@@@@", &authored);
        content.put_str(0, 1, "@@@@@@@@", &authored);

        let runtime = WasmRuntime::new();
        let module = runtime.compile(&wasm_bytes).expect("compile");
        let mut instance = runtime
            .instantiate(&module, 8, 2, Some(content))
            .expect("instantiate");
        let frame_count = instance.init(8, 2).expect("init");

        let mut adapter = WasmAnimationAdapter::try_new(
            "title".into(),
            node,
            instance,
            24,
            crate::parser::ast::LoopBehavior::None,
            0,
            None,
            false,
            false,
            frame_count,
            8,
            2,
        )
        .expect("adapter");

        assert!(
            !adapter.fast_forwardable(),
            "an animation still waiting for its authored trigger has no run to fast-forward",
        );

        // Fast-forward flushes the authored delays too, so the `animate` that
        // starts it fires in the same cascade — and only then is there a run
        // to settle.
        adapter.trigger_start(std::time::Instant::now());
        assert!(adapter.fast_forwardable());
        adapter.skip_with_scene(&mut scene);

        let settled = scene.wasm_buffer_mut(node).expect("animation buffer");
        let resolved = (0..8)
            .filter(|&x| settled.get(x, 1).is_some_and(|cell| cell.ch == '@'))
            .count();
        assert_eq!(
            resolved, 8,
            "skipping the entrance must settle on the title, not atomise it away",
        );
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn title_lifecycle_builds_content_bottom_up_with_coloured_rain() {
        let Some(wasm_bytes) = load_effect_variant_wasm("content_particles", "lifecycle") else {
            eprintln!("skipping title-lifecycle: run `make effects` to build it");
            return;
        };

        let mut content = CellBuffer::new(40, 5);
        let authored = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::BrightWhite)),
            bold: true,
            ..Default::default()
        };
        content.put_str(0, 0, "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@", &authored);
        content.put_str(0, 4, "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@", &authored);

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt
            .instantiate(&module, 40, 5, Some(content))
            .expect("instantiate");
        let frame_count = inst.init(40, 5).expect("init");
        assert_eq!(frame_count, 48);

        inst.tick(0).expect("tick entrance start");
        inst.tick(10).expect("tick coloured-rain frame");
        let mut rain_colours = Vec::new();
        for y in 0..5 {
            for x in 0..40 {
                let Some(cell) = inst.buffer().get(x, y) else {
                    continue;
                };
                if cell.ch == '@' || cell.ch == ' ' {
                    continue;
                }
                if let Some(colour) = cell.style.fg
                    && !rain_colours.contains(&colour)
                {
                    rain_colours.push(colour);
                }
            }
        }
        assert!(
            rain_colours.len() >= 2,
            "rain should use more than one accent colour"
        );
        inst.tick(16).expect("tick bottom-up frame");
        let top_resolved = (0..40)
            .filter(|&x| inst.buffer().get(x, 0).is_some_and(|cell| cell.ch == '@'))
            .count();
        let bottom_resolved = (0..40)
            .filter(|&x| inst.buffer().get(x, 4).is_some_and(|cell| cell.ch == '@'))
            .count();
        assert_eq!(bottom_resolved, 40, "bottom row should settle first");
        assert_eq!(top_resolved, 0, "top row should still be resolving");

        assert!(inst.tick(frame_count - 1).expect("tick final frame"));
        assert_eq!(inst.buffer().get(0, 0).map(|cell| cell.ch), Some('@'));
        assert_eq!(inst.buffer().get(0, 4).map(|cell| cell.ch), Some('@'));

        // Starting the lifecycle a second time atomises the title for deferred
        // navigation. Empty cells must remain transparent so the animated
        // Matrix background beneath the title can show through.
        inst.tick(0).expect("tick transparent exit frame");
        assert!(
            inst.buffer()
                .get(20, 2)
                .is_some_and(|cell| cell.is_transparent()),
            "title exit must not paint an opaque black mask"
        );

        inst.tick(13).expect("tick mirrored Matrix exit");
        let top_remaining = (0..40)
            .filter(|&x| inst.buffer().get(x, 0).is_some_and(|cell| cell.ch == '@'))
            .count();
        let bottom_remaining = (0..40)
            .filter(|&x| inst.buffer().get(x, 4).is_some_and(|cell| cell.ch == '@'))
            .count();
        assert_eq!(top_remaining, 0, "top row should dissolve first");
        assert_eq!(bottom_remaining, 40, "bottom row should dissolve last");
        assert!(
            (0..5).any(|y| {
                (0..40).any(|x| {
                    inst.buffer()
                        .get(x, y)
                        .is_some_and(|cell| cell.ch != '@' && cell.ch != ' ')
                })
            }),
            "departing title should become Matrix glyphs"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn typewriter_wasm_empty_content() {
        let wasm_bytes = match load_effect_wasm("typewriter") {
            Some(b) => b,
            None => return,
        };

        // Empty content buffer — all spaces
        let content = CellBuffer::new(10, 5);

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt
            .instantiate(&module, 10, 5, Some(content))
            .expect("instantiate");
        let frame_count = inst.init(10, 5).expect("init");

        // With no content, frame_count should be 0 (or tick should finish immediately)
        if frame_count > 0 {
            let finished = inst.tick(0).expect("tick");
            assert!(finished, "empty content should finish immediately");
        }
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn line_draw_wasm_reveals_path_in_reading_order() {
        let wasm_bytes = match load_effect_wasm("line_draw") {
            Some(b) => b,
            None => {
                eprintln!("skipping: line_draw.wasm not built");
                return;
            }
        };

        let mut content = CellBuffer::new(4, 2);
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::BrightWhite)),
            ..Default::default()
        };
        content.put_str(0, 0, "┬", &style);
        content.put_str(0, 1, "└──", &style);

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt
            .instantiate(&module, 4, 2, Some(content))
            .expect("instantiate");
        let frame_count = inst.init(4, 2).expect("init");
        assert_eq!(frame_count, 5, "blank frame plus four path cells");

        inst.tick(1).expect("first path cell");
        assert_eq!(inst.buffer().get(0, 0).map(|cell| cell.ch), Some('┬'));
        assert_eq!(inst.buffer().get(0, 1).map(|cell| cell.ch), Some(' '));

        let finished = inst.tick(4).expect("final path frame");
        assert!(finished);
        assert_eq!(inst.buffer().get(0, 1).map(|cell| cell.ch), Some('└'));
        assert_eq!(inst.buffer().get(2, 1).map(|cell| cell.ch), Some('─'));
        assert_eq!(
            inst.buffer().get(2, 1).and_then(|cell| cell.style.fg),
            Some(ResolvedColor::Named(NamedColor::BrightWhite)),
        );
    }

    #[cfg_attr(
        miri,
        ignore = "runs the WASM interpreter; Miri interpreting an interpreter is impractical"
    )]
    #[test]
    fn line_draw_down_wasm_reveals_complete_rows_top_to_bottom() {
        let Some(wasm_bytes) = load_effect_variant_wasm("line_draw", "top-down") else {
            eprintln!("skipping line-draw top-down: run `make effects` to build it");
            return;
        };

        let mut content = CellBuffer::new(4, 2);
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::BrightWhite)),
            ..Default::default()
        };
        content.put_str(0, 0, "┬─", &style);
        content.put_str(0, 1, "└──", &style);

        let rt = WasmRuntime::new();
        let module = rt.compile(&wasm_bytes).expect("compile");
        let mut inst = rt
            .instantiate(&module, 4, 2, Some(content))
            .expect("instantiate");
        let frame_count = inst.init(4, 2).expect("init");
        assert_eq!(frame_count, 3, "blank frame plus two path rows");

        let finished = inst.tick(1).expect("first path row");
        assert!(!finished);
        assert_eq!(inst.buffer().get(0, 0).map(|cell| cell.ch), Some('┬'));
        assert_eq!(inst.buffer().get(1, 0).map(|cell| cell.ch), Some('─'));
        assert_eq!(inst.buffer().get(0, 1).map(|cell| cell.ch), Some(' '));

        let finished = inst.tick(2).expect("second path row");
        assert!(finished);
        assert_eq!(inst.buffer().get(0, 1).map(|cell| cell.ch), Some('└'));
        assert_eq!(inst.buffer().get(2, 1).map(|cell| cell.ch), Some('─'));
    }
}
