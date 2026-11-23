//! Play history (`player-history.json`): tracks that played at least 30 s or to the end,
//! newest first.

use crate::model::HistoryEntry;

/// Entries kept (spec §4.2; the renderer stored 50 and showed 30).
pub const HISTORY_CAP: usize = 30;

#[derive(Debug, Default)]
pub struct History {
    entries: Vec<HistoryEntry>,
}

impl History {
    /// Reads stored history; unreadable bytes give an empty history.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut entries: Vec<HistoryEntry> = serde_json::from_slice(bytes).unwrap_or_default();
        entries.truncate(HISTORY_CAP);
        Self { entries }
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(&self.entries).expect("history always serialises")
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Adds an entry in front. A repeat of the newest entry is not added again (renderer rule).
    /// Returns whether the history changed.
    pub fn push(&mut self, entry: HistoryEntry) -> bool {
        if self.entries.first().is_some_and(|e| e.id == entry.id) {
            return false;
        }
        self.entries.insert(0, entry);
        self.entries.truncate(HISTORY_CAP);
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CatalogId, ItemKind, TrackMeta};

    fn entry(id: &str, at: u64) -> HistoryEntry {
        HistoryEntry {
            id: CatalogId(id.into()),
            kind: ItemKind::Song,
            meta: TrackMeta::default(),
            played_at: at,
        }
    }

    #[test]
    fn newest_first_capped_and_no_consecutive_repeat() {
        let mut h = History::default();
        assert!(h.push(entry("a", 1)));
        assert!(
            !h.push(entry("a", 2)),
            "the same track twice in a row is one entry"
        );
        assert!(h.push(entry("b", 3)));
        assert!(h.push(entry("a", 4)));
        assert_eq!(
            h.entries()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "a"]
        );
        for i in 0..40 {
            h.push(entry(&i.to_string(), i));
        }
        assert_eq!(h.entries().len(), HISTORY_CAP);
        assert_eq!(h.entries()[0].id.as_str(), "39");
        let back = History::parse(&h.to_json());
        assert_eq!(back.entries(), h.entries());
        assert!(History::parse(b"{").entries().is_empty());
    }
}
