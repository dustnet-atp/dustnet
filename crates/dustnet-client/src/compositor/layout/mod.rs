pub mod border;
pub mod cell;
pub mod engine;
pub mod kinds;
pub mod rect;
pub mod text;

pub use rect::Rect;

/// The layout allocations a test can force to behave as refused.
///
/// Layout had no rejection injection at all: its accounting tests exhausted a
/// small real governor, which proves a refusal is *possible* but cannot say
/// *which* allocation refused, so it cannot express one candidate failing
/// while its siblings roll back cleanly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LayoutAllocationSite {
    /// A governed temporary vector — the child-id snapshots and column widths
    /// the per-kind layouts take.
    TempVec,
    /// The focusable/placed/sticky metadata admitted as one transaction.
    FixedMetadata,
    /// A plain-text wrap's line vector and line strings.
    WrappedText,
}

#[cfg(test)]
thread_local! {
    static REJECT_LAYOUT_ALLOCATION: std::cell::Cell<Option<LayoutAllocationSite>> =
        const { std::cell::Cell::new(None) };
}

/// Arms one layout allocation site to refuse, and disarms it on drop.
///
/// Scoped rather than one-shot so a test cannot leave a site armed for
/// whatever runs next on the same thread.
#[cfg(test)]
pub(crate) struct LayoutRejectionGuard;

#[cfg(test)]
impl LayoutRejectionGuard {
    pub(crate) fn at(site: LayoutAllocationSite) -> Self {
        REJECT_LAYOUT_ALLOCATION.with(|rejected| rejected.set(Some(site)));
        Self
    }
}

#[cfg(test)]
impl Drop for LayoutRejectionGuard {
    fn drop(&mut self) {
        REJECT_LAYOUT_ALLOCATION.with(|rejected| rejected.set(None));
    }
}

#[cfg(test)]
pub(crate) fn reject_layout_allocation(site: LayoutAllocationSite) -> bool {
    REJECT_LAYOUT_ALLOCATION.with(|rejected| rejected.get() == Some(site))
}

/// Compiled away in release builds: there is no custom global allocator and no
/// production path can reach the injection state.
#[cfg(not(test))]
pub(crate) fn reject_layout_allocation(_site: LayoutAllocationSite) -> bool {
    false
}
