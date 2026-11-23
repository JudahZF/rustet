//! Geometry of the responsive album grid: as many columns as fit, cards stretched to fill.

use gpui::{Pixels, px};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridGeometry {
    pub columns: usize,
    pub card_width: Pixels,
    pub gap: Pixels,
}

impl GridGeometry {
    /// The layout for a content `width`: cards at least `min_card` wide, `gap` between them.
    pub fn for_width(width: Pixels, min_card: Pixels, gap: Pixels) -> Self {
        let columns = (((width + gap) / (min_card + gap)).floor() as usize).max(1);
        let card_width = ((width - gap * (columns as f32 - 1.)) / columns as f32).max(px(1.));
        Self {
            columns,
            card_width,
            gap,
        }
    }

    pub fn rows(&self, items: usize) -> usize {
        items.div_ceil(self.columns)
    }

    /// Item indices on `row`.
    pub fn row_items(&self, row: usize, items: usize) -> std::ops::Range<usize> {
        let start = row * self.columns;
        start.min(items)..(start + self.columns).min(items)
    }
}

/// A move of the keyboard focus between items laid out in rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Left,
    Right,
    Up,
    Down,
}

/// The item the focus moves to from `ix` among `len` items in rows of `columns`, or `None` at
/// an edge. Down from the last full row goes to the last item.
pub fn step(ix: usize, len: usize, columns: usize, step: Step) -> Option<usize> {
    let columns = columns.max(1);
    let to = match step {
        Step::Left => ix.checked_sub(1)?,
        Step::Right => ix + 1,
        Step::Up => ix.checked_sub(columns)?,
        Step::Down if ix / columns + 1 < len.div_ceil(columns) => (ix + columns).min(len - 1),
        Step::Down => return None,
    };
    (to < len).then_some(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_stop_at_the_edges() {
        // 10 items in rows of 4: 0..4, 4..8, 8..10.
        assert_eq!(step(0, 10, 4, Step::Left), None);
        assert_eq!(step(3, 10, 4, Step::Right), Some(4));
        assert_eq!(step(9, 10, 4, Step::Right), None);
        assert_eq!(step(2, 10, 4, Step::Up), None);
        assert_eq!(step(6, 10, 4, Step::Up), Some(2));
        assert_eq!(step(7, 10, 4, Step::Down), Some(9));
        assert_eq!(step(9, 10, 4, Step::Down), None);
    }

    #[test]
    fn fits_columns_and_fills_the_width() {
        let g = GridGeometry::for_width(px(1000.), px(160.), px(20.));
        assert_eq!(g.columns, 5);
        assert_eq!(g.card_width, px(184.));
        assert_eq!(g.rows(5000), 1000);
        assert_eq!(g.rows(5001), 1001);
        assert_eq!(g.row_items(1000, 5001), 5000..5001);
        // Never fewer than one column, even narrower than one card.
        assert_eq!(
            GridGeometry::for_width(px(100.), px(160.), px(20.)).columns,
            1
        );
    }
}
