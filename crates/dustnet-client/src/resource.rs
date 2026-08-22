//! Aggregate resource accounting for all remotely influenced client state.

use std::sync::{Arc, Mutex};

pub const MAX_SCENE_CELLS: usize = 1_048_576;
pub const MAX_REMOTE_MEMORY: usize = 128 * 1024 * 1024;
pub const MAX_WASM_GUEST_BYTES: usize = 4 * 1024 * 1024;
/// Maximum live guests across the active page and one transactionally staged
/// navigation candidate. The parser independently limits each page to
/// `MAX_WASM_INSTANCES`, so staging needs room for two valid pages at once.
pub const MAX_WASM_GUESTS: usize = crate::parser::MAX_WASM_INSTANCES * 2;
pub const MAX_WASM_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceCategory {
    AstStrings,
    SceneCells,
    CompositorCells,
    ResourceCache,
    PendingUpdates,
    History,
    Wasm,
    AnimationRegions,
    AuthoredFrames,
    RemoteCollections,
    Sessions,
}

impl ResourceCategory {
    const COUNT: usize = 11;
    const fn index(self) -> usize {
        self as usize
    }
    const fn byte_limit(self) -> usize {
        match self {
            Self::SceneCells => MAX_SCENE_CELLS,
            Self::ResourceCache | Self::PendingUpdates => 8 * 1024 * 1024,
            Self::History => 16 * 1024 * 1024,
            Self::Wasm => MAX_WASM_BYTES,
            Self::AnimationRegions => 1_024,
            Self::AuthoredFrames => 256,
            Self::AstStrings | Self::CompositorCells | Self::RemoteCollections => MAX_REMOTE_MEMORY,
            Self::Sessions => dustnet_core::session::MAX_SESSION_CANDIDATE_BYTES * 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetError {
    Category {
        category: ResourceCategory,
        requested: usize,
        available: usize,
    },
    Aggregate {
        requested: usize,
        available: usize,
    },
    OwnerTooLarge {
        category: ResourceCategory,
        requested: usize,
        maximum: usize,
    },
    Count {
        category: ResourceCategory,
        maximum: usize,
    },
}

#[derive(Debug, Default)]
struct Usage {
    bytes: usize,
    categories: [usize; ResourceCategory::COUNT],
    counts: [usize; ResourceCategory::COUNT],
}

impl Usage {
    /// Per-category slots are indexed by `ResourceCategory::index`, which is
    /// `self as usize` over an enum with exactly `COUNT` variants, so every
    /// index is in range by construction.
    ///
    /// These accessors exist so that argument lives in one readable place
    /// rather than being re-made at eleven call sites, and so none of those
    /// sites is a raw index that would panic if a variant were ever added
    /// without updating `COUNT`. Reading an out-of-range slot reports zero and
    /// writing one is dropped — both fail toward refusing an admission rather
    /// than toward granting one.
    fn bytes_for(&self, category: ResourceCategory) -> usize {
        self.categories.get(category.index()).copied().unwrap_or(0)
    }

    fn count_for(&self, category: ResourceCategory) -> usize {
        self.counts.get(category.index()).copied().unwrap_or(0)
    }

    fn set_bytes(&mut self, category: ResourceCategory, value: usize) {
        if let Some(slot) = self.categories.get_mut(category.index()) {
            *slot = value;
        }
    }

    fn set_count(&mut self, category: ResourceCategory, value: usize) {
        if let Some(slot) = self.counts.get_mut(category.index()) {
            *slot = value;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResourceGovernor {
    usage: Arc<Mutex<Usage>>,
}

impl ResourceGovernor {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn shares_budget_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.usage, &other.usage)
    }

    pub fn reserve(
        &self,
        category: ResourceCategory,
        amount: usize,
    ) -> Result<BudgetLease, BudgetError> {
        self.reserve_with_cost(category, amount, amount)
    }

    /// Reserve category-specific units while charging their exact byte cost to the aggregate.
    pub fn reserve_with_cost(
        &self,
        category: ResourceCategory,
        amount: usize,
        byte_cost: usize,
    ) -> Result<BudgetLease, BudgetError> {
        let owner_limit = match category {
            ResourceCategory::Wasm => Some(MAX_WASM_GUEST_BYTES),
            _ => None,
        };
        if let Some(maximum) = owner_limit
            && amount > maximum
        {
            return Err(BudgetError::OwnerTooLarge {
                category,
                requested: amount,
                maximum,
            });
        }
        let count_limit = match category {
            ResourceCategory::ResourceCache => Some(32),
            ResourceCategory::PendingUpdates => Some(64),
            ResourceCategory::History => Some(128),
            ResourceCategory::Wasm => Some(MAX_WASM_GUESTS),
            _ => None,
        };
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(maximum) = count_limit
            && usage.count_for(category) >= maximum
        {
            return Err(BudgetError::Count { category, maximum });
        }
        let current = usage.bytes_for(category);
        let available = category.byte_limit().saturating_sub(current);
        if amount > available {
            return Err(BudgetError::Category {
                category,
                requested: amount,
                available,
            });
        }
        let aggregate_available = MAX_REMOTE_MEMORY.saturating_sub(usage.bytes);
        if byte_cost > aggregate_available {
            return Err(BudgetError::Aggregate {
                requested: byte_cost,
                available: aggregate_available,
            });
        }
        let next_count = usage.count_for(category) + 1;
        usage.set_bytes(category, current + amount);
        usage.set_count(category, next_count);
        usage.bytes += byte_cost;
        drop(usage);
        Ok(BudgetLease {
            governor: self.clone(),
            category,
            amount,
            byte_cost,
        })
    }

    pub fn used(&self, category: ResourceCategory) -> usize {
        self.usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bytes_for(category)
    }

    pub fn total_used(&self) -> usize {
        self.usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bytes
    }

    pub fn count(&self, category: ResourceCategory) -> usize {
        self.usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .count_for(category)
    }
}

#[derive(Debug)]
pub struct BudgetLease {
    governor: ResourceGovernor,
    category: ResourceCategory,
    amount: usize,
    byte_cost: usize,
}

impl BudgetLease {
    pub(crate) fn governor(&self) -> ResourceGovernor {
        self.governor.clone()
    }

    pub fn amount(&self) -> usize {
        self.amount
    }

    pub fn byte_cost(&self) -> usize {
        self.byte_cost
    }

    pub fn category(&self) -> ResourceCategory {
        self.category
    }

    /// Atomically resize category units and their aggregate byte cost.
    ///
    /// Growth is admitted before the lease changes. Shrinkage releases the
    /// exact unit and byte deltas. On error both the lease and governor remain
    /// unchanged, so callers can reserve first and then replace their storage.
    pub fn try_resize_with_cost(
        &mut self,
        new_amount: usize,
        new_byte_cost: usize,
    ) -> Result<(), BudgetError> {
        let maximum = match self.category {
            ResourceCategory::Wasm => Some(MAX_WASM_GUEST_BYTES),
            _ => None,
        };
        if let Some(maximum) = maximum
            && new_amount > maximum
        {
            return Err(BudgetError::OwnerTooLarge {
                category: self.category,
                requested: new_amount,
                maximum,
            });
        }
        let mut usage = self
            .governor
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let category_without_self = usage.bytes_for(self.category).saturating_sub(self.amount);
        let available = self
            .category
            .byte_limit()
            .saturating_sub(category_without_self);
        if new_amount > available {
            return Err(BudgetError::Category {
                category: self.category,
                requested: new_amount,
                available,
            });
        }
        let aggregate_without_self = usage.bytes.saturating_sub(self.byte_cost);
        let aggregate_available = MAX_REMOTE_MEMORY.saturating_sub(aggregate_without_self);
        if new_byte_cost > aggregate_available {
            return Err(BudgetError::Aggregate {
                requested: new_byte_cost,
                available: aggregate_available,
            });
        }
        usage.set_bytes(self.category, category_without_self + new_amount);
        usage.bytes = aggregate_without_self + new_byte_cost;
        self.amount = new_amount;
        self.byte_cost = new_byte_cost;
        Ok(())
    }

    /// Grow an exact byte-for-byte reservation without consuming another
    /// owner/count slot.
    pub fn try_grow(&mut self, new_amount: usize) -> Result<(), BudgetError> {
        if new_amount <= self.amount {
            return Ok(());
        }
        assert_eq!(
            self.amount, self.byte_cost,
            "try_grow requires an exact byte reservation"
        );
        self.try_resize_with_cost(new_amount, new_amount)
    }

    /// Reduce an exact byte-for-byte reservation after owned storage shrinks.
    ///
    /// This is intentionally infallible and only applies to leases created
    /// with `reserve`, where category units and aggregate bytes are identical.
    ///
    /// Both invariants are `debug_assert`s rather than `assert`s, and the
    /// amount is clamped. Every caller passes a *measured* retained capacity
    /// derived from remote content, so an accounting slip here would abort the
    /// client on a hostile page — the denial-of-service class this crate's
    /// panic gate exists to remove. The invariant stays loud where it is
    /// actually exercised, which is the test suite.
    pub fn shrink_to(&mut self, new_amount: usize) {
        debug_assert!(new_amount <= self.amount, "a lease cannot shrink upward");
        debug_assert_eq!(
            self.amount, self.byte_cost,
            "shrink_to requires an exact byte reservation"
        );
        let new_amount = new_amount.min(self.amount);
        // Shrinking an exact reservation releases units; it admits nothing, so
        // the governor cannot refuse it. Ignoring the result keeps this
        // infallible without an `expect` on a remote-fed path.
        let _ = self.try_resize_with_cost(new_amount, new_amount);
    }
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        let mut usage = self
            .governor
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let released_bytes = usage.bytes_for(self.category).saturating_sub(self.amount);
        let released_count = usage.count_for(self.category).saturating_sub(1);
        usage.set_bytes(self.category, released_bytes);
        usage.set_count(self.category, released_count);
        usage.bytes = usage.bytes.saturating_sub(self.byte_cost);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_atomic_and_release_on_drop() {
        let governor = ResourceGovernor::new();
        let lease = governor
            .reserve(ResourceCategory::PendingUpdates, 1024)
            .unwrap();
        assert_eq!(lease.amount(), 1024);
        assert_eq!(governor.total_used(), 1024);
        drop(lease);
        assert_eq!(governor.total_used(), 0);
    }

    #[test]
    fn shrinking_a_lease_releases_bytes_without_releasing_its_owner_slot() {
        let governor = ResourceGovernor::new();
        let mut lease = governor
            .reserve(ResourceCategory::CompositorCells, 4096)
            .unwrap();
        lease.shrink_to(1024);
        assert_eq!(lease.amount(), 1024);
        assert_eq!(governor.total_used(), 1024);
        assert_eq!(governor.count(ResourceCategory::CompositorCells), 1);
        drop(lease);
        assert_eq!(governor.total_used(), 0);
        assert_eq!(governor.count(ResourceCategory::CompositorCells), 0);
    }

    #[test]
    fn exact_cost_resize_tracks_units_and_bytes_atomically() {
        let governor = ResourceGovernor::new();
        let mut lease = governor
            .reserve_with_cost(ResourceCategory::SceneCells, 2, 128)
            .unwrap();

        lease.try_resize_with_cost(5, 320).unwrap();
        assert_eq!(lease.amount(), 5);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 5);
        assert_eq!(governor.total_used(), 320);

        lease.try_resize_with_cost(1, 64).unwrap();
        assert_eq!(lease.amount(), 1);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 1);
        assert_eq!(governor.total_used(), 64);
        assert_eq!(governor.count(ResourceCategory::SceneCells), 1);
    }

    #[test]
    fn rejected_exact_cost_resize_preserves_the_original_lease() {
        let governor = ResourceGovernor::new();
        let mut lease = governor
            .reserve_with_cost(ResourceCategory::SceneCells, 2, 128)
            .unwrap();

        assert!(matches!(
            lease.try_resize_with_cost(MAX_SCENE_CELLS + 1, 256),
            Err(BudgetError::Category { .. })
        ));
        assert_eq!(lease.amount(), 2);
        assert_eq!(governor.used(ResourceCategory::SceneCells), 2);
        assert_eq!(governor.total_used(), 128);
    }

    #[test]
    fn mixed_categories_cannot_exceed_aggregate_budget() {
        let governor = ResourceGovernor::new();
        let wasm: Vec<_> = (0..MAX_WASM_BYTES / MAX_WASM_GUEST_BYTES)
            .map(|_| {
                governor
                    .reserve(ResourceCategory::Wasm, MAX_WASM_GUEST_BYTES)
                    .unwrap()
            })
            .collect();
        let _ast = governor
            .reserve(ResourceCategory::AstStrings, MAX_REMOTE_MEMORY / 2)
            .unwrap();
        let error = governor
            .reserve(ResourceCategory::CompositorCells, 1)
            .unwrap_err();
        assert!(matches!(error, BudgetError::Aggregate { .. }));
        drop(wasm);
    }

    #[test]
    fn mixed_remote_owners_rollback_and_release_exactly() {
        let governor = ResourceGovernor::new();
        let owners = [
            (ResourceCategory::AstStrings, 701usize),
            (ResourceCategory::SceneCells, 37),
            (ResourceCategory::CompositorCells, 809),
            (ResourceCategory::ResourceCache, 907),
            (ResourceCategory::PendingUpdates, 1_009),
            (ResourceCategory::History, 1_103),
            (ResourceCategory::Wasm, 1_201),
            (ResourceCategory::AnimationRegions, 13),
            (ResourceCategory::AuthoredFrames, 17),
            (ResourceCategory::RemoteCollections, 1_303),
        ];
        let mut leases: Vec<_> = owners
            .into_iter()
            .map(|(category, amount)| governor.reserve(category, amount).unwrap())
            .collect();
        let before = governor.total_used();
        let original = leases[1].amount();

        assert!(matches!(
            leases[1].try_resize_with_cost(MAX_SCENE_CELLS + 1, 1),
            Err(BudgetError::Category { .. })
        ));
        assert_eq!(leases[1].amount(), original);
        assert_eq!(governor.total_used(), before);

        while let Some(lease) = leases.pop() {
            drop(lease);
        }
        assert_eq!(governor.total_used(), 0);
        for (category, _) in owners {
            assert_eq!(governor.used(category), 0);
            assert_eq!(governor.count(category), 0);
        }
    }

    #[test]
    fn wasm_enforces_per_guest_and_guest_count_limits() {
        let governor = ResourceGovernor::new();
        assert!(matches!(
            governor.reserve(ResourceCategory::Wasm, MAX_WASM_GUEST_BYTES + 1),
            Err(BudgetError::OwnerTooLarge { .. })
        ));
        let leases: Vec<_> = (0..MAX_WASM_GUESTS)
            .map(|_| governor.reserve(ResourceCategory::Wasm, 1).unwrap())
            .collect();
        assert!(matches!(
            governor.reserve(ResourceCategory::Wasm, 1),
            Err(BudgetError::Count { .. })
        ));
        drop(leases);
    }

    /// The conformance limits are data, not prose: `verification/conformance-limits.json`
    /// is the authoritative table and `docs/spec/05-conformance.md` refers to it rather
    /// than restating the numbers. This asserts every entry matches the constant that
    /// actually enforces it, so a limit cannot be changed in code while the contract
    /// still claims the old value.
    #[test]
    fn documented_limits_match_implementation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let raw = std::fs::read_to_string(root.join("verification/conformance-limits.json"))
            .expect("read conformance-limits.json");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("parse conformance-limits.json");
        let documented: std::collections::BTreeMap<&str, u64> = parsed["limits"]
            .as_array()
            .expect("`limits` is an array")
            .iter()
            .map(|row| {
                (
                    row["name"].as_str().expect("limit has a name"),
                    row["value"].as_u64().expect("limit has a numeric value"),
                )
            })
            .collect();

        for (limit, enforced) in [
            ("Frame", dustnet_core::protocol::MAX_FRAME_SIZE as u64),
            (
                "Control body",
                dustnet_core::protocol::MAX_CONTROL_MESSAGE_SIZE as u64,
            ),
            (
                "INPUT body",
                dustnet_core::protocol::MAX_INPUT_MESSAGE_SIZE as u64,
            ),
            (
                "PAGE body",
                dustnet_core::protocol::MAX_PAGE_MESSAGE_SIZE as u64,
            ),
            (
                "UPDATE body",
                dustnet_core::protocol::MAX_LIVE_UPDATE_SIZE as u64,
            ),
            (
                "WASM module",
                dustnet_core::protocol::MAX_WASM_MODULE_SIZE as u64,
            ),
            ("Aggregate scene cells", MAX_SCENE_CELLS as u64),
            ("Aggregate tracked remote memory", MAX_REMOTE_MEMORY as u64),
            ("WASM guest memory", MAX_WASM_GUEST_BYTES as u64),
            (
                "WASM guests per page",
                dustnet_core::parser::MAX_WASM_INSTANCES as u64,
            ),
            (
                "Staged WASM guests during navigation",
                MAX_WASM_GUESTS as u64,
            ),
            ("Aggregate WASM memory", MAX_WASM_BYTES as u64),
            (
                "Sessions per origin",
                dustnet_core::session::MAX_SESSIONS_PER_SITE as u64,
            ),
            (
                "Sessions total",
                dustnet_core::session::MAX_TOTAL_SESSIONS as u64,
            ),
            ("Session token", dustnet_core::session::MAX_TOKEN_LEN as u64),
            ("Session scope", dustnet_core::session::MAX_SCOPE_LEN as u64),
            (
                "PAGE path",
                dustnet_core::protocol::MAX_PAGE_PATH_LEN as u64,
            ),
            (
                "Animation regions",
                dustnet_core::parser::MAX_ANIMATE_REGIONS as u64,
            ),
            (
                "Authored frames",
                dustnet_core::parser::MAX_ANIMATION_FRAMES as u64,
            ),
        ] {
            let listed = documented
                .get(limit)
                .unwrap_or_else(|| panic!("conformance-limits.json has no `{limit}` entry"));
            assert_eq!(
                *listed, enforced,
                "`{limit}` is documented as {listed} but the code enforces {enforced}"
            );
        }
    }
}
