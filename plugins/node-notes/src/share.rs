//! The mesh channel: what this plugin puts on the wire, and what it does with
//! what comes back.
//!
//! One declared channel, `node-notes.v1`, carrying four message kinds:
//!
//! | `message_kind`  | Direction | Target        | Body |
//! | --------------- | --------- | ------------- | ---- |
//! | `note`          | announce  | broadcast     | [`SharedNote`] |
//! | `retract`       | announce  | broadcast     | [`RetractPayload`] |
//! | `sync_request`  | ask       | one peer      | `{}` |
//! | `sync`          | answer    | one peer      | [`SyncPayload`] |
//!
//! ## What the host actually does with these
//!
//! Read this before assuming a note reaches the mesh. The host's behaviour,
//! from `crates/tdcc-host-runtime/src/mesh/plugin_mesh.rs`:
//!
//! - An outbound message is dropped unless the plugin's manifest declares the
//!   channel, then stamped with this node's peer id **only if the plugin left
//!   `source_peer_id` empty**, given a message id, and written to every
//!   currently connected peer.
//! - A receiving node delivers the frame to the plugin registered under the
//!   *sender's* plugin id when the target is empty or is that node, and
//!   forwards it onward only when the message names a different target.
//! - An untargeted broadcast is therefore **one hop**: direct peers see it,
//!   their peers do not. A targeted message gets one forwarding hop toward its
//!   target.
//! - Frames are deduplicated by message id for 120 seconds and capped at 10 MB.
//!
//! Two consequences shape everything below. First, **this node never relays
//! another node's notes** — it could not usefully do so anyway, and forwarding
//! third-party text would launder its provenance. Second, `sync_request` /
//! `sync` exists because one-hop broadcast means a node that was offline when
//! a note was written would otherwise never learn of it; on `peer_up` we ask
//! the new peer directly.
//!
//! ## `source_peer_id` is a claim
//!
//! The host stamps the source only when it is blank, and the receiving side
//! does not check the frame's `source_peer_id` against the connection it
//! arrived on. A peer running modified code can therefore put any id it likes
//! on its own messages. Everything in this plugin treats that id as a label for
//! grouping and rate limiting, never as an authorization — and every note view
//! says the id is self-declared.

use serde::{Deserialize, Serialize};

use crate::note::{Kind, Note, Origin, Subject};

/// The one mesh channel this plugin declares.
pub const CHANNEL: &str = "node-notes.v1";

pub const KIND_NOTE: &str = "note";
pub const KIND_RETRACT: &str = "retract";
pub const KIND_SYNC_REQUEST: &str = "sync_request";
pub const KIND_SYNC: &str = "sync";

/// Notes carried in one `sync` answer.
///
/// A peer asking for a sync is asking for context, not for a database. Sixty
/// four notes is more than a human reads in one sitting and keeps the frame
/// two orders of magnitude below the host's 10 MB cap.
pub const MAX_SYNC_NOTES: usize = 64;

/// Longest note body accepted off the wire, before the local cap is applied.
///
/// A first bound on allocation that does not depend on this node's own
/// `--max-note-chars`: a peer cannot make this node build a megabyte string
/// just to truncate it afterwards.
pub const MAX_WIRE_TEXT_CHARS: usize = 8_000;

/// A note as it travels between nodes.
///
/// Deliberately not [`Note`]: `origin`, `shared`, and `truncated` are local
/// bookkeeping, and a peer has no business asserting any of them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SharedNote {
    pub id: String,
    /// `mesh`, `node:local`, or `node:<peer-id>`. `node:local` is resolved to
    /// the sending peer on arrival.
    pub subject: String,
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: String,
    pub created_at: u64,
    pub expires_at: u64,
}

impl SharedNote {
    /// Render a local note for the wire.
    ///
    /// `local_peer_id` replaces a `node:local` subject when this node knows its
    /// own mesh id, so the note stays about the right machine after it lands.
    pub fn of(note: &Note, local_peer_id: Option<&str>) -> Self {
        let subject = match local_peer_id {
            Some(peer_id) => note.subject.clone().resolve_local(peer_id),
            None => note.subject.clone(),
        };
        Self {
            id: note.id.clone(),
            subject: subject.as_str(),
            kind: note.kind.as_str().to_string(),
            text: note.text.clone(),
            tags: note.tags.clone(),
            author: note.author.clone(),
            created_at: note.created_at,
            expires_at: note.expires_at,
        }
    }

