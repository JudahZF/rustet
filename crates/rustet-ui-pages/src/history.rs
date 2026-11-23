//! A mirror of the navigation stack that keeps something per history entry: the page area
//! keeps each entry's view, so Back shows the page as it was left (scroll position, selection,
//! shelf pages, focus).
//!
//! `rustet_app::nav::Nav` reports only the current route and whether Back and Forward are
//! possible, so the mirror infers each move: the route on top of the mirrored back stack with
//! Forward now possible is a Back, the top of the forward stack is a Forward, the same route is
//! no move (a reload or "load more"), anything else is a new page.

use rustet_app::route::Route;

/// How the current route was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// A new page: a click, a link.
    Push,
    Back,
    Forward,
    /// Still the same route.
    Stay,
}

struct Slot<T> {
    route: Route,
    kept: Option<T>,
}

/// The mirrored stack. `T` is what an entry keeps (the area: its page view and focus).
pub struct History<T> {
    back: Vec<Slot<T>>,
    here: Slot<T>,
    forward: Vec<Slot<T>>,
    /// Entries further back than this drop what they kept (the page loads again).
    keep: usize,
}

impl<T> History<T> {
    pub fn new(start: Route, keep: usize) -> Self {
        Self {
            back: Vec::new(),
            here: Slot {
                route: start,
                kept: None,
            },
            forward: Vec::new(),
            keep,
        }
    }

    pub fn route(&self) -> &Route {
        &self.here.route
    }

    /// What the current entry keeps.
    pub fn kept(&self) -> Option<&T> {
        self.here.kept.as_ref()
    }

    pub fn kept_mut(&mut self) -> &mut Option<T> {
        &mut self.here.kept
    }

    /// Follows the navigation to `route`. The entry that is left keeps what it had.
    pub fn follow(&mut self, route: &Route, can_back: bool, can_forward: bool) -> Move {
        if *route == self.here.route {
            return Move::Stay;
        }
        let new = |route: &Route| Slot {
            route: route.clone(),
            kept: None,
        };
        let step = if can_forward && self.back.last().is_some_and(|s| s.route == *route) {
            let slot = self.back.pop().unwrap_or_else(|| new(route));
            let left = std::mem::replace(&mut self.here, slot);
            self.forward.push(left);
            Move::Back
        } else if can_back && self.forward.last().is_some_and(|s| s.route == *route) {
            let slot = self.forward.pop().unwrap_or_else(|| new(route));
            let left = std::mem::replace(&mut self.here, slot);
            self.back.push(left);
            Move::Forward
        } else {
            let left = std::mem::replace(&mut self.here, new(route));
            self.back.push(left);
            self.forward.clear();
            Move::Push
        };
        // Nav forgets old entries too; only the recent ones keep a view.
        let old = self.back.len().saturating_sub(self.keep);
        for slot in &mut self.back[..old] {
            slot.kept = None;
        }
        if !can_back {
            self.back.clear();
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str) -> Route {
        Route::Album { id: id.into() }
    }

    #[test]
    fn back_and_forward_return_what_the_entry_kept() {
        let mut h: History<&str> = History::new(Route::New, 8);
        *h.kept_mut() = Some("new, scrolled");
        assert_eq!(h.follow(&album("1"), true, false), Move::Push);
        assert_eq!(h.kept(), None);
        *h.kept_mut() = Some("album 1");
        assert_eq!(h.follow(&Route::New, false, true), Move::Back);
        assert_eq!(h.kept(), Some(&"new, scrolled"));
        assert_eq!(h.follow(&album("1"), true, false), Move::Forward);
        assert_eq!(h.kept(), Some(&"album 1"));
        assert_eq!(
            h.follow(&album("1"), true, false),
            Move::Stay,
            "a reload is no move"
        );
    }

    #[test]
    fn a_link_to_the_previous_page_is_a_new_entry() {
        let mut h: History<u8> = History::new(Route::New, 8);
        h.follow(&album("1"), true, false);
        // nav.go(New) from album 1: Back stays possible, Forward does not.
        assert_eq!(h.follow(&Route::New, true, false), Move::Push);
    }

    #[test]
    fn only_recent_entries_keep_their_view() {
        let mut h: History<usize> = History::new(album("0"), 2);
        for i in 1..=4 {
            *h.kept_mut() = Some(i - 1);
            h.follow(&album(&i.to_string()), true, false);
        }
        assert_eq!(h.follow(&album("3"), true, true), Move::Back);
        assert_eq!(h.kept(), Some(&3));
        h.follow(&album("2"), true, true);
        assert_eq!(h.kept(), Some(&2));
        h.follow(&album("1"), true, true);
        assert_eq!(h.kept(), None, "too far back: loads again");
    }
}
