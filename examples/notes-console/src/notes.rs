//! In-memory note store owned by the plugin process.
//!
//! Plugin-owned state stays in the plugin. The host never reaches into it; it
//! only invokes the operations the manifest declares.

use std::sync::{Arc, Mutex};

use serde::Serialize;

/// Hard ceiling on retained notes.
///
/// This is the plugin's own safety limit, not the operator-facing `max_notes`
/// setting: `[plugin.settings]` values are host-owned and are never delivered
/// to the plugin process, so the plugin cannot enforce them itself.
const RETAINED_NOTES: usize = 500;

#[derive(Clone, Debug, Serialize)]
pub struct Note {
    pub id: u64,
    pub text: String,
}

/// Cheap to clone: every clone shares one note list, so each handler closure
/// can own a clone without duplicating state.
#[derive(Clone, Debug, Default)]
pub struct NoteStore {
    inner: Arc<Mutex<Vec<Note>>>,
}

impl NoteStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a note and return it. Oldest notes fall off past
    /// [`RETAINED_NOTES`].
    pub fn add(&self, text: String) -> Note {
        let mut notes = self.lock();
        let id = notes.last().map_or(1, |note| note.id + 1);
        let note = Note { id, text };
        notes.push(note.clone());
        let overflow = notes.len().saturating_sub(RETAINED_NOTES);
        if overflow > 0 {
            notes.drain(..overflow);
        }
        note
    }

    /// Most recent notes first, capped at `limit`.
    pub fn recent(&self, limit: usize) -> Vec<Note> {
        let notes = self.lock();
        notes.iter().rev().take(limit).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// A poisoned lock means a handler panicked mid-update. The note list is a
    /// plain `Vec` with no cross-field invariant, so recovering the data keeps
    /// the plugin usable instead of poisoning every later request.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Note>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_returns_newest_first_within_the_limit() {
        let store = NoteStore::new();
        store.add("first".into());
        store.add("second".into());
        store.add("third".into());

        let recent = store.recent(2);

        assert_eq!(
            recent
                .iter()
                .map(|note| note.text.as_str())
                .collect::<Vec<_>>(),
            vec!["third", "second"]
        );
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn ids_keep_increasing_after_retention_trims_the_oldest() {
        let store = NoteStore::new();
        for index in 0..RETAINED_NOTES + 5 {
            store.add(format!("note {index}"));
        }

        assert_eq!(store.len(), RETAINED_NOTES);
        assert_eq!(store.recent(1)[0].id, (RETAINED_NOTES + 5) as u64);
    }
}
