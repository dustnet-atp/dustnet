//! Rectangle algebra used by layout and composition.
//!
//! `Rect` is the single geometric primitive for every placement,
//! invalidation, and clip used by the renderer. Id/kind metadata
//! rides on `PlacedElement` / `NodeKind`, not on `Rect`.

/// A rectangle in buffer-absolute cell coordinates.
///
/// Empty rectangles (`w == 0 || h == 0`) are treated as "no contribution"
/// by `union` so that accumulating a bbox starting from an empty rect
/// yields the other operand unchanged — important for placement accumulation
/// in `layout_children`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// The rightmost column (exclusive).
    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.w)
    }

    /// The bottom row (exclusive).
    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.h)
    }

    /// Smallest rectangle containing both `self` and `other`. An empty rect
    /// is treated as "no contribution" — union with empty returns the other.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    }

    /// Intersection of `self` and `other`, or `None` if disjoint.
    pub fn intersect(self, other: Rect) -> Option<Rect> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            None
        } else {
            Some(Rect {
                x,
                y,
                w: right - x,
                h: bottom - y,
            })
        }
    }

    /// Translate this rectangle by `(dx, dy)`. Saturating arithmetic keeps
    /// results inside u16 — a translate that would overflow clamps to the
    /// limit, consistent with how the rest of the engine handles coordinates.
    pub fn translate(self, dx: i32, dy: i32) -> Rect {
        let x = (self.x as i32 + dx).clamp(0, u16::MAX as i32) as u16;
        let y = (self.y as i32 + dy).clamp(0, u16::MAX as i32) as u16;
        Rect {
            x,
            y,
            w: self.w,
            h: self.h,
        }
    }

    pub fn contains_point(&self, x: u16, y: u16) -> bool {
        !self.is_empty() && x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_identity_with_empty() {
        let a = Rect::new(5, 10, 0, 0);
        let b = Rect::new(20, 30, 4, 2);
        assert_eq!(a.union(b), b);
        assert_eq!(b.union(a), b);
    }

    #[test]
    fn union_merges_disjoint() {
        let a = Rect::new(0, 0, 5, 2);
        let b = Rect::new(10, 10, 3, 3);
        assert_eq!(a.union(b), Rect::new(0, 0, 13, 13));
    }

    #[test]
    fn intersect_overlapping() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(b), Some(Rect::new(5, 5, 5, 5)));
    }

    #[test]
    fn intersect_disjoint_returns_none() {
        let a = Rect::new(0, 0, 5, 5);
        let b = Rect::new(10, 10, 5, 5);
        assert_eq!(a.intersect(b), None);
    }

    #[test]
    fn translate_moves_origin() {
        let r = Rect::new(3, 4, 10, 5);
        assert_eq!(r.translate(2, -1), Rect::new(5, 3, 10, 5));
    }

    #[test]
    fn translate_saturates_at_zero() {
        let r = Rect::new(1, 1, 10, 5);
        assert_eq!(r.translate(-100, -100), Rect::new(0, 0, 10, 5));
    }

    #[test]
    fn contains_point_inside() {
        let r = Rect::new(5, 5, 10, 10);
        assert!(r.contains_point(5, 5));
        assert!(r.contains_point(14, 14));
        assert!(!r.contains_point(15, 15));
        assert!(!r.contains_point(4, 4));
    }

    #[test]
    fn empty_rect_contains_nothing() {
        let r = Rect::new(5, 5, 0, 0);
        assert!(!r.contains_point(5, 5));
    }
}