    /// Cheap pre-check before any per-note work.
    ///
    /// An id has to be short and printable, and the text has to be within the
    /// wire bound. Everything else is normalized rather than rejected.
    pub fn looks_usable(&self) -> bool {
        !self.id.is_empty()
            && self.id.chars().count() <= 64
            && self.id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
            && !self.text.trim().is_empty()
            && self.text.chars().count() <= MAX_WIRE_TEXT_CHARS
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RetractPayload {
    pub id: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncPayload {
    #[serde(default)]
    pub notes: Vec<SharedNote>,
}

/// What an inbound channel message means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Inbound {
    /// A peer published a note.
    Note(Box<SharedNote>),
    /// A peer withdrew a note it had published.
    Retract(String),
    /// A peer asked what this node knows.
    SyncRequest,
    /// A peer answered our request.
    Sync(Vec<SharedNote>),
    /// Nothing this plugin acts on.
    Ignore,
}

/// Decide what an inbound message means, without touching any state.
///
/// A message on another channel, of an unknown kind, or with a body that does
/// not parse is [`Inbound::Ignore`] rather than an error: a peer on an older or
/// newer build must not be able to make this node log-spam or fail a handler,
/// and there is nothing useful to answer with.
pub fn plan_inbound(channel: &str, message_kind: &str, body: &[u8]) -> Inbound {
    if channel != CHANNEL {
        return Inbound::Ignore;
    }
    match message_kind {
        KIND_SYNC_REQUEST => Inbound::SyncRequest,
        KIND_NOTE => match serde_json::from_slice::<SharedNote>(body) {
            Ok(note) if note.looks_usable() => Inbound::Note(Box::new(note)),
            _ => Inbound::Ignore,
        },
        KIND_RETRACT => match serde_json::from_slice::<RetractPayload>(body) {
            Ok(payload) if !payload.id.is_empty() => Inbound::Retract(payload.id),
            _ => Inbound::Ignore,
        },
        KIND_SYNC => match serde_json::from_slice::<SyncPayload>(body) {
            Ok(payload) => Inbound::Sync(
                payload
                    .notes
                    .into_iter()
                    .filter(SharedNote::looks_usable)
                    .take(MAX_SYNC_NOTES)
                    .collect(),
            ),
            Err(_) => Inbound::Ignore,
        },
        _ => Inbound::Ignore,
    }
}

/// Turn a peer's note into one this node will hold.
///
/// Every field is re-derived here with the same functions a local `write` uses:
/// the text is sanitized and capped to *this* node's limit, tags are
/// re-normalized, the kind falls back to `info`, `node:local` becomes the
/// sending peer, and the expiry is recomputed from this node's TTL ceiling
/// rather than believed. A peer cannot pin a note into this node's memory, hide
/// an escape sequence in it, or claim to be a local note.
pub fn adopt(
    shared: &SharedNote,
    from_peer: &str,
    now: u64,
    max_note_chars: usize,
    max_ttl_secs: u64,
) -> Note {
    let (text, truncated) = crate::note::sanitize_text(&shared.text, max_note_chars);
    let requested_ttl = shared.expires_at.saturating_sub(now);
    let ttl = crate::note::clamp_ttl(Some(requested_ttl), max_ttl_secs, max_ttl_secs);
    Note {
        id: shared.id.clone(),
        subject: Subject::parse_untrusted(&shared.subject).resolve_local(from_peer),
        kind: Kind::parse_untrusted(&shared.kind),
        text,
        tags: crate::note::normalize_tags(&shared.tags),
        author: crate::note::sanitize_label(&shared.author, crate::note::MAX_AUTHOR_CHARS)
            .unwrap_or_default(),
        // A peer's clock is its own. `created_at` is kept only when it is not in
        // the future, so a note cannot sort itself to the top forever.
        created_at: shared.created_at.min(now),
        expires_at: now.saturating_add(ttl),
        origin: Origin::Peer(from_peer.to_string()),
        truncated,
        shared: false,
    }
}

/// A fixed-window counter, used to bound both what this node publishes and what
/// it accepts from any one peer.
///
/// Fixed windows are coarser than a token bucket at the boundary, and that is
/// the right trade here: the limit exists to stop a runaway loop and a hostile
/// flood, not to shape traffic, and a counter that is obviously correct is
/// worth more than one that is precisely fair.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RateWindow {
    window_start: u64,
    used: u32,
}

impl RateWindow {
    pub const WINDOW_SECS: u64 = 60;

