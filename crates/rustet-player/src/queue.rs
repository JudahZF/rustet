//! The play queue (spec §4.2): pure data and rules, no I/O.
//!
//! Invariants, kept by every operation:
//! - `current` is `Some` and in range exactly when the queue is not empty;
//! - in the upcoming part (after `current`) no user, context or station item follows an autoplay item;
//! - shuffle off restores the order from before shuffle on, with the current item kept.

use crate::model::{
    CatalogId, Container, ContainerKind, InsertPosition, ItemKind, ItemSource, QueueItem,
    QueueItemIn, QueueView, Repeat,
};
use crate::snapshot::{Snapshot, SnapshotItem};

/// Upcoming items at or below which autoplay and stations fetch more.
pub const REFILL_THRESHOLD: usize = 2;
/// Most upcoming autoplay items kept (MusicKit limit), and the autoplay seed count.
pub const AUTOPLAY_CAP: usize = 10;
/// Tracks asked from a station per refill.
pub const STATION_BATCH: usize = 10;

/// Why the player moves on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// The track ended by itself: Repeat One replays it.
    Natural,
    /// The user pressed Next.
    Manual,
}

/// More tracks the queue wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refill {
    Station { id: String },
    Autoplay { seeds: Vec<CatalogId> },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    #[error("no queue item with uid {0}")]
    UnknownUid(u64),
    #[error("start index {index} is outside a queue of {len}")]
    BadIndex { index: usize, len: usize },
}

#[derive(Debug)]
pub struct Queue {
    items: Vec<QueueItem>,
    /// Uids in the order from before shuffle on.
    original: Option<Vec<u64>>,
    current: Option<usize>,
    repeat: Repeat,
    shuffle: bool,
    autoplay: bool,
    context: Option<Container>,
    next_uid: u64,
    /// Bumped when the queue is replaced: refills for an older queue are dropped.
    epoch: u64,
    /// Bumped on every change the UI must redraw.
    version: u64,
    rng: fastrand::Rng,
}

impl Queue {
    pub fn new(seed: u64) -> Self {
        Self {
            items: Vec::new(),
            original: None,
            current: None,
            repeat: Repeat::Off,
            shuffle: false,
            autoplay: true,
            context: None,
            next_uid: 1,
            epoch: 0,
            version: 0,
            rng: fastrand::Rng::with_seed(seed),
        }
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.current.map(|i| &self.items[i])
    }

