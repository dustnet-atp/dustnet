use crate::color::ResolvedColor;
use crate::resource::{BudgetError, BudgetLease, ResourceCategory, ResourceGovernor};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Hard ceiling for one render buffer. A page owns several buffers, so this
/// deliberately stays well below the process-wide memory envelope.
pub const MAX_BUFFER_CELLS: usize = 1_048_576;

/// Failure to allocate a remotely influenced render buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferLimitExceeded {
    pub width: u16,
    pub height: u16,
}

impl std::fmt::Display for BufferLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "render buffer {}x{} exceeds the {}-cell limit",
            self.width, self.height, MAX_BUFFER_CELLS
        )
    }
}

impl std::error::Error for BufferLimitExceeded {}

/// Failure to admit or allocate a remotely influenced render buffer.
#[derive(Debug)]
pub enum GovernedBufferError {
    Budget(BudgetError),
    Buffer(BufferLimitExceeded),
}

impl std::fmt::Display for GovernedBufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(error) => write!(f, "render-buffer budget rejected: {error:?}"),
            Self::Buffer(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for GovernedBufferError {}

/// A 2D grid of styled character cells — the intermediate representation
/// between layout and rendering.
///
/// The cell buffer is row-major: cells[y * width + x].
/// Width matches the terminal width. Height may exceed the terminal
/// height (for scrollable document-mode pages).
#[derive(Debug)]
pub struct CellBuffer {
    pub width: u16,
    pub height: u16,
    cells: Vec<Cell>,
    allocation_failed: bool,
    /// Present for remotely influenced buffers that own their accounting.
    /// Keeping the lease beside the allocation makes drop and replacement
    /// exact and prevents an aggregate page lease outliving its storage.
    budget_lease: Option<BudgetLease>,
    /// Governor used for variable-sized grapheme payloads. Cell vectors and
    /// graphemes have different lifetimes when cells are copied between
    /// buffers, so graphemes own independent shared leases.
    grapheme_governor: Option<ResourceGovernor>,
}

impl CellBuffer {
    fn checked_size(width: u16, height: u16) -> Result<usize, BufferLimitExceeded> {
        if width > crate::parser::ast::MAX_LAYOUT_COLUMNS
            || height > crate::parser::ast::MAX_LAYOUT_ROWS
        {
            return Err(BufferLimitExceeded { width, height });
        }
        let size = usize::from(width)
            .checked_mul(usize::from(height))
            .ok_or(BufferLimitExceeded { width, height })?;
        if size > MAX_BUFFER_CELLS {
            return Err(BufferLimitExceeded { width, height });
        }
        Ok(size)
    }

    pub fn checked_cell_count(width: u16, height: u16) -> Result<usize, BufferLimitExceeded> {
        Self::checked_size(width, height)
    }

    /// Fallible constructor for every remotely influenced allocation.
    pub fn try_new(width: u16, height: u16) -> Result<Self, BufferLimitExceeded> {
        let size = Self::checked_size(width, height)?;
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(size)
            .map_err(|_| BufferLimitExceeded { width, height })?;
        cells.resize(size, Cell::empty());
        Ok(CellBuffer {
            width,
            height,
            cells,
            allocation_failed: false,
            budget_lease: None,
            grapheme_governor: None,
        })
    }

    fn governed_amount(category: ResourceCategory, cells: usize, bytes: usize) -> usize {
        if category == ResourceCategory::SceneCells {
            cells
        } else {
            bytes
        }
    }

    /// Reserve the exact logical cell storage before allocating it and retain
    /// that reservation for precisely as long as this buffer remains alive.
    pub fn try_new_governed(
        width: u16,
        height: u16,
        governor: &ResourceGovernor,
        category: ResourceCategory,
    ) -> Result<Self, GovernedBufferError> {
        let cells = Self::checked_size(width, height).map_err(GovernedBufferError::Buffer)?;
        let bytes =
            cells
                .checked_mul(std::mem::size_of::<Cell>())
                .ok_or(GovernedBufferError::Buffer(BufferLimitExceeded {
                    width,
                    height,
                }))?;
        let amount = Self::governed_amount(category, cells, bytes);
        let lease = governor
            .reserve_with_cost(category, amount, bytes)
            .map_err(GovernedBufferError::Budget)?;
        let mut buffer = Self::try_new(width, height).map_err(GovernedBufferError::Buffer)?;
        buffer.budget_lease = Some(lease);
        buffer.grapheme_governor = Some(governor.clone());
        Ok(buffer)
    }

    /// Allocate a transparent buffer from a reservation acquired as part of a
    /// larger transaction, transferring exact ownership into the buffer.
    pub(crate) fn try_new_with_lease(
        width: u16,
        height: u16,
        mut lease: BudgetLease,
    ) -> Result<Self, GovernedBufferError> {
        let cells = Self::checked_size(width, height).map_err(GovernedBufferError::Buffer)?;
        let bytes =
            cells
                .checked_mul(std::mem::size_of::<Cell>())
                .ok_or(GovernedBufferError::Buffer(BufferLimitExceeded {
                    width,
                    height,
                }))?;
        let amount = Self::governed_amount(lease.category(), cells, bytes);
        let governor = lease.governor();
        lease
            .try_resize_with_cost(amount, bytes)
            .map_err(GovernedBufferError::Budget)?;
        let mut buffer = Self::try_new(width, height).map_err(GovernedBufferError::Buffer)?;
        buffer.budget_lease = Some(lease);
        buffer.grapheme_governor = Some(governor);
        Ok(buffer)
    }

    /// Fallibly clone remotely influenced cell storage. Grapheme payloads and
    /// their leases share one `Arc`, while the new cell vector owns a distinct
    /// reservation and cannot be created without prior admission.
    pub fn try_clone_governed(
        &self,
        governor: &ResourceGovernor,
        category: ResourceCategory,
    ) -> Result<Self, GovernedBufferError> {
        let mut cloned = Self::try_new_governed(self.width, self.height, governor, category)?;
        cloned.cells.clone_from_slice(&self.cells);
        cloned.allocation_failed = self.allocation_failed;
        Ok(cloned)
    }

    /// Fallible copy for trusted/local callers. Remote state must use
    /// [`Self::try_clone_governed`] so the duplicate allocation is admitted.
    pub(crate) fn try_clone(&self) -> Result<Self, BufferLimitExceeded> {
        let mut cloned = Self::try_new(self.width, self.height)?;
        cloned.cells.clone_from_slice(&self.cells);
        cloned.allocation_failed = self.allocation_failed;
        Ok(cloned)
    }

    /// Create a new buffer filled with empty (transparent) cells.
    ///
    /// This convenience constructor is for dimensions already proved to be
    /// trusted. Hostile dimensions must use [`CellBuffer::try_new`].
    #[allow(
        clippy::expect_used,
        reason = "dimensions are caller-proved constants or already-admitted \
                  buffer sizes; remote dimensions must use try_new, which the \
                  doc comment above requires and every remote-fed caller does"
    )]
    pub(crate) fn new(width: u16, height: u16) -> Self {
        Self::try_new(width, height).expect("trusted render-buffer dimensions exceed limits")
    }

    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    pub fn allocation_failed(&self) -> bool {
        self.allocation_failed
    }

    pub(crate) fn record_allocation_failure(&mut self) {
        self.allocation_failed = true;
    }

    pub fn clear_transparent(&mut self) {
        self.cells.fill(Cell::empty());
    }

    /// Fallible resize for remotely influenced dimensions. The original
    /// buffer remains intact on failure.
    pub fn try_resize_preserving(
        &mut self,
        width: u16,
        height: u16,
    ) -> Result<(), BufferLimitExceeded> {
        if width == self.width && height == self.height {
            return Ok(());
        }

        let new_cells = Self::checked_size(width, height)?;
        let new_bytes = new_cells
            .checked_mul(std::mem::size_of::<Cell>())
            .ok_or(BufferLimitExceeded { width, height })?;
        let old_accounting = self
            .budget_lease
            .as_ref()
            .map(|lease| (lease.amount(), lease.byte_cost(), lease.category()));
        if let Some(lease) = self.budget_lease.as_mut()
            && let Some((old_amount, old_bytes, category)) = old_accounting
        {
            let new_amount = Self::governed_amount(category, new_cells, new_bytes);
            lease
                .try_resize_with_cost(
                    old_amount.saturating_add(new_amount),
                    old_bytes.saturating_add(new_bytes),
                )
                .map_err(|_| BufferLimitExceeded { width, height })?;
        }

        let mut resized = match CellBuffer::try_new(width, height) {
            Ok(buffer) => buffer,
            Err(error) => {
                // Restoring a previously admitted reservation releases
                // units and admits nothing, so it cannot be refused. Ignoring
                // the result keeps the rollback path panic-free.
                if let Some(lease) = self.budget_lease.as_mut()
                    && let Some((old_amount, old_bytes, _)) = old_accounting
                {
                    let _ = lease.try_resize_with_cost(old_amount, old_bytes);
                }
                return Err(error);
            }
        };
        let copy_w = self.width.min(width);
        let copy_h = self.height.min(height);
        for y in 0..copy_h {
            for x in 0..copy_w {
                if let Some(cell) = self.get(x, y) {
                    resized.set(x, y, cell.clone());
                }
            }
        }
        resized.budget_lease = self.budget_lease.take();
        resized.grapheme_governor = self.grapheme_governor.clone();
        if let Some(lease) = resized.budget_lease.as_mut() {
            let category = lease.category();
            let new_amount = Self::governed_amount(category, new_cells, new_bytes);
            // Shrinking an admitted reservation cannot be refused.
            let _ = lease.try_resize_with_cost(new_amount, new_bytes);
        }
        *self = resized;
        Ok(())
    }

    /// Create a new buffer filled with opaque black cells (space + black bg).
    ///
    /// Unlike `new()` which creates transparent cells, this produces cells that
    /// will occlude lower compositor layers — useful for transition buffers where
    /// "blank" should mean black, not see-through.
    #[cfg(test)]
    pub(crate) fn new_opaque(width: u16, height: u16) -> Self {
        Self::try_new_opaque(width, height)
            .expect("trusted opaque render-buffer dimensions exceed limits")
    }

    pub fn try_new_opaque(width: u16, height: u16) -> Result<Self, BufferLimitExceeded> {
        let size = Self::checked_size(width, height)?;
        let black_cell = Cell {
            ch: ' ',
            grapheme: None,
            style: CellStyle {
                bg: Some(ResolvedColor::Named(crate::color::NamedColor::Black)),
                ..Default::default()
            },
        };
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(size)
            .map_err(|_| BufferLimitExceeded { width, height })?;
        cells.resize(size, black_cell);
        Ok(CellBuffer {
            width,
            height,
            cells,
            allocation_failed: false,
            budget_lease: None,
            grapheme_governor: None,
        })
    }

    /// Governed counterpart to [`Self::try_new_opaque`]. The reservation is
    /// acquired before the cell vector is allocated and remains attached to
    /// the resulting buffer for its full lifetime.
    pub fn try_new_opaque_governed(
        width: u16,
        height: u16,
        governor: &ResourceGovernor,
        category: ResourceCategory,
    ) -> Result<Self, GovernedBufferError> {
        let cells = Self::checked_size(width, height).map_err(GovernedBufferError::Buffer)?;
        let bytes =
            cells
                .checked_mul(std::mem::size_of::<Cell>())
                .ok_or(GovernedBufferError::Buffer(BufferLimitExceeded {
                    width,
                    height,
                }))?;
        let amount = Self::governed_amount(category, cells, bytes);
        let lease = governor
            .reserve_with_cost(category, amount, bytes)
            .map_err(GovernedBufferError::Budget)?;
        let mut buffer =
            Self::try_new_opaque(width, height).map_err(GovernedBufferError::Buffer)?;
        buffer.budget_lease = Some(lease);
        buffer.grapheme_governor = Some(governor.clone());
        Ok(buffer)
    }

    /// Allocate an opaque buffer using a reservation acquired earlier in a
    /// larger transaction (for example, before transition snapshots are
    /// captured). The lease is normalized to this buffer's exact storage
    /// before allocation and then transferred into the buffer.
    pub(crate) fn try_new_opaque_with_lease(
        width: u16,
        height: u16,
        mut lease: BudgetLease,
    ) -> Result<Self, GovernedBufferError> {
        let cells = Self::checked_size(width, height).map_err(GovernedBufferError::Buffer)?;
        let bytes =
            cells
                .checked_mul(std::mem::size_of::<Cell>())
                .ok_or(GovernedBufferError::Buffer(BufferLimitExceeded {
                    width,
                    height,
                }))?;
        let amount = Self::governed_amount(lease.category(), cells, bytes);
        let governor = lease.governor();
        lease
            .try_resize_with_cost(amount, bytes)
            .map_err(GovernedBufferError::Budget)?;
        let mut buffer =
            Self::try_new_opaque(width, height).map_err(GovernedBufferError::Buffer)?;
        buffer.budget_lease = Some(lease);
        buffer.grapheme_governor = Some(governor);
        Ok(buffer)
    }

    /// Get a reference to the cell at (x, y). Returns None if out of bounds.
    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        // The dimension check guards the *logical* grid; `get` guards the
        // *physical* backing store. They are the same only while
        // `cells.len() == width * height`, which a refused resize could break.
        self.cells
            .get(y as usize * self.width as usize + x as usize)
    }

    /// Get a mutable reference to the cell at (x, y). Returns None if out of bounds.
    pub fn get_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = y as usize * self.width as usize + x as usize;
        self.cells.get_mut(offset)
    }

    /// Set the cell at (x, y). No-op if out of bounds.
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if let Some(slot) = self.get_mut(x, y) {
            *slot = cell;
        }
    }

    /// Write a character at (x, y) with the given style. No-op if out of bounds.
    pub fn put_char(&mut self, x: u16, y: u16, ch: char, style: &CellStyle) {
        if let Some(cell) = self.get_mut(x, y) {
            cell.ch = ch;
            cell.grapheme = None;
            cell.style = style.clone();
        }
    }

    /// Write a complete grapheme cluster at the anchor cell.
    pub fn put_grapheme(&mut self, x: u16, y: u16, grapheme: &str, style: &CellStyle) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let Some(ch) = grapheme.chars().next() else {
            return false;
        };
        let storage = if grapheme.chars().count() > 1 {
            match GraphemeStorage::try_new(grapheme, self.grapheme_governor.as_ref()) {
                Ok(storage) => Some(storage),
                Err(()) => {
                    self.allocation_failed = true;
                    return false;
                }
            }
        } else {
            None
        };
        if let Some(cell) = self.get_mut(x, y) {
            cell.ch = ch;
            cell.grapheme = storage;
            cell.style = style.clone();
        }
        true
    }

    /// Write a string starting at (x, y), advancing x by the character's
    /// display width. Stops at the right edge — does NOT wrap. Wide
    /// characters (display width 2) occupy two cells: the second cell
    /// holds `'\0'` as a continuation marker, matching the invariant
    /// that the ANSI emitter in `present/mod.rs` relies on to skip
    /// doubled emission. Zero-width characters are skipped.
    ///
    /// Returns the number of columns advanced.
    pub fn put_str(&mut self, x: u16, y: u16, s: &str, style: &CellStyle) -> u16 {
        let mut col = x;
        for grapheme in s.graphemes(true) {
            let w = UnicodeWidthStr::width(grapheme) as u16;
            if w == 0 {
                continue;
            }
            if col.saturating_add(w) > self.width {
                break;
            }
            if !self.put_grapheme(col, y, grapheme, style) {
                break;
            }
            if w == 2 {
                self.put_char(col.saturating_add(1), y, '\0', style);
            }
            col = col.saturating_add(w);
        }
        col - x
    }

    /// Fill a rectangular region with a character and style.
    /// Coordinates are clamped to buffer bounds.
    pub fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, ch: char, style: &CellStyle) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        let x_start = x.min(self.width);
        let y_start = y.min(self.height);

        for row in y_start..y_end {
            for col in x_start..x_end {
                self.put_char(col, row, ch, style);
            }
        }
    }

    /// Grow the buffer vertically to at least the given height.
    /// New rows are filled with empty cells.
    pub fn ensure_height(&mut self, min_height: u16) {
        let cell_limited_height = if self.width == 0 {
            0
        } else {
            (MAX_BUFFER_CELLS / self.width as usize).min(u16::MAX as usize) as u16
        };
        let min_height = min_height
            .min(crate::parser::ast::MAX_LAYOUT_ROWS)
            .min(cell_limited_height);
        if min_height > self.height {
            if self.budget_lease.is_some() {
                if self.try_resize_preserving(self.width, min_height).is_err() {
                    self.allocation_failed = true;
                }
                return;
            }
            let new_cells = (min_height - self.height) as usize * self.width as usize;
            if self.cells.try_reserve_exact(new_cells).is_err() {
                self.allocation_failed = true;
                return;
            }
            self.cells
                .extend(std::iter::repeat_n(Cell::empty(), new_cells));
            self.height = min_height;
        }
    }

    /// Get a row as a slice of cells.
    pub fn row(&self, y: u16) -> Option<&[Cell]> {
        if y >= self.height {
            return None;
        }
        let start = y as usize * self.width as usize;
        let end = start.checked_add(self.width as usize)?;
        self.cells.get(start..end)
    }

    /// Return the number of rows that contain non-empty content.
    /// Scans from the bottom to find the last row with a non-space character
    /// or explicit background color.
    pub fn content_height(&self) -> u16 {
        for y in (0..self.height).rev() {
            if let Some(row) = self.row(y)
                && row.iter().any(|c| c.ch != ' ' || c.style.bg.is_some())
            {
                return y + 1;
            }
        }
        0
    }

    /// Compare two buffers and return the positions of cells that differ.
    ///
    /// Returns `None` when the allocator refuses the exact reservation. The
    /// differing cells are counted before any are recorded, so the result
    /// vector is allocated once at its final size rather than grown a cell at
    /// a time — buffer height follows page content, so the difference count is
    /// remotely influenced even though each buffer's own storage is admitted.
    pub fn diff(&self, other: &CellBuffer) -> Option<Vec<(u16, u16)>> {
        let min_h = self.height.min(other.height);
        let min_w = self.width.min(other.width);

        let mut count = 0usize;
        for y in 0..min_h {
            for x in 0..min_w {
                if self.get(x, y) != other.get(x, y) {
                    count += 1;
                }
            }
        }

        let mut changed = Vec::new();
        changed.try_reserve_exact(count).ok()?;
        for y in 0..min_h {
            for x in 0..min_w {
                if self.get(x, y) != other.get(x, y) {
                    changed.push((x, y));
                }
            }
        }

        Some(changed)
    }
}