    /// Consume one slot, returning whether it was available.
    pub fn allow(&mut self, now: u64, limit: u32) -> bool {
        if now.saturating_sub(self.window_start) >= Self::WINDOW_SECS {
            self.window_start = now;
            self.used = 0;
        }
        if self.used >= limit {
            return false;
        }
        self.used += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(id: &str) -> SharedNote {
        SharedNote {
            id: id.to_string(),
            subject: "mesh".into(),
            kind: "incident".into(),
            text: "gpu 0 fell off the bus".into(),
            tags: vec!["gpu".into()],
            author: "ops".into(),
            created_at: 1_000,
            expires_at: 4_600,
        }
    }

    fn body<T: Serialize>(payload: &T) -> Vec<u8> {
        serde_json::to_vec(payload).expect("payload serializes")
    }

    #[test]
    fn each_message_kind_is_planned_as_itself() {
        assert_eq!(
            plan_inbound(CHANNEL, KIND_SYNC_REQUEST, b"{}"),
            Inbound::SyncRequest
        );
        assert_eq!(
            plan_inbound(CHANNEL, KIND_NOTE, &body(&shared("n1"))),
            Inbound::Note(Box::new(shared("n1")))
        );
        assert_eq!(
            plan_inbound(
                CHANNEL,
                KIND_RETRACT,
                &body(&RetractPayload { id: "n1".into() })
            ),
            Inbound::Retract("n1".into())
        );
        assert_eq!(
            plan_inbound(
                CHANNEL,
                KIND_SYNC,
                &body(&SyncPayload {
                    notes: vec![shared("n1"), shared("n2")]
                })
            ),
            Inbound::Sync(vec![shared("n1"), shared("n2")])
        );
    }

    #[test]
    fn another_channel_or_an_unknown_kind_is_ignored() {
        assert_eq!(
            plan_inbound("something.else", KIND_NOTE, &body(&shared("n1"))),
            Inbound::Ignore
        );
        assert_eq!(plan_inbound(CHANNEL, "gossip", b"{}"), Inbound::Ignore);
    }

    #[test]
    fn a_malformed_body_is_ignored_rather_than_raised() {
        assert_eq!(
            plan_inbound(CHANNEL, KIND_NOTE, b"not json at all"),
            Inbound::Ignore
        );
        assert_eq!(
            plan_inbound(CHANNEL, KIND_SYNC, b"\"a bare string\""),
            Inbound::Ignore
        );
        // A retraction with no id names nothing and is not acted on.
        assert_eq!(plan_inbound(CHANNEL, KIND_RETRACT, b"{}"), Inbound::Ignore);
    }

    #[test]
    fn a_note_with_an_unusable_id_or_body_never_reaches_the_store() {
        for hostile in [
            SharedNote {
                id: String::new(),
                ..shared("x")
            },
            SharedNote {
                id: "../../etc/passwd".into(),
                ..shared("x")
            },
            SharedNote {
                id: "a".repeat(200),
                ..shared("x")
            },
            SharedNote {
                text: "   ".into(),
                ..shared("x")
            },
            SharedNote {
                text: "x".repeat(MAX_WIRE_TEXT_CHARS + 1),
                ..shared("x")
            },
        ] {
            assert!(
                !hostile.looks_usable(),
                "{:?} should be refused",
                hostile.id
            );
            assert_eq!(
                plan_inbound(CHANNEL, KIND_NOTE, &body(&hostile)),
                Inbound::Ignore
            );
        }
    }

    #[test]
    fn a_sync_answer_is_truncated_and_filtered_before_anything_else_sees_it() {
        let mut notes: Vec<SharedNote> = (0..MAX_SYNC_NOTES + 40)
            .map(|index| shared(&format!("n{index}")))
            .collect();
        notes.push(SharedNote {
            id: "bad id".into(),
            ..shared("x")
        });

        let Inbound::Sync(accepted) =
            plan_inbound(CHANNEL, KIND_SYNC, &body(&SyncPayload { notes }))
        else {
            panic!("expected a sync answer");
        };
        assert_eq!(accepted.len(), MAX_SYNC_NOTES);
        assert!(accepted.iter().all(SharedNote::looks_usable));
    }

    #[test]
    fn adopting_re_derives_every_field_from_this_nodes_own_limits() {
        let hostile = SharedNote {
            id: "n1".into(),
            subject: "node:local".into(),
            kind: "catastrophe".into(),
            text: format!("\u{1b}[2J{}", "x".repeat(2_000)),
            tags: vec!["GPU!".into(), "gpu".into()],
            author: "  someone\nelse ".into(),
            // A year of TTL and a creation date in the future.
            created_at: 9_000_000,
            expires_at: 40_000_000,
        };

        let note = adopt(&hostile, "peer-1", 1_000, 500, 3_600);

        assert_eq!(note.subject, Subject::Node("peer-1".into()));
        assert_eq!(note.kind, Kind::Info, "an unknown kind falls back");
        assert!(!note.text.contains('\u{1b}'));
        assert_eq!(note.text.chars().count(), 500);
        assert!(note.truncated);
        assert_eq!(note.tags, vec!["gpu".to_string()]);
        assert_eq!(note.author, "someone else");
        assert_eq!(note.created_at, 1_000, "a future clock is pulled back");
        assert_eq!(note.expires_at, 4_600, "the TTL is this node's ceiling");
        assert_eq!(note.origin, Origin::Peer("peer-1".into()));
        assert!(!note.shared, "a peer's note is never re-shared");
    }

    #[test]
    fn adopting_keeps_a_short_ttl_a_peer_asked_for() {
        let note = adopt(&shared("n1"), "peer-1", 4_000, 500, 86_400);
        assert_eq!(
            note.expires_at, 4_600,
            "600s left, and that is what is kept"
        );
    }

    #[test]
    fn an_already_expired_note_is_adopted_with_the_floor_not_the_past() {
        // `expires_at` is behind `now`, so the requested TTL is zero. The clamp
        // floor applies, and the store's own expiry check does the rest.
        let note = adopt(&shared("n1"), "peer-1", 9_999, 500, 86_400);
        assert_eq!(note.expires_at, 9_999 + crate::config::MIN_TTL_SECS);
    }

    #[test]
    fn a_shared_note_names_this_node_once_it_knows_its_own_id() {
        let note = Note {
            id: "n1".into(),
            subject: Subject::Node(crate::note::LOCAL_NODE.into()),
            kind: Kind::Pin,
            text: "pinned to q4".into(),
            tags: vec!["model".into()],
            author: "ops".into(),
            created_at: 10,
            expires_at: 100,
            origin: Origin::Local,
            truncated: false,
            shared: true,
        };

        assert_eq!(SharedNote::of(&note, Some("abc123")).subject, "node:abc123");
        assert_eq!(SharedNote::of(&note, None).subject, "node:local");
    }

    #[test]
    fn a_rate_window_sheds_beyond_the_limit_and_reopens_next_window() {
        let mut window = RateWindow::default();
        for _ in 0..3 {
            assert!(window.allow(1_000, 3));
        }
        assert!(!window.allow(1_000, 3), "the fourth is shed");
        assert!(!window.allow(1_059, 3), "still inside the same window");
        assert!(window.allow(1_060, 3), "a new window opens");
        assert!(window.allow(1_060, 3), "and has its full allowance again");
    }

    #[test]
    fn a_rate_window_starts_open() {
        let mut window = RateWindow::default();
        assert!(window.allow(0, 1));
        assert!(!window.allow(0, 1));
    }
}