    pub fn upcoming(&self) -> &[QueueItem] {
        match self.current {
            Some(i) => &self.items[i + 1..],
            None => &[],
        }
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn autoplay(&self) -> bool {
        self.autoplay
    }

    pub fn context(&self) -> Option<&Container> {
        self.context.as_ref()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn view(&self) -> QueueView {
        QueueView {
            items: self.items.clone(),
            current_index: self.current,
        }
    }

    pub fn index_of(&self, uid: u64) -> Option<usize> {
        self.items.iter().position(|i| i.uid == uid)
    }

    fn make(&mut self, item: QueueItemIn, source: ItemSource) -> QueueItem {
        let uid = self.next_uid;
        self.next_uid += 1;
        QueueItem {
            uid,
            id: item.id,
            library_id: item.library_id,
            kind: item.kind,
            source,
            meta: item.meta,
            container_id: item.container_id,
        }
    }

    fn changed(&mut self) {
        self.version += 1;
    }

    /// Play now: replaces the queue. With shuffle the start item comes first. `shuffle: None`
    /// keeps the current shuffle mode.
    pub fn set(
        &mut self,
        items: Vec<QueueItemIn>,
        start: usize,
        context: Option<Container>,
        shuffle: Option<bool>,
    ) -> Result<(), QueueError> {
        if !items.is_empty() && start >= items.len() {
            return Err(QueueError::BadIndex {
                index: start,
                len: items.len(),
            });
        }
        let source = if context.is_some() {
            ItemSource::Context
        } else {
            ItemSource::User
        };
        let source = match context.as_ref().map(|c| c.kind) {
            Some(ContainerKind::Station) => ItemSource::Station,
            _ => source,
        };
        self.items = items.into_iter().map(|i| self.make(i, source)).collect();
        self.context = context;
        self.shuffle = shuffle.unwrap_or(self.shuffle);
        self.original = None;
        self.current = (!self.items.is_empty()).then_some(start);
        self.epoch += 1;
        if self.shuffle {
            self.original = Some(self.items.iter().map(|i| i.uid).collect());
        }
        if self.shuffle && !self.items.is_empty() {
            // The chosen item plays first; the rest is shuffled behind it.
            let first = self.items.remove(start);
            self.items.insert(0, first);
            self.current = Some(0);
            self.shuffle_upcoming();
        }
        self.changed();
        Ok(())
    }

    /// Play next / play later. Returns the new uids.
    pub fn insert(&mut self, items: Vec<QueueItemIn>, position: InsertPosition) -> Vec<u64> {
        if items.is_empty() {
            return Vec::new();
        }
        let new: Vec<QueueItem> = items
            .into_iter()
            .map(|i| self.make(i, ItemSource::User))
            .collect();
        let uids: Vec<u64> = new.iter().map(|i| i.uid).collect();
        let at = match (self.current, position) {
            (None, _) => 0,
            (Some(c), InsertPosition::Next) => c + 1,
            (Some(_), InsertPosition::Later) => {
                self.first_upcoming_autoplay().unwrap_or(self.items.len())
            }
        };
        if let Some(original) = &mut self.original {
            let at = match (self.current, position) {
                (Some(c), InsertPosition::Next) => {
                    let cur = self.items[c].uid;
                    original
                        .iter()
                        .position(|&u| u == cur)
                        .map_or(original.len(), |p| p + 1)
                }
                _ => {
                    let autoplay: Vec<u64> = self
                        .items
                        .iter()
                        .filter(|i| i.source == ItemSource::Autoplay)
                        .map(|i| i.uid)
                        .collect();
                    original
                        .iter()
                        .position(|u| autoplay.contains(u))
                        .unwrap_or(original.len())
                }
            };
            original.splice(at..at, uids.iter().copied());
        }
        self.items.splice(at..at, new);
        if self.current.is_none() {
            self.current = Some(0);
        }
        self.changed();
        uids
    }

    /// Index of the first autoplay item after `current`.
    fn first_upcoming_autoplay(&self) -> Option<usize> {
        let from = self.current.map_or(0, |c| c + 1);
        self.items[from..]
            .iter()
            .position(|i| i.source == ItemSource::Autoplay)
            .map(|p| p + from)
    }

    /// Removes items. When the current item goes, the item after it becomes current (or the new
    /// last item when it was last). Returns whether the current item was removed.
    pub fn remove(&mut self, uids: &[u64]) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        let cur_uid = self.items[cur].uid;
        let removed_current = uids.contains(&cur_uid);
        let before = self.items[..cur]
            .iter()
            .filter(|i| uids.contains(&i.uid))
            .count();
        let len = self.items.len();
        self.items.retain(|i| !uids.contains(&i.uid));
        if let Some(original) = &mut self.original {
            original.retain(|u| !uids.contains(u));
        }
        if self.items.len() == len {
            return false;
        }
        self.current = if self.items.is_empty() {
            None
        } else {
            Some((cur - before).min(self.items.len() - 1))
        };
        self.changed();
        removed_current
    }

    /// Moves an item to `to_index` (its index after the move, clamped). A user item cannot move
    /// behind autoplay items; an autoplay item moved in front of user items becomes a user item.
    pub fn move_item(&mut self, uid: u64, to_index: usize) -> Result<(), QueueError> {
        let from = self.index_of(uid).ok_or(QueueError::UnknownUid(uid))?;
        let cur_uid = self.current.map(|c| self.items[c].uid);
        let item = self.items.remove(from);
        let to = to_index.min(self.items.len());
        self.items.insert(to, item);
        self.current = cur_uid.and_then(|u| self.index_of(u));
        let upcoming_from = self.current.map_or(0, |c| c + 1);
        if to >= upcoming_from
            && self.items[to].source == ItemSource::Autoplay
            && self.items[to + 1..]
                .iter()
                .any(|i| i.source != ItemSource::Autoplay)
        {
            self.items[to].source = ItemSource::User;
        }
        // A user item dropped among autoplay items lands in front of them.
        self.normalize_upcoming();
        self.changed();
        Ok(())
    }

    /// Makes an item current. Returns its index.
    pub fn jump(&mut self, uid: u64) -> Result<usize, QueueError> {
        let i = self.index_of(uid).ok_or(QueueError::UnknownUid(uid))?;
        self.advance_to(i);
        Ok(i)
    }

    /// Removes every upcoming item. Pending refills are dropped.
    pub fn clear_upcoming(&mut self) {
        let Some(c) = self.current else { return };
        if c + 1 == self.items.len() {
            return;
        }
        let gone: Vec<u64> = self.items.drain(c + 1..).map(|i| i.uid).collect();
        if let Some(original) = &mut self.original {
            original.retain(|u| !gone.contains(u));
        }
        self.epoch += 1;
        self.changed();
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        if self.repeat != repeat {
            self.repeat = repeat;
            self.changed();
        }
    }

    /// On: Fisher–Yates on the upcoming user items (autoplay items stay last). Off: the order
    /// from before shuffle on, current item kept.
    pub fn set_shuffle(&mut self, on: bool) {
        if self.shuffle == on {
            return;
        }
        self.shuffle = on;
        if on {
            self.original = Some(self.items.iter().map(|i| i.uid).collect());
            self.shuffle_upcoming();
        } else if let Some(original) = self.original.take() {
            let cur_uid = self.current.map(|c| self.items[c].uid);
            let rank = |uid: u64| {
                original
                    .iter()
                    .position(|&u| u == uid)
                    .unwrap_or(usize::MAX)
            };
            self.items.sort_by_key(|i| rank(i.uid));
            self.current = cur_uid.and_then(|u| self.index_of(u));
            self.normalize_upcoming();
        }
        self.changed();
    }

    fn shuffle_upcoming(&mut self) {
        let from = self.current.map_or(0, |c| c + 1);
        let to = self.first_upcoming_autoplay().unwrap_or(self.items.len());
        let block = &mut self.items[from..to];
        for i in (1..block.len()).rev() {
            block.swap(i, self.rng.usize(..=i));
        }
    }

    /// Stable partition of the upcoming part: non-autoplay items first.
    fn normalize_upcoming(&mut self) {
        let from = self.current.map_or(0, |c| c + 1);
        self.items[from..].sort_by_key(|i| i.source == ItemSource::Autoplay);
    }

    /// Autoplay off drops the upcoming autoplay items and pending refills.
    pub fn set_autoplay(&mut self, on: bool) {
        if self.autoplay == on {
            return;
        }
        self.autoplay = on;
        if !on {
            let from = self.current.map_or(0, |c| c + 1);
            let gone: Vec<u64> = self.items[from..]
                .iter()
                .filter(|i| i.source == ItemSource::Autoplay)
                .map(|i| i.uid)
                .collect();
            self.remove_uids(&gone);
            self.epoch += 1;
        }
        self.changed();
    }

    fn remove_uids(&mut self, uids: &[u64]) {
        let cur_uid = self.current.map(|c| self.items[c].uid);
        self.items.retain(|i| !uids.contains(&i.uid));
        if let Some(original) = &mut self.original {
            original.retain(|u| !uids.contains(u));
        }
        self.current = cur_uid.and_then(|u| self.index_of(u));
    }

    /// The item to play after the current one, or `None` at the end.
    pub fn next_index(&self, why: Advance) -> Option<usize> {
        let c = self.current?;
        if why == Advance::Natural && self.repeat == Repeat::One {
            return Some(c);
        }
        if c + 1 < self.items.len() {
            Some(c + 1)
        } else if self.repeat == Repeat::All {
            Some(0)
        } else {
            None
        }
    }

    /// The item before the current one (no wrap).
    pub fn previous_index(&self) -> Option<usize> {
        self.current.filter(|&c| c > 0).map(|c| c - 1)
    }

    /// Makes `index` current. Going back can bring played autoplay items into the upcoming
    /// part; user items move in front of them, where play later would have put them.
    pub fn advance_to(&mut self, index: usize) {
        debug_assert!(index < self.items.len());
        if self.current != Some(index) {
            self.current = Some(index);
            self.normalize_upcoming();
            self.changed();
        }
    }

    /// What to fetch when the queue runs low. Autoplay is off with repeat, which never runs out.
    pub fn refill(&self) -> Option<Refill> {
        let upcoming = self.upcoming();
        if upcoming.len() > REFILL_THRESHOLD {
            return None;
        }
        match &self.context {
            Some(c) if c.kind == ContainerKind::Station => {
                Some(Refill::Station { id: c.id.clone() })
            }
            Some(c) if c.kind == ContainerKind::LiveStation => None,
            _ if self.autoplay && self.repeat == Repeat::Off && !self.items.is_empty() => {
                let auto = upcoming
                    .iter()
                    .filter(|i| i.source == ItemSource::Autoplay)
                    .count();
                // Apple's autoplay station takes songs only: a music video as a seed is a 500.
                let seeds: Vec<_> = self.items[self.items.len().saturating_sub(AUTOPLAY_CAP)..]
                    .iter()
                    .filter(|i| i.kind == ItemKind::Song)
                    .map(|i| i.id.clone())
                    .collect();
                (auto < AUTOPLAY_CAP && !seeds.is_empty()).then_some(Refill::Autoplay { seeds })
            }
            _ => None,
        }
    }

    /// Appends fetched tracks for `epoch`. Returns false when the queue was replaced meanwhile.
    pub fn append_refill(
        &mut self,
        epoch: u64,
        items: Vec<QueueItemIn>,
        source: ItemSource,
    ) -> bool {
        if epoch != self.epoch || items.is_empty() {
            return false;
        }
        let new: Vec<QueueItem> = items.into_iter().map(|i| self.make(i, source)).collect();
        if let Some(original) = &mut self.original {
            original.extend(new.iter().map(|i| i.uid));
        }
        self.items.extend(new);
        if source == ItemSource::Autoplay {
            let from = self.current.map_or(0, |c| c + 1);
            let extra: Vec<u64> = self.items[from..]
                .iter()
                .filter(|i| i.source == ItemSource::Autoplay)
                .skip(AUTOPLAY_CAP)
                .map(|i| i.uid)
                .collect();
            self.remove_uids(&extra);
        }
        if self.current.is_none() {
            self.current = Some(0);
        }
        self.changed();
        true
    }

    pub fn to_snapshot(&self, position_ms: u64, saved_at: u64) -> Snapshot {
        let original = self
            .original
            .as_ref()
            .map(|o| o.iter().filter_map(|&u| self.index_of(u)).collect());
        Snapshot {
            v: crate::snapshot::SNAPSHOT_VERSION,
            items: self.items.iter().map(SnapshotItem::from).collect(),
            current: self.current,
            original,
            repeat: self.repeat,
            shuffle: self.shuffle,
            autoplay: self.autoplay,
            context: self.context.clone(),
            position_ms,
            saved_at,
        }
    }

    /// Restores a snapshot; uids are new. An out-of-range index is clamped.
    pub fn restore(&mut self, snap: Snapshot) {
        self.items = snap
            .items
            .into_iter()
            .map(|s| {
                let src = s.source;
                self.make(s.item, src)
            })
            .collect();
        self.current = match (self.items.len(), snap.current) {
            (0, _) => None,
            (n, c) => Some(c.unwrap_or(0).min(n - 1)),
        };
        self.repeat = snap.repeat;
        self.shuffle = snap.shuffle;
        self.autoplay = snap.autoplay;
        self.context = snap.context;
        self.original = match (snap.shuffle, snap.original) {
            (true, Some(o)) => Some(
                o.into_iter()
                    .filter_map(|i| self.items.get(i).map(|it| it.uid))
                    .collect(),
            ),
            (true, None) => Some(self.items.iter().map(|i| i.uid).collect()),
            (false, _) => None,
        };
        self.normalize_upcoming();
        self.epoch += 1;
        self.changed();
    }

    /// Checks the invariants (tests).
    #[cfg(test)]
    fn check(&self) -> Result<(), String> {
        match self.current {
            None if !self.items.is_empty() => {
                return Err("no current item in a non-empty queue".into());
            }
            Some(c) if c >= self.items.len() => {
                return Err(format!("current {c} out of {}", self.items.len()));
            }
            _ => {}
        }
        let mut seen_auto = false;
        for i in self.upcoming() {
            if i.source == ItemSource::Autoplay {
                seen_auto = true;
            } else if seen_auto {
                return Err(format!("user item {} after an autoplay item", i.uid));
            }
        }
        let mut uids: Vec<u64> = self.items.iter().map(|i| i.uid).collect();
        uids.sort_unstable();
        uids.dedup();
        if uids.len() != self.items.len() {
            return Err("duplicate uid".into());
        }
        if self.shuffle != self.original.is_some() {
            return Err("original order kept without shuffle, or lost with it".into());
        }
        if let Some(o) = &self.original {
            let mut a = o.clone();
            a.sort_unstable();
            if a != uids {
                return Err("original order does not hold the same items".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, TrackMeta};
    use proptest::prelude::*;

    fn songs(ids: &[&str]) -> Vec<QueueItemIn> {
        ids.iter()
            .map(|id| QueueItemIn {
                id: CatalogId((*id).into()),
                library_id: None,
                kind: ItemKind::Song,
                meta: TrackMeta::default(),
                container_id: None,
            })
            .collect()
    }

    fn ids(q: &Queue) -> Vec<&str> {
        q.items().iter().map(|i| i.id.as_str()).collect()
    }

    fn album() -> Option<Container> {
        Some(Container {
            kind: ContainerKind::Album,
            id: "al".into(),
            name: None,
        })
    }

    fn queue(list: &[&str], start: usize) -> Queue {
        let mut q = Queue::new(7);
        q.set(songs(list), start, album(), None).unwrap();
        q
    }

    #[test]
    fn set_replaces_and_marks_the_context() {
        let mut q = queue(&["a", "b"], 1);
        assert_eq!(q.current().unwrap().id.as_str(), "b");
        assert_eq!(q.items()[0].source, ItemSource::Context);
        let epoch = q.epoch();
        q.set(songs(&["x"]), 0, None, None).unwrap();
        assert_eq!(ids(&q), ["x"]);
        assert_eq!(q.items()[0].source, ItemSource::User);
        assert!(q.epoch() > epoch);
        assert_eq!(
            q.set(songs(&["x"]), 1, None, None),
            Err(QueueError::BadIndex { index: 1, len: 1 })
        );
        q.set(Vec::new(), 0, None, None).unwrap();
        assert!(q.is_empty() && q.current_index().is_none());
    }

    #[test]
    fn shuffled_set_keeps_the_chosen_item_first() {
        for seed in 0..20 {
            let mut q = Queue::new(seed);
            q.set(songs(&["a", "b", "c", "d", "e"]), 3, None, Some(true))
                .unwrap();
            assert_eq!(q.current_index(), Some(0));
            assert_eq!(q.current().unwrap().id.as_str(), "d");
            q.set_shuffle(false);
            assert_eq!(ids(&q), ["a", "b", "c", "d", "e"]);
            assert_eq!(
                q.current().unwrap().id.as_str(),
                "d",
                "shuffle off keeps the current item"
            );
        }
    }

    #[test]
    fn play_next_and_later() {
        let mut q = queue(&["a", "b", "c"], 0);
        q.insert(songs(&["n"]), InsertPosition::Next);
        q.insert(songs(&["l"]), InsertPosition::Later);
        assert_eq!(ids(&q), ["a", "n", "b", "c", "l"]);
        // Later goes before autoplay items.
        assert!(q.append_refill(q.epoch(), songs(&["auto"]), ItemSource::Autoplay));
        q.insert(songs(&["l2"]), InsertPosition::Later);
        assert_eq!(ids(&q), ["a", "n", "b", "c", "l", "l2", "auto"]);
        // Play next on an empty queue makes it current.
        let mut e = Queue::new(1);
        e.insert(songs(&["x"]), InsertPosition::Next);
        assert_eq!(e.current_index(), Some(0));
    }

    #[test]
    fn removing_the_current_item_advances() {
        let mut q = queue(&["a", "b", "c"], 1);
        let b = q.items()[1].uid;
        assert!(q.remove(&[b]));
        assert_eq!(q.current().unwrap().id.as_str(), "c");
        let c = q.items()[1].uid;
        assert!(
            q.remove(&[c]),
            "the last item: the new last item becomes current"
        );
        assert_eq!(q.current().unwrap().id.as_str(), "a");
        let a = q.items()[0].uid;
        assert!(q.remove(&[a]));
        assert!(q.current_index().is_none());
        // Removing an earlier item keeps the current one.
        let mut q = queue(&["a", "b", "c"], 2);
        let a = q.items()[0].uid;
        assert!(!q.remove(&[a]));
        assert_eq!(q.current().unwrap().id.as_str(), "c");
    }

    #[test]
    fn move_keeps_user_items_before_autoplay() {
        let mut q = queue(&["a", "b", "c"], 0);
        q.append_refill(q.epoch(), songs(&["x", "y"]), ItemSource::Autoplay);
        let b = q.items()[1].uid;
        q.move_item(b, 10).unwrap();
        assert_eq!(
            ids(&q),
            ["a", "c", "b", "x", "y"],
            "clamped in front of the autoplay items"
        );
        let y = q.items()[4].uid;
        q.move_item(y, 1).unwrap();
        assert_eq!(ids(&q), ["a", "y", "c", "b", "x"]);
        assert_eq!(
            q.items()[1].source,
            ItemSource::User,
            "an autoplay item moved in front becomes a user item"
        );
        assert_eq!(q.move_item(999, 0), Err(QueueError::UnknownUid(999)));
        // Moving the current item keeps it current.
        let a = q.items()[0].uid;
        q.move_item(a, 2).unwrap();
        assert_eq!(q.current().unwrap().id.as_str(), "a");
        q.check().unwrap();
    }

    #[test]
    fn jump_and_clear_upcoming() {
        let mut q = queue(&["a", "b", "c"], 0);
        let c = q.items()[2].uid;
        assert_eq!(q.jump(c), Ok(2));
        q.jump(q.items()[0].uid).unwrap();
        let epoch = q.epoch();
        q.clear_upcoming();
        assert_eq!(ids(&q), ["a"]);
        assert!(q.epoch() > epoch, "a refill in flight is dropped");
    }

    #[test]
    fn repeat_modes() {
        let mut q = queue(&["a", "b"], 1);
        assert_eq!(q.next_index(Advance::Natural), None);
        q.set_repeat(Repeat::All);
        assert_eq!(q.next_index(Advance::Natural), Some(0));
        q.set_repeat(Repeat::One);
        assert_eq!(
            q.next_index(Advance::Natural),
            Some(1),
            "repeat one replays on a natural end"
        );
        assert_eq!(
            q.next_index(Advance::Manual),
            None,
            "next still advances (and does not wrap)"
        );
        q.advance_to(0);
        assert_eq!(q.next_index(Advance::Manual), Some(1));
        assert_eq!(q.previous_index(), None);
    }

    #[test]
    fn autoplay_threshold_cap_and_seeds() {
        let mut q = queue(&["a", "b", "c", "d"], 0);
        assert_eq!(q.refill(), None, "three upcoming items");
        q.advance_to(1);
        let Some(Refill::Autoplay { seeds }) = q.refill() else {
            panic!("two upcoming items refill")
        };
        assert_eq!(seeds.len(), 4);
        let many: Vec<String> = (0..15).map(|i| format!("x{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(q.append_refill(q.epoch(), songs(&many), ItemSource::Autoplay));
        let auto = q
            .upcoming()
            .iter()
            .filter(|i| i.source == ItemSource::Autoplay)
            .count();
        assert_eq!(auto, AUTOPLAY_CAP);
        assert!(
            !q.append_refill(q.epoch() + 1, songs(&["late"]), ItemSource::Autoplay),
            "stale refill dropped"
        );
        // The seeds are the last ten items.
        q.advance_to(q.items().len() - 1);
        let Some(Refill::Autoplay { seeds }) = q.refill() else {
            panic!()
        };
        assert_eq!(seeds.len(), AUTOPLAY_CAP);
        assert_eq!(seeds.last().unwrap().as_str(), "x9");
        // Off with repeat, off when disabled, and disabling drops the autoplay items.
        q.set_repeat(Repeat::All);
        assert_eq!(q.refill(), None);
        q.set_repeat(Repeat::Off);
        q.advance_to(1);
        q.set_autoplay(false);
        assert!(q.items().iter().all(|i| i.source != ItemSource::Autoplay));
        assert_eq!(q.refill(), None);
    }

    #[test]
    fn stations_refill_with_station_items() {
        let mut q = Queue::new(1);
        let station = Container {
            kind: ContainerKind::Station,
            id: "ra.1".into(),
            name: None,
        };
        q.set(Vec::new(), 0, Some(station), None).unwrap();
        assert_eq!(q.refill(), Some(Refill::Station { id: "ra.1".into() }));
        assert!(q.append_refill(
            q.epoch(),
            songs(&["s1", "s2", "s3", "s4"]),
            ItemSource::Station
        ));
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.items()[0].source, ItemSource::Station);
        assert_eq!(q.refill(), None, "three upcoming");
        q.advance_to(1);
        assert!(matches!(q.refill(), Some(Refill::Station { .. })));
    }

    #[test]
    fn snapshot_round_trip_keeps_order_modes_and_shuffle_origin() {
        let mut q = queue(&["a", "b", "c", "d"], 1);
        q.set_shuffle(true);
        q.set_repeat(Repeat::All);
        let order = ids(&q).iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let snap = q.to_snapshot(42, 7);
        let mut r = Queue::new(3);
        r.restore(snap);
        assert_eq!(ids(&r), order);
        assert_eq!(r.current().unwrap().id.as_str(), "b");
        assert_eq!((r.repeat(), r.shuffle()), (Repeat::All, true));
        r.set_shuffle(false);
        assert_eq!(ids(&r), ["a", "b", "c", "d"]);
        r.check().unwrap();
    }

    #[derive(Debug, Clone)]
    enum Op {
        Set(u8, u8, Option<bool>),
        Insert(u8, bool),
        Remove(u8),
        Move(u8, u8),
        Jump(u8),
        Clear,
        Shuffle(bool),
        Repeat(u8),
        Autoplay(bool),
        Refill(u8),
        Next,
        Previous,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..8, any::<u8>(), any::<Option<bool>>()).prop_map(|(n, s, sh)| Op::Set(n, s, sh)),
            (1u8..4, any::<bool>()).prop_map(|(n, next)| Op::Insert(n, next)),
            any::<u8>().prop_map(Op::Remove),
            (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Move(a, b)),
            any::<u8>().prop_map(Op::Jump),
            Just(Op::Clear),
            any::<bool>().prop_map(Op::Shuffle),
            (0u8..3).prop_map(Op::Repeat),
            any::<bool>().prop_map(Op::Autoplay),
            (0u8..12).prop_map(Op::Refill),
            Just(Op::Next),
            Just(Op::Previous),
        ]
    }

    fn apply(q: &mut Queue, op: Op, n: &mut u32) {
        let mut fresh = |k: u8| -> Vec<QueueItemIn> {
            (0..k)
                .map(|_| {
                    *n += 1;
                    songs(&[&format!("t{n}")]).remove(0)
                })
                .collect()
        };
        let pick =
            |q: &Queue, i: u8| (!q.is_empty()).then(|| q.items()[i as usize % q.items().len()].uid);
        match op {
            Op::Set(k, s, sh) => {
                let items = fresh(k);
                let start = if k == 0 { 0 } else { s as usize % k as usize };
                q.set(items, start, album(), sh).unwrap();
            }
            Op::Insert(k, next) => {
                let items = fresh(k);
                q.insert(
                    items,
                    if next {
                        InsertPosition::Next
                    } else {
                        InsertPosition::Later
                    },
                );
            }
            Op::Remove(i) => {
                if let Some(u) = pick(q, i) {
                    q.remove(&[u]);
                }
            }
            Op::Move(i, to) => {
                if let Some(u) = pick(q, i) {
                    q.move_item(u, to as usize % (q.items().len() + 1)).unwrap();
                }
            }
            Op::Jump(i) => {
                if let Some(u) = pick(q, i) {
                    q.jump(u).unwrap();
                }
            }
            Op::Clear => q.clear_upcoming(),
            Op::Shuffle(on) => q.set_shuffle(on),
            Op::Repeat(r) => q.set_repeat([Repeat::Off, Repeat::All, Repeat::One][r as usize]),
            Op::Autoplay(on) => q.set_autoplay(on),
            Op::Refill(k) => {
                let items = fresh(k);
                q.append_refill(q.epoch(), items, ItemSource::Autoplay);
            }
            Op::Next => {
                if let Some(i) = q.next_index(Advance::Manual) {
                    q.advance_to(i);
                }
            }
            Op::Previous => {
                if let Some(i) = q.previous_index() {
                    q.advance_to(i);
                }
            }
        }
    }

    proptest! {
        #[test]
        fn any_command_sequence_keeps_the_invariants(seed in any::<u64>(), ops in prop::collection::vec(op(), 0..60)) {
            let mut q = Queue::new(seed);
            let mut n = 0;
            for op in ops {
                apply(&mut q, op.clone(), &mut n);
                if let Err(e) = q.check() {
                    return Err(TestCaseError::fail(format!("after {op:?}: {e}")));
                }
            }
        }

        #[test]
        fn shuffle_off_restores_the_original_order(seed in any::<u64>(), ops in prop::collection::vec(op(), 0..40)) {
            let mut q = Queue::new(seed);
            let mut n = 0;
            for op in ops {
                apply(&mut q, op, &mut n);
            }
            q.set_shuffle(false);
            let before: Vec<u64> = q.items().iter().map(|i| i.uid).collect();
            let cur = q.current().map(|i| i.uid);
            q.set_shuffle(true);
            prop_assert_eq!(q.current().map(|i| i.uid), cur, "shuffle on keeps the current item");
            q.set_shuffle(false);
            let after: Vec<u64> = q.items().iter().map(|i| i.uid).collect();
            prop_assert_eq!(after, before);
            prop_assert_eq!(q.current().map(|i| i.uid), cur);
        }
    }
}
