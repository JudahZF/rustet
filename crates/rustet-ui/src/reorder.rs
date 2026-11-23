//! Drag-to-reorder for lists (the Up Next queue).
//!
//! A row is dragged with a [`DraggedRow`] payload; dropping it on another row moves it there.

/// The drag payload: which row is moving. `label` is drawn under the pointer.
#[derive(Debug, Clone)]
pub struct DraggedRow {
    pub index: usize,
    pub label: gpui::SharedString,
}

/// Moves `items[from]` so it ends up at index `to`; the items between shift by one.
/// Out-of-range indices leave the list unchanged.
pub fn move_item<T>(items: &mut [T], from: usize, to: usize) {
    if from >= items.len() || to >= items.len() {
        return;
    }
    if from < to {
        items[from..=to].rotate_left(1);
    } else {
        items[to..=from].rotate_right(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_down_up_and_ignores_bad_indices() {
        let mut v = vec!['a', 'b', 'c', 'd'];
        move_item(&mut v, 0, 2);
        assert_eq!(v, ['b', 'c', 'a', 'd']);
        move_item(&mut v, 3, 0);
        assert_eq!(v, ['d', 'b', 'c', 'a']);
        move_item(&mut v, 1, 1);
        assert_eq!(v, ['d', 'b', 'c', 'a']);
        move_item(&mut v, 9, 0);
        assert_eq!(v, ['d', 'b', 'c', 'a']);
    }
}