/// Complete multi-scalar glyph storage whose budget follows every shared cell
/// copy. The payload and lease live behind the same `Arc`, so copying a cell
/// neither allocates another string nor creates another accounting owner.
#[derive(Debug)]
pub struct GraphemeStorage {
    text: Box<str>,
    _lease: Option<BudgetLease>,
}

impl GraphemeStorage {
    fn try_new(grapheme: &str, governor: Option<&ResourceGovernor>) -> Result<Arc<Self>, ()> {
        let lease = governor
            .map(|governor| governor.reserve(ResourceCategory::RemoteCollections, grapheme.len()))
            .transpose()
            .map_err(|_| ())?;
        let mut text = String::new();
        text.try_reserve_exact(grapheme.len()).map_err(|_| ())?;
        text.push_str(grapheme);
        Ok(Arc::new(Self {
            text: text.into_boxed_str(),
            _lease: lease,
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl PartialEq for GraphemeStorage {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for GraphemeStorage {}

impl std::ops::Deref for GraphemeStorage {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

/// A single character cell with styling.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub ch: char,
    /// Complete cluster when the visible glyph contains multiple scalars.
    /// `ch` remains the anchor scalar for fast classification and the WASM ABI.
    pub grapheme: Option<Arc<GraphemeStorage>>,
    pub style: CellStyle,
}

impl Cell {
    /// An empty cell: space character with default style.
    pub fn empty() -> Self {
        Cell {
            ch: ' ',
            grapheme: None,
            style: CellStyle::default(),
        }
    }

    /// Create a cell with a character and style.
    pub fn new(ch: char, style: CellStyle) -> Self {
        Cell {
            ch,
            grapheme: None,
            style,
        }
    }

    /// String emitted for this cell's visible glyph.
    pub fn glyph(&self) -> std::borrow::Cow<'_, str> {
        match self.grapheme.as_ref() {
            Some(grapheme) => std::borrow::Cow::Borrowed(grapheme.as_str()),
            None => std::borrow::Cow::Owned(self.ch.to_string()),
        }
    }

    /// Write the visible glyph without allocating for a scalar character.
    pub fn write_glyph(&self, out: &mut impl std::io::Write) -> std::io::Result<()> {
        match self.grapheme.as_ref() {
            Some(grapheme) => out.write_all(grapheme.as_bytes()),
            None => write!(out, "{}", self.ch),
        }
    }

    /// Whether this cell is transparent for compositing purposes.
    ///
    /// A cell is transparent if it is a space with no explicit background color
    /// and no visible text decorations. Underline and strikethrough are visible
    /// on space characters, so cells with those decorations are opaque.
    pub fn is_transparent(&self) -> bool {
        self.ch == ' '
            && self.style.bg.is_none()
            && !self.style.underline
            && !self.style.strikethrough
    }
}

/// Style properties for a cell, resolved to concrete terminal values.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CellStyle {
    pub fg: Option<ResolvedColor>,
    pub bg: Option<ResolvedColor>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub blink: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::NamedColor;

    #[test]
    fn new_buffer_is_empty() {
        let buf = CellBuffer::new(10, 5);
        assert_eq!(buf.width, 10);
        assert_eq!(buf.height, 5);
        for y in 0..5 {
            for x in 0..10 {
                let cell = buf.get(x, y).unwrap();
                assert_eq!(cell.ch, ' ');
            }
        }
    }

    #[test]
    fn hostile_dimensions_are_rejected_not_clamped() {
        assert!(matches!(
            CellBuffer::try_new(u16::MAX, u16::MAX),
            Err(BufferLimitExceeded {
                width: u16::MAX,
                height: u16::MAX,
            })
        ));
        assert!(CellBuffer::try_new_opaque(u16::MAX, u16::MAX).is_err());
    }

    #[test]
    fn resize_preserving_keeps_overlap_and_clears_new_cells() {
        let mut buf = CellBuffer::new(2, 2);
        buf.put_char(1, 1, 'X', &CellStyle::default());

        buf.try_resize_preserving(4, 3).unwrap();

        assert_eq!(buf.width, 4);
        assert_eq!(buf.height, 3);
        assert_eq!(buf.get(1, 1).unwrap().ch, 'X');
        assert_eq!(buf.get(3, 2).unwrap().ch, ' ');

        buf.try_resize_preserving(2, 2).unwrap();
        assert_eq!(buf.get(1, 1).unwrap().ch, 'X');
    }

    #[test]
    fn get_out_of_bounds() {
        let buf = CellBuffer::new(10, 5);
        assert!(buf.get(10, 0).is_none());
        assert!(buf.get(0, 5).is_none());
        assert!(buf.get(100, 100).is_none());
    }

    #[test]
    fn set_and_get() {
        let mut buf = CellBuffer::new(10, 5);
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Red)),
            bold: true,
            ..Default::default()
        };
        buf.set(3, 2, Cell::new('X', style.clone()));
        let cell = buf.get(3, 2).unwrap();
        assert_eq!(cell.ch, 'X');
        assert_eq!(cell.style.fg, Some(ResolvedColor::Named(NamedColor::Red)));
        assert!(cell.style.bold);
    }

    #[test]
    fn put_char_out_of_bounds_is_noop() {
        let mut buf = CellBuffer::new(10, 5);
        buf.put_char(20, 20, 'X', &CellStyle::default());
        // Should not panic
    }

    #[test]
    fn put_str_basic() {
        let mut buf = CellBuffer::new(20, 1);
        let style = CellStyle::default();
        let written = buf.put_str(2, 0, "hello", &style);
        assert_eq!(written, 5);
        assert_eq!(buf.get(2, 0).unwrap().ch, 'h');
        assert_eq!(buf.get(3, 0).unwrap().ch, 'e');
        assert_eq!(buf.get(6, 0).unwrap().ch, 'o');
    }

    #[test]
    fn put_str_preserves_complete_grapheme_clusters() {
        let mut buf = CellBuffer::new(12, 1);
        let written = buf.put_str(0, 0, "e\u{301}👩‍💻", &CellStyle::default());

        assert_eq!(written, 3);
        assert_eq!(buf.get(0, 0).unwrap().glyph(), "e\u{301}");
        assert_eq!(buf.get(1, 0).unwrap().glyph(), "👩‍💻");
        assert_eq!(buf.get(2, 0).unwrap().ch, '\0');
    }

    #[test]
    fn cloned_cells_share_grapheme_storage() {
        let mut buf = CellBuffer::new(4, 1);
        buf.put_grapheme(0, 0, "👩‍💻", &CellStyle::default());
        let original = buf.get(0, 0).unwrap();
        let cloned = original.clone();

        assert!(Arc::ptr_eq(
            original.grapheme.as_ref().unwrap(),
            cloned.grapheme.as_ref().unwrap()
        ));
    }

    #[test]
    fn governed_clone_owns_a_distinct_exact_lease_and_shares_graphemes() {
        let governor = ResourceGovernor::new();
        let mut original =
            CellBuffer::try_new_governed(4, 1, &governor, ResourceCategory::SceneCells).unwrap();
        original.put_grapheme(0, 0, "👩‍💻", &CellStyle::default());
        let bytes = 4 * std::mem::size_of::<Cell>();
        let grapheme_bytes = "👩‍💻".len();
        assert_eq!(governor.used(ResourceCategory::SceneCells), 4);
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            grapheme_bytes
        );
        assert_eq!(governor.total_used(), bytes + grapheme_bytes);

        let cloned = original
            .try_clone_governed(&governor, ResourceCategory::SceneCells)
            .unwrap();
        assert_eq!(governor.used(ResourceCategory::SceneCells), 8);
        assert_eq!(governor.total_used(), bytes * 2 + grapheme_bytes);
        assert!(Arc::ptr_eq(
            original.get(0, 0).unwrap().grapheme.as_ref().unwrap(),
            cloned.get(0, 0).unwrap().grapheme.as_ref().unwrap(),
        ));

        drop(original);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 4);
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            grapheme_bytes
        );
        assert_eq!(governor.total_used(), bytes + grapheme_bytes);
        drop(cloned);
        assert_eq!(governor.total_used(), 0);
    }

    #[test]
    fn replacing_or_clearing_a_governed_grapheme_releases_its_shared_lease() {
        let governor = ResourceGovernor::new();
        let mut buffer =
            CellBuffer::try_new_governed(2, 1, &governor, ResourceCategory::SceneCells).unwrap();

        assert!(buffer.put_grapheme(0, 0, "e\u{301}", &CellStyle::default()));
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            "e\u{301}".len()
        );

        buffer.put_char(0, 0, 'x', &CellStyle::default());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);

        assert!(buffer.put_grapheme(1, 0, "👩‍💻", &CellStyle::default()));
        buffer.clear_transparent();
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn grapheme_budget_rejection_preserves_the_previous_cell() {
        let governor = ResourceGovernor::new();
        let mut buffer =
            CellBuffer::try_new_governed(1, 1, &governor, ResourceCategory::SceneCells).unwrap();
        buffer.put_char(0, 0, 'x', &CellStyle::default());
        let remaining = crate::resource::MAX_REMOTE_MEMORY - governor.total_used();
        let _pressure = governor
            .reserve(ResourceCategory::RemoteCollections, remaining)
            .unwrap();

        assert!(!buffer.put_grapheme(0, 0, "e\u{301}", &CellStyle::default()));
        assert_eq!(buffer.get(0, 0).unwrap().glyph(), "x");
        assert!(buffer.allocation_failed());
        assert_eq!(governor.total_used(), crate::resource::MAX_REMOTE_MEMORY);
    }

    #[test]
    fn governed_resize_rejects_before_allocation_and_preserves_buffer_and_lease() {
        let governor = ResourceGovernor::new();
        let _pressure = governor
            .reserve_with_cost(
                ResourceCategory::SceneCells,
                crate::resource::MAX_SCENE_CELLS - 4,
                0,
            )
            .unwrap();
        let mut buffer =
            CellBuffer::try_new_governed(2, 2, &governor, ResourceCategory::SceneCells).unwrap();
        buffer.put_char(1, 1, 'X', &CellStyle::default());
        let used_before = governor.used(ResourceCategory::SceneCells);
        let bytes_before = governor.total_used();

        assert!(buffer.try_resize_preserving(3, 2).is_err());
        assert_eq!((buffer.width, buffer.height), (2, 2));
        assert_eq!(buffer.get(1, 1).unwrap().ch, 'X');
        assert_eq!(governor.used(ResourceCategory::SceneCells), used_before);
        assert_eq!(governor.total_used(), bytes_before);
    }

    #[test]
    fn governed_growth_and_shrink_track_the_live_buffer_exactly() {
        let governor = ResourceGovernor::new();
        let mut buffer =
            CellBuffer::try_new_governed(2, 2, &governor, ResourceCategory::SceneCells).unwrap();
        buffer.put_char(1, 1, 'X', &CellStyle::default());

        buffer.ensure_height(3);
        assert!(!buffer.allocation_failed());
        assert_eq!((buffer.width, buffer.height), (2, 3));
        assert_eq!(buffer.get(1, 1).unwrap().ch, 'X');
        assert_eq!(governor.used(ResourceCategory::SceneCells), 6);
        assert_eq!(governor.total_used(), 6 * std::mem::size_of::<Cell>());

        buffer.try_resize_preserving(1, 1).unwrap();
        assert_eq!(governor.used(ResourceCategory::SceneCells), 1);
        assert_eq!(governor.total_used(), std::mem::size_of::<Cell>());
    }

    #[test]
    fn put_str_truncates_at_edge() {
        let mut buf = CellBuffer::new(5, 1);
        let style = CellStyle::default();
        let written = buf.put_str(3, 0, "hello", &style);
        assert_eq!(written, 2); // only 'h' and 'e' fit
        assert_eq!(buf.get(3, 0).unwrap().ch, 'h');
        assert_eq!(buf.get(4, 0).unwrap().ch, 'e');
    }

    #[test]
    fn put_str_wide_chars_write_continuation() {
        let mut buf = CellBuffer::new(8, 1);
        let style = CellStyle::default();
        let written = buf.put_str(0, 0, "日本語", &style);
        assert_eq!(written, 6, "three wide chars advance 6 cols");
        assert_eq!(buf.get(0, 0).unwrap().ch, '日');
        assert_eq!(buf.get(1, 0).unwrap().ch, '\0', "continuation marker");
        assert_eq!(buf.get(2, 0).unwrap().ch, '本');
        assert_eq!(buf.get(3, 0).unwrap().ch, '\0');
        assert_eq!(buf.get(4, 0).unwrap().ch, '語');
        assert_eq!(buf.get(5, 0).unwrap().ch, '\0');
    }

    #[test]
    fn put_str_wide_char_wont_split_at_edge() {
        let mut buf = CellBuffer::new(3, 1);
        let style = CellStyle::default();
        // "a日b" — 'a' fits at col 0, '日' needs cols 1-2 (fits),
        // 'b' doesn't fit at col 3. Written = 3 (1 for 'a' + 2 for '日').
        let written = buf.put_str(0, 0, "a日b", &style);
        assert_eq!(written, 3);
        assert_eq!(buf.get(0, 0).unwrap().ch, 'a');
        assert_eq!(buf.get(1, 0).unwrap().ch, '日');
        assert_eq!(buf.get(2, 0).unwrap().ch, '\0');
    }

    #[test]
    fn put_str_wide_char_refuses_to_straddle_edge() {
        let mut buf = CellBuffer::new(2, 1);
        let style = CellStyle::default();
        // "a日" — 'a' fits, '日' would need cols 1-2 but buf is only 2
        // wide, so it stops. Written = 1.
        let written = buf.put_str(0, 0, "a日", &style);
        assert_eq!(written, 1);
        assert_eq!(buf.get(0, 0).unwrap().ch, 'a');
        assert_eq!(buf.get(1, 0).unwrap().ch, ' ', "no partial wide char");
    }

    #[test]
    fn fill_rect() {
        let mut buf = CellBuffer::new(10, 5);
        let style = CellStyle {
            bg: Some(ResolvedColor::Named(NamedColor::Blue)),
            ..Default::default()
        };
        buf.fill_rect(2, 1, 3, 2, '#', &style);
        assert_eq!(buf.get(2, 1).unwrap().ch, '#');
        assert_eq!(buf.get(4, 2).unwrap().ch, '#');
        assert_eq!(buf.get(1, 1).unwrap().ch, ' '); // outside rect
        assert_eq!(buf.get(5, 1).unwrap().ch, ' '); // outside rect
    }

    #[test]
    fn fill_rect_clamps_to_bounds() {
        let mut buf = CellBuffer::new(5, 3);
        let style = CellStyle::default();
        buf.fill_rect(3, 1, 10, 10, 'X', &style);
        assert_eq!(buf.get(3, 1).unwrap().ch, 'X');
        assert_eq!(buf.get(4, 2).unwrap().ch, 'X');
        // Should not panic on oversized rect
    }

    #[test]
    fn ensure_height_grows() {
        let mut buf = CellBuffer::new(10, 5);
        buf.ensure_height(10);
        assert_eq!(buf.height, 10);
        assert_eq!(buf.get(0, 9).unwrap().ch, ' ');
    }

    #[test]
    fn ensure_height_noop_if_already_tall() {
        let mut buf = CellBuffer::new(10, 20);
        buf.ensure_height(5);
        assert_eq!(buf.height, 20);
    }

    #[test]
    fn row_slice() {
        let mut buf = CellBuffer::new(3, 2);
        buf.put_char(0, 1, 'A', &CellStyle::default());
        buf.put_char(1, 1, 'B', &CellStyle::default());
        buf.put_char(2, 1, 'C', &CellStyle::default());

        let row = buf.row(1).unwrap();
        assert_eq!(row.len(), 3);
        assert_eq!(row[0].ch, 'A');
        assert_eq!(row[1].ch, 'B');
        assert_eq!(row[2].ch, 'C');
    }

    #[test]
    fn row_out_of_bounds() {
        let buf = CellBuffer::new(3, 2);
        assert!(buf.row(5).is_none());
    }

    #[test]
    fn diff_identical_buffers() {
        let buf1 = CellBuffer::new(5, 3);
        let buf2 = CellBuffer::new(5, 3);
        assert!(buf1.diff(&buf2).expect("diff refused").is_empty());
    }

    #[test]
    fn diff_detects_changes() {
        let buf1 = CellBuffer::new(5, 3);
        let mut buf2 = CellBuffer::new(5, 3);
        buf2.put_char(2, 1, 'X', &CellStyle::default());
        buf2.put_char(4, 2, 'Y', &CellStyle::default());

        let changes = buf1.diff(&buf2).expect("diff refused");
        assert_eq!(changes.len(), 2);
        assert!(changes.contains(&(2, 1)));
        assert!(changes.contains(&(4, 2)));
    }

    #[test]
    fn zero_size_buffer() {
        let buf = CellBuffer::new(0, 0);
        assert!(buf.get(0, 0).is_none());
        assert!(buf.row(0).is_none());
    }

    #[test]
    fn dimensions_and_cell_count_are_bounded() {
        assert!(CellBuffer::try_new(u16::MAX, u16::MAX).is_err());
        let buf = CellBuffer::try_new(512, 2048).unwrap();
        assert_eq!(buf.cell_count(), MAX_BUFFER_CELLS);
    }

    #[test]
    #[cfg_attr(miri, ignore = "1 MiB cell budget loop is covered by native tests")]
    fn ensure_height_respects_cell_budget() {
        let mut buf = CellBuffer::new(512, 1);
        buf.ensure_height(u16::MAX);
        assert_eq!(buf.height, (MAX_BUFFER_CELLS / 512) as u16);
    }

    #[test]
    fn empty_cell_is_transparent() {
        assert!(Cell::empty().is_transparent());
    }

    #[test]
    fn cell_with_char_is_opaque() {
        let cell = Cell::new('X', CellStyle::default());
        assert!(!cell.is_transparent());
    }

    #[test]
    fn space_with_bg_is_opaque() {
        let style = CellStyle {
            bg: Some(ResolvedColor::Named(NamedColor::Blue)),
            ..Default::default()
        };
        let cell = Cell::new(' ', style);
        assert!(!cell.is_transparent());
    }

    #[test]
    fn underlined_space_is_opaque() {
        let style = CellStyle {
            underline: true,
            ..Default::default()
        };
        let cell = Cell::new(' ', style);
        assert!(
            !cell.is_transparent(),
            "underlined space must survive compositing"
        );
    }

    #[test]
    fn strikethrough_space_is_opaque() {
        let style = CellStyle {
            strikethrough: true,
            ..Default::default()
        };
        let cell = Cell::new(' ', style);
        assert!(!cell.is_transparent());
    }

    #[test]
    fn space_with_only_fg_is_transparent() {
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Red)),
            ..Default::default()
        };
        let cell = Cell::new(' ', style);
        assert!(
            cell.is_transparent(),
            "fg-only space has no visible decoration"
        );
    }
}
