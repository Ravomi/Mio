//! Explicit host-registered pinned context (DES-11).
//!
//! [`PinnedContext`] is a small, bounded, insertion-ordered registry of the
//! constraints and unfinished goals that an embedding host registers
//! explicitly, each with a stable ID and verbatim text. It exists so host
//! constraints survive request-level fitting and later summarization without
//! ever being re-derived from natural language:
//!
//! - **Host-only registration.** This module never inspects message text for
//!   keywords and never exposes a model-facing tool that could add, rewrite or
//!   remove an entry. Only the host calls [`PinnedContext::register`],
//!   [`PinnedContext::insert`], [`PinnedContext::replace`] and
//!   [`PinnedContext::remove`].
//! - **One validation path.** Constructors, `register`/`replace` and the
//!   `serde` deserializers all run the same checks, so a hand-written JSON
//!   payload cannot smuggle in a duplicate ID, an empty or control-character
//!   ID, empty text, or an entry/total byte overflow.
//! - **Deterministic, auditable rendering.**
//!   [`PinnedContext::render_instruction`] returns the block the host appends
//!   to the system instructions. Each entry is labelled with its stable ID and
//!   kind and its text is reproduced verbatim, including newlines. The header
//!   states that the entries are host-registered constraints and unfinished
//!   goals, not platform system messages and not model-authored instructions.
//! - **Refuse, never trim.** [`PinnedContext::render_within`] returns `None`
//!   when the registered entries do not fit the caller's byte allowance, so the
//!   host refuses the request instead of silently dropping a pinned item.
//!
//! Scope: this module owns only the *host-registered* part of the Pinned tier.
//! System instructions, tool schemas and the current turn's tool call/result
//! pairs stay owned by the request builder and [`crate::context_budget`].
//! Structured loss reporting (DES-13) and compaction are intentionally not part
//! of this module.

use serde::{Deserialize, Serialize};

/// Maximum number of entries one registry can hold.
pub const MAX_PINNED_ENTRIES: usize = 64;
/// Maximum UTF-8 byte length of a single stable entry ID.
pub const MAX_PINNED_ID_BYTES: usize = 64;
/// Maximum UTF-8 byte length of a single entry's verbatim text.
pub const MAX_PINNED_ENTRY_BYTES: usize = 8 * 1024;
/// Maximum sum of all entries' verbatim text bytes.
pub const MAX_PINNED_TOTAL_BYTES: usize = 32 * 1024;

/// What a pinned entry represents. The two kinds are kept apart so a host can
/// report registered constraints and unfinished goals distinctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinnedContextKind {
    /// A rule the host wants carried into every request unchanged.
    Constraint,
    /// A goal the host considers unfinished; it is evidence, not permission to
    /// replay the request that recorded it.
    Goal,
}

impl PinnedContextKind {
    /// Stable lowercase wire/label form (`constraint` / `goal`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Constraint => "constraint",
            Self::Goal => "goal",
        }
    }
}

impl std::fmt::Display for PinnedContextKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One host-registered pinned entry: a stable ID, its kind and its verbatim
/// text.
///
/// The fields are public so a host can inspect an entry, but every insertion
/// path re-validates what it receives: constructing this struct with a literal
/// does not make an invalid entry registerable. Prefer
/// [`PinnedContextEntry::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PinnedContextEntry {
    /// Stable, case-sensitive identifier. Compared byte-for-byte; reusing an ID
    /// requires [`PinnedContext::replace`], never a second `register`.
    pub id: String,
    /// Whether this is a constraint or an unfinished goal.
    pub kind: PinnedContextKind,
    /// The original text, preserved exactly. Newlines are ordinary characters
    /// and are never normalized or reflowed.
    pub text: String,
}

impl PinnedContextEntry {
    /// Validates and builds one entry.
    pub fn new(
        id: impl Into<String>,
        kind: PinnedContextKind,
        text: impl Into<String>,
    ) -> Result<Self, String> {
        let entry = Self {
            id: id.into(),
            kind,
            text: text.into(),
        };
        entry.validate()?;
        Ok(entry)
    }

    /// Runs the entry-level checks shared by the constructors, `register` and
    /// `replace`.
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.id)?;
        validate_text(&self.text)?;
        Ok(())
    }

    /// UTF-8 byte length of the verbatim text.
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }
}

/// Entry-level ID validation.
fn validate_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("pinned context: entry id must not be empty".into());
    }
    if id.len() > MAX_PINNED_ID_BYTES {
        return Err(format!(
            "pinned context: entry id must not exceed {} bytes",
            MAX_PINNED_ID_BYTES
        ));
    }
    if id.chars().any(char::is_control) {
        return Err("pinned context: entry id must not contain control characters".into());
    }
    Ok(())
}

/// Text validation. Newline, carriage return and tab are ordinary text, so a
/// multi-line constraint keeps its original bytes.
fn validate_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("pinned context: entry text must not be empty".into());
    }
    if text.len() > MAX_PINNED_ENTRY_BYTES {
        return Err(format!(
            "pinned context: entry text must not exceed {} bytes",
            MAX_PINNED_ENTRY_BYTES
        ));
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(
            "pinned context: entry text must not contain control characters other than newline, carriage return and tab"
                .into(),
        );
    }
    Ok(())
}

impl<'de> Deserialize<'de> for PinnedContextEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Strict wire shape: unknown fields are rejected instead of ignored.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: String,
            kind: PinnedContextKind,
            text: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        // Deserialization uses the same constructor as ordinary registration,
        // so validation cannot be bypassed by writing JSON directly.
        PinnedContextEntry::new(wire.id, wire.kind, wire.text).map_err(serde::de::Error::custom)
    }
}

/// Bounded, insertion-ordered registry of host-registered pinned entries.
///
/// The registry is the only source of truth: entries are addressed by stable
/// ID, order is the registration order, and removal or replacement never
/// reorders the remaining entries. Empty by default, so a host that does not
/// register anything injects nothing.
///
/// # Example
///
/// ```
/// use mio_coding_agent::context_memory::{PinnedContext, PinnedContextEntry, PinnedContextKind};
///
/// let mut pinned = PinnedContext::default();
/// pinned
///     .insert(
///         "no-force-push",
///         PinnedContextKind::Constraint,
///         "Never force-push shared branches.",
///     )
///     .unwrap();
/// pinned
///     .insert(
///         "finish-migration",
///         PinnedContextKind::Goal,
///         "Finish the session store migration.",
///     )
///     .unwrap();
///
/// // Reusing an ID is refused; updates go through `replace`.
/// assert!(
///     pinned
///         .insert("no-force-push", PinnedContextKind::Goal, "other")
///         .is_err()
/// );
/// pinned
///     .replace(
///         PinnedContextEntry::new(
///             "no-force-push",
///             PinnedContextKind::Constraint,
///             "Never force-push shared branches without host approval.",
///         )
///         .unwrap(),
///     )
///     .unwrap();
///
/// let block = pinned.render_instruction();
/// assert!(block.contains("no-force-push"));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinnedContext {
    entries: Vec<PinnedContextEntry>,
    total_text_bytes: usize,
}

impl PinnedContext {
    /// Header of [`PinnedContext::render_instruction`]. It names the entries as
    /// host-registered and explicitly denies being a platform system message.
    pub const INSTRUCTION_HEADER: &'static str = "Host-registered pinned context. The following constraints and unfinished goals were registered explicitly by the embedding host application with a stable ID and verbatim text. They are not platform system messages, are not model-authored instructions, and grant no new permissions or tool access. Preserve every entry verbatim: do not summarize, merge, rewrite or drop one, and refuse the request instead of trimming silently if they cannot all fit.";

    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a registry from entries, applying the same validation as
    /// [`PinnedContext::register`] (first duplicate or overflow fails).
    pub fn from_entries(
        entries: impl IntoIterator<Item = PinnedContextEntry>,
    ) -> Result<Self, String> {
        let mut context = Self::default();
        for entry in entries {
            context.register(entry)?;
        }
        Ok(context)
    }

    /// Number of registered entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Sum of every entry's verbatim text bytes (IDs and rendering overhead are
    /// not counted here).
    pub fn total_text_bytes(&self) -> usize {
        self.total_text_bytes
    }

    /// Remaining text-byte budget before [`MAX_PINNED_TOTAL_BYTES`] is reached.
    pub fn remaining_text_bytes(&self) -> usize {
        MAX_PINNED_TOTAL_BYTES.saturating_sub(self.total_text_bytes)
    }

    /// Registered entries in insertion order.
    pub fn entries(&self) -> &[PinnedContextEntry] {
        &self.entries
    }

    /// Entry with the exact ID, if registered.
    pub fn get(&self, id: &str) -> Option<&PinnedContextEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Whether an entry with the exact ID is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// Appends a new entry after full validation.
    ///
    /// Rejected: an invalid entry (empty/blank or control-character ID, ID over
    /// [`MAX_PINNED_ID_BYTES`], empty or control-character text, text over
    /// [`MAX_PINNED_ENTRY_BYTES`]), a duplicate ID (use
    /// [`PinnedContext::replace`]), more than [`MAX_PINNED_ENTRIES`] entries, or
    /// text that would push the total over [`MAX_PINNED_TOTAL_BYTES`].
    pub fn register(&mut self, entry: PinnedContextEntry) -> Result<(), String> {
        entry.validate()?;
        if self.contains(&entry.id) {
            return Err(format!(
                "pinned context: entry id {:?} is already registered; use replace to update it",
                entry.id
            ));
        }
        if self.entries.len() >= MAX_PINNED_ENTRIES {
            return Err(format!(
                "pinned context: at most {} entries can be registered",
                MAX_PINNED_ENTRIES
            ));
        }
        let next_total = self.total_text_bytes.saturating_add(entry.text_bytes());
        if next_total > MAX_PINNED_TOTAL_BYTES {
            return Err(format!(
                "pinned context: registering {:?} needs {next_total} text bytes, above the {}-byte total",
                entry.id, MAX_PINNED_TOTAL_BYTES
            ));
        }
        self.total_text_bytes = next_total;
        self.entries.push(entry);
        Ok(())
    }

    /// Convenience wrapper that builds an entry and registers it.
    pub fn insert(
        &mut self,
        id: impl Into<String>,
        kind: PinnedContextKind,
        text: impl Into<String>,
    ) -> Result<(), String> {
        self.register(PinnedContextEntry::new(id, kind, text)?)
    }

    /// Replaces the entry with the same ID in place, returning the previous
    /// entry. The ID must already be registered: an update is always explicit
    /// and `register` never silently upserts.
    pub fn replace(&mut self, entry: PinnedContextEntry) -> Result<PinnedContextEntry, String> {
        entry.validate()?;
        let index = self
            .entries
            .iter()
            .position(|existing| existing.id == entry.id)
            .ok_or_else(|| {
                format!(
                    "pinned context: entry id {:?} is not registered; use register first",
                    entry.id
                )
            })?;
        let replaced_bytes = self.entries[index].text_bytes();
        let next_total = self
            .total_text_bytes
            .saturating_sub(replaced_bytes)
            .saturating_add(entry.text_bytes());
        if next_total > MAX_PINNED_TOTAL_BYTES {
            return Err(format!(
                "pinned context: replacing {:?} needs {next_total} text bytes, above the {}-byte total",
                entry.id, MAX_PINNED_TOTAL_BYTES
            ));
        }
        // Position is preserved, so replacement never reorders the registry.
        let previous = std::mem::replace(&mut self.entries[index], entry);
        self.total_text_bytes = next_total;
        Ok(previous)
    }

    /// Removes and returns the entry with the exact ID, freeing its byte budget.
    pub fn remove(&mut self, id: &str) -> Option<PinnedContextEntry> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        let removed = self.entries.remove(index);
        self.total_text_bytes = self.total_text_bytes.saturating_sub(removed.text_bytes());
        Some(removed)
    }

    /// Drops every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_text_bytes = 0;
    }

    /// Deterministic, auditable instruction block for the host to insert into
    /// the system instructions.
    ///
    /// The output is the [`PinnedContext::INSTRUCTION_HEADER`] followed by one
    /// labelled block per entry in insertion order. Each label carries the
    /// entry's kind and stable ID; the entry text is appended verbatim,
    /// including its newlines. Byte-for-byte identical for identical
    /// registries: no timestamps, hashing, sorting or locale-dependent
    /// formatting. Returns an empty string for an empty registry so nothing is
    /// injected when the host registers nothing.
    pub fn render_instruction(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(
            Self::INSTRUCTION_HEADER.len() + self.total_text_bytes + self.entries.len() * 64,
        );
        out.push_str(Self::INSTRUCTION_HEADER);
        for entry in &self.entries {
            out.push_str("\n\n[entry kind=");
            out.push_str(entry.kind.as_str());
            out.push_str(" id=");
            out.push_str(&serde_json::to_string(&entry.id).expect("string serialization"));
            out.push_str("]\n");
            out.push_str(&entry.text);
        }
        out
    }

    /// [`PinnedContext::render_instruction`] as `Option`, where an empty
    /// registry yields `None` instead of empty text.
    pub fn instruction_block(&self) -> Option<String> {
        let rendered = self.render_instruction();
        (!rendered.is_empty()).then_some(rendered)
    }

    /// Byte length of the rendered instruction block, for reserving room in a
    /// request budget before building it.
    pub fn instruction_bytes(&self) -> usize {
        self.render_instruction().len()
    }

    /// Returns the rendered block only when it fits inside `budget` bytes
    /// (UTF-8 length).
    ///
    /// `None` means the registered entries cannot be carried in full. Callers
    /// must refuse the request rather than trim: a partial pinned block would be
    /// a silent loss of a host constraint or unfinished goal. An empty registry
    /// returns `Some("")` and costs nothing.
    pub fn render_within(&self, budget: usize) -> Option<String> {
        if self.entries.is_empty() {
            return Some(String::new());
        }
        let rendered = self.render_instruction();
        (rendered.len() <= budget).then_some(rendered)
    }
}

impl Serialize for PinnedContext {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        /// Only the entries are persisted; the byte total is recomputed by
        /// validation, so the wire form cannot assert a false total.
        #[derive(Serialize)]
        struct Wire<'a> {
            entries: &'a [PinnedContextEntry],
        }
        Wire {
            entries: &self.entries,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PinnedContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Strict wire shape: unknown fields are rejected.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            entries: Vec<PinnedContextEntry>,
        }
        let wire = Wire::deserialize(deserializer)?;
        // Per-entry validation already ran in `PinnedContextEntry`; this pass
        // adds duplicate-ID and total-byte enforcement, which are registry
        // invariants.
        PinnedContext::from_entries(wire.entries).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: PinnedContextKind, text: &str) -> PinnedContextEntry {
        PinnedContextEntry::new(id, kind, text).unwrap()
    }

    fn registered(id: &str, kind: PinnedContextKind, text: &str) -> PinnedContext {
        let mut context = PinnedContext::default();
        context.register(entry(id, kind, text)).unwrap();
        context
    }

    #[test]
    fn default_registry_is_empty_and_injects_nothing() {
        let context = PinnedContext::default();
        assert!(context.is_empty());
        assert_eq!(context.len(), 0);
        assert_eq!(context.total_text_bytes(), 0);
        assert_eq!(context.remaining_text_bytes(), MAX_PINNED_TOTAL_BYTES);
        assert_eq!(context.render_instruction(), "");
        assert!(context.instruction_block().is_none());
        assert_eq!(context.instruction_bytes(), 0);
        assert_eq!(context.render_within(0).as_deref(), Some(""));
        assert_eq!(context, PinnedContext::new());
        assert_eq!(PinnedContext::from_entries(Vec::new()).unwrap(), context);
    }

    #[test]
    fn register_rejects_duplicates_and_replace_updates_in_place() {
        let mut context = PinnedContext::default();
        context
            .register(entry(
                "deploy-freeze",
                PinnedContextKind::Constraint,
                "Do not deploy.",
            ))
            .unwrap();
        context
            .register(entry(
                "migration",
                PinnedContextKind::Goal,
                "Finish migration.",
            ))
            .unwrap();

        let duplicate = context
            .register(entry(
                "deploy-freeze",
                PinnedContextKind::Goal,
                "different text",
            ))
            .unwrap_err();
        assert!(duplicate.contains("already registered"), "{duplicate}");
        assert!(duplicate.contains("replace"), "{duplicate}");
        assert_eq!(context.len(), 2);
        assert_eq!(
            context.total_text_bytes(),
            "Do not deploy.".len() + "Finish migration.".len()
        );

        let previous = context
            .replace(entry(
                "deploy-freeze",
                PinnedContextKind::Constraint,
                "Freeze deploys until Friday.",
            ))
            .unwrap();
        assert_eq!(previous.text, "Do not deploy.");
        assert_eq!(context.len(), 2);
        assert_eq!(
            context.get("deploy-freeze").unwrap().text,
            "Freeze deploys until Friday."
        );
        // Replacement keeps the original position; it is not moved to the end.
        assert_eq!(context.entries()[0].id, "deploy-freeze");
        assert_eq!(context.entries()[1].id, "migration");
        assert_eq!(
            context.total_text_bytes(),
            "Freeze deploys until Friday.".len() + "Finish migration.".len()
        );
    }

    #[test]
    fn replace_requires_an_existing_id() {
        let mut context = PinnedContext::default();
        let error = context
            .replace(entry("missing", PinnedContextKind::Goal, "text"))
            .unwrap_err();
        assert!(error.contains("not registered"), "{error}");
        assert!(context.is_empty());
    }

    #[test]
    fn remove_returns_the_entry_and_frees_its_bytes() {
        let mut context = PinnedContext::default();
        context
            .insert("a", PinnedContextKind::Goal, "first")
            .unwrap();
        context
            .insert("b", PinnedContextKind::Goal, "second")
            .unwrap();

        let removed = context.remove("a").unwrap();
        assert_eq!(removed.id, "a");
        assert_eq!(removed.text, "first");
        assert_eq!(context.total_text_bytes(), "second".len());
        assert!(context.remove("a").is_none());
        // The freed budget is reusable and order is preserved.
        context
            .insert("c", PinnedContextKind::Goal, "third value")
            .unwrap();
        assert_eq!(context.len(), 2);
        assert_eq!(context.entries()[0].id, "b");
        assert_eq!(context.entries()[1].id, "c");

        context.clear();
        assert!(context.is_empty());
        assert_eq!(context.total_text_bytes(), 0);
        assert_eq!(context.render_instruction(), "");
    }

    #[test]
    fn validation_rejects_empty_control_and_oversized_entries() {
        // Blank or control-character IDs.
        for bad_id in ["", "   ", "\t", "id\nnewline", "id\u{7}bell", "id\u{0}"] {
            assert!(
                PinnedContextEntry::new(bad_id, PinnedContextKind::Goal, "text").is_err(),
                "id {bad_id:?} must be rejected"
            );
        }
        // ID over the byte cap.
        assert!(
            PinnedContextEntry::new(
                "x".repeat(MAX_PINNED_ID_BYTES + 1),
                PinnedContextKind::Goal,
                "text"
            )
            .is_err()
        );
        assert!(
            PinnedContextEntry::new(
                "x".repeat(MAX_PINNED_ID_BYTES),
                PinnedContextKind::Goal,
                "text"
            )
            .is_ok()
        );

        // Empty or whitespace-only text.
        for bad_text in ["", "   ", "\n\t\r\n"] {
            assert!(
                PinnedContextEntry::new("ok", PinnedContextKind::Goal, bad_text).is_err(),
                "text {bad_text:?} must be rejected"
            );
        }
        // Control characters other than newline/carriage return/tab.
        assert!(PinnedContextEntry::new("ok", PinnedContextKind::Goal, "bell\u{7}").is_err());
        assert!(PinnedContextEntry::new("ok", PinnedContextKind::Goal, "esc\u{1b}[31m").is_err());
        assert!(
            PinnedContextEntry::new("ok", PinnedContextKind::Goal, "line1\nline2\r\n\tindent")
                .is_ok()
        );

        // Oversized text at the exact boundaries: cap is inclusive.
        let context = registered(
            "ok",
            PinnedContextKind::Goal,
            &"x".repeat(MAX_PINNED_ENTRY_BYTES),
        );
        assert_eq!(context.total_text_bytes(), MAX_PINNED_ENTRY_BYTES);
        assert!(
            PinnedContextEntry::new(
                "ok",
                PinnedContextKind::Goal,
                "x".repeat(MAX_PINNED_ENTRY_BYTES + 1)
            )
            .is_err()
        );
    }

    #[test]
    fn register_enforces_entry_count_and_total_byte_limits() {
        // Entry count cap.
        let mut context = PinnedContext::default();
        for index in 0..MAX_PINNED_ENTRIES {
            context
                .insert(format!("id-{index}"), PinnedContextKind::Goal, "goal text")
                .unwrap();
        }
        assert_eq!(context.len(), MAX_PINNED_ENTRIES);
        let error = context
            .insert("overflow", PinnedContextKind::Goal, "goal text")
            .unwrap_err();
        assert!(error.contains("at most"), "{error}");

        // Total byte cap: four full-size entries fit exactly, the fifth cannot.
        let mut context = PinnedContext::default();
        let full = "y".repeat(MAX_PINNED_ENTRY_BYTES);
        for index in 0..(MAX_PINNED_TOTAL_BYTES / MAX_PINNED_ENTRY_BYTES) {
            context
                .insert(
                    format!("full-{index}"),
                    PinnedContextKind::Goal,
                    full.clone(),
                )
                .unwrap();
        }
        assert_eq!(context.total_text_bytes(), MAX_PINNED_TOTAL_BYTES);
        assert_eq!(context.remaining_text_bytes(), 0);
        let error = context
            .insert("one-more", PinnedContextKind::Goal, full.clone())
            .unwrap_err();
        assert!(error.contains("total"), "{error}");
        assert_eq!(context.len(), 4);

        // `replace` cannot grow the registry past the total cap either.
        let mut context = PinnedContext::default();
        for index in 0..3 {
            context
                .insert(
                    format!("full-{index}"),
                    PinnedContextKind::Goal,
                    full.clone(),
                )
                .unwrap();
        }
        context
            .insert("pad", PinnedContextKind::Goal, "p".repeat(100))
            .unwrap();
        context
            .insert("room", PinnedContextKind::Goal, "r".repeat(4_096))
            .unwrap();
        let before = context.total_text_bytes();
        assert_eq!(before, 3 * MAX_PINNED_ENTRY_BYTES + 100 + 4_096);
        let error = context
            .replace(entry("room", PinnedContextKind::Goal, full.as_str()))
            .unwrap_err();
        assert!(error.contains("total"), "{error}");
        // A rejected replacement leaves the registry untouched.
        assert_eq!(context.total_text_bytes(), before);
        assert_eq!(context.get("room").unwrap().text.len(), 4_096);

        // Shrinking through `replace` frees bytes for a new entry.
        context
            .replace(entry("room", PinnedContextKind::Goal, "short"))
            .unwrap();
        let remaining = context.remaining_text_bytes();
        assert_eq!(
            remaining,
            MAX_PINNED_TOTAL_BYTES - before + 4_096 - "short".len()
        );
        context
            .insert("now-fits", PinnedContextKind::Goal, "x".repeat(remaining))
            .unwrap();
        assert_eq!(context.total_text_bytes(), MAX_PINNED_TOTAL_BYTES);
        assert!(
            context
                .insert("too-much", PinnedContextKind::Goal, "x")
                .is_err()
        );
    }

    #[test]
    fn struct_literals_cannot_bypass_validation() {
        let mut context = PinnedContext::default();
        let raw = PinnedContextEntry {
            id: "   ".into(),
            kind: PinnedContextKind::Goal,
            text: "text".into(),
        };
        assert!(context.register(raw).is_err());
        let raw = PinnedContextEntry {
            id: "ok".into(),
            kind: PinnedContextKind::Goal,
            text: String::new(),
        };
        assert!(context.register(raw).is_err());
        assert!(context.is_empty());

        let raw = PinnedContextEntry {
            id: "ok".into(),
            kind: PinnedContextKind::Goal,
            text: "valid text".into(),
        };
        context.register(raw).unwrap();
        // The same literal with a duplicate ID is still refused on insertion.
        let duplicate = PinnedContextEntry {
            id: "ok".into(),
            kind: PinnedContextKind::Goal,
            text: "other text".into(),
        };
        assert!(context.register(duplicate).is_err());
        assert_eq!(context.len(), 1);
    }

    #[test]
    fn unicode_ids_and_multiline_text_are_preserved_exactly() {
        let text = "约束：不要删除用户文件。\n第二行含 emoji 🚀 与制表符\t。";
        let mut context = PinnedContext::default();
        context
            .insert("目标-迁移", PinnedContextKind::Goal, text)
            .unwrap();

        let stored = &context.entries()[0];
        assert_eq!(stored.id, "目标-迁移");
        assert_eq!(stored.text, text);
        // Bytes, not chars, drive the budget.
        assert_eq!(context.total_text_bytes(), text.len());
        assert_ne!(text.len(), text.chars().count());

        let rendered = context.render_instruction();
        assert!(rendered.contains(text), "text must appear verbatim");
        assert!(rendered.contains("目标-迁移"));
        assert!(rendered.contains("kind=goal"));

        // Round-tripping through JSON keeps the exact bytes.
        let json = serde_json::to_string(&context).unwrap();
        let restored: PinnedContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, context);
        assert_eq!(restored.entries()[0].text, text);
    }

    #[test]
    fn render_is_deterministic_auditable_and_ordered() {
        let mut context = PinnedContext::default();
        context
            .insert(
                "no-deploy",
                PinnedContextKind::Constraint,
                "Do not deploy while the freeze is active.",
            )
            .unwrap();
        context
            .insert(
                "finish-migration",
                PinnedContextKind::Goal,
                "Finish the session store migration.",
            )
            .unwrap();

        let first = context.render_instruction();
        assert_eq!(first, context.render_instruction(), "render must be stable");
        assert_eq!(context.instruction_block().as_deref(), Some(first.as_str()));
        assert_eq!(context.instruction_bytes(), first.len());

        assert!(first.starts_with(PinnedContext::INSTRUCTION_HEADER));
        assert!(first.contains("kind=constraint id=\"no-deploy\""));
        assert!(first.contains("kind=goal id=\"finish-migration\""));
        assert!(first.contains("Do not deploy while the freeze is active."));
        assert!(first.contains("Finish the session store migration."));

        // Insertion order, not sorted or hashed.
        let constraint_at = first.find("no-deploy").unwrap();
        let goal_at = first.find("finish-migration").unwrap();
        assert!(constraint_at < goal_at);

        // The header describes host-registered entries and does not claim to be
        // a platform system message.
        let header = PinnedContext::INSTRUCTION_HEADER.to_lowercase();
        assert!(header.contains("host-registered"));
        assert!(header.contains("not platform system messages"));
        assert!(header.contains("verbatim"));

        // Replacing keeps the position, so the order of the block is stable.
        context
            .replace(entry(
                "no-deploy",
                PinnedContextKind::Constraint,
                "Freeze ended; deploys allowed again.",
            ))
            .unwrap();
        let second = context.render_instruction();
        assert!(second.find("no-deploy").unwrap() < second.find("finish-migration").unwrap());
        assert!(second.contains("Freeze ended; deploys allowed again."));
    }

    #[test]
    fn rendered_identifiers_round_trip_without_ambiguous_escaping() {
        let id = "引用-\"quoted\"-\\path";
        let context = registered(id, PinnedContextKind::Constraint, "Keep the original API.");
        let text = context.render_instruction();
        let label = text
            .lines()
            .find(|line| line.starts_with("[entry "))
            .unwrap();
        let encoded = label
            .split_once(" id=")
            .unwrap()
            .1
            .strip_suffix(']')
            .unwrap();
        assert_eq!(serde_json::from_str::<String>(encoded).unwrap(), id);
    }

    #[test]
    fn render_within_refuses_instead_of_trimming() {
        let mut context = PinnedContext::default();
        context
            .insert("goal", PinnedContextKind::Goal, "Finish the migration.")
            .unwrap();
        let rendered = context.render_instruction();
        assert_eq!(
            context.render_within(rendered.len()).as_deref(),
            Some(rendered.as_str())
        );
        assert!(context.render_within(rendered.len() - 1).is_none());
        assert!(context.render_within(0).is_none());
    }

    #[test]
    fn serde_round_trip_preserves_order_and_shape() {
        let mut context = PinnedContext::default();
        context
            .insert(
                "a",
                PinnedContextKind::Constraint,
                "keep line 1\nkeep line 2",
            )
            .unwrap();
        context
            .insert("b", PinnedContextKind::Goal, "finish 目标")
            .unwrap();

        let json = serde_json::to_string(&context).unwrap();
        assert!(json.contains("\"entries\""));
        assert!(!json.contains("total_text_bytes"), "{json}");
        let restored: PinnedContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, context);
        assert_eq!(restored.entries()[0].id, "a");
        assert_eq!(restored.entries()[1].id, "b");
        assert_eq!(restored.entries()[0].text, "keep line 1\nkeep line 2");
        assert_eq!(restored.entries()[0].kind, PinnedContextKind::Constraint);

        let entry = PinnedContextEntry::new("a", PinnedContextKind::Constraint, "text").unwrap();
        let json = serde_json::to_string(&entry).unwrap();
        let restored: PinnedContextEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, entry);
    }

    #[test]
    fn deserialization_runs_the_same_validation_as_register() {
        // Duplicate ID: accepted by the entry shape, rejected by the registry.
        let duplicate = r#"{"entries":[{"id":"dup","kind":"goal","text":"one"},{"id":"dup","kind":"goal","text":"two"}]}"#;
        let error = serde_json::from_str::<PinnedContext>(duplicate).unwrap_err();
        assert!(error.to_string().contains("already registered"), "{error}");

        // Per-entry violations reachable from raw JSON.
        for payload in [
            r#"{"entries":[{"id":"ok","kind":"goal","text":""}]}"#,
            r#"{"entries":[{"id":"   ","kind":"goal","text":"x"}]}"#,
            r#"{"entries":[{"id":"a\u0000b","kind":"goal","text":"x"}]}"#,
            r#"{"entries":[{"id":"ok","kind":"unsupported","text":"x"}]}"#,
            r#"{"entries":[{"id":"ok","kind":"goal"}]}"#,
            r#"{"entries":[{"id":"ok","kind":"goal","text":"x","extra":1}]}"#,
        ] {
            assert!(
                serde_json::from_str::<PinnedContext>(payload).is_err(),
                "payload must be rejected: {payload}"
            );
        }

        // Unknown fields on the registry itself are rejected.
        assert!(serde_json::from_str::<PinnedContext>(r#"{"entries":[],"extra":1}"#).is_err());

        // Individually valid entries whose sum exceeds the total byte cap.
        let full = "x".repeat(MAX_PINNED_ENTRY_BYTES);
        let overflow = serde_json::json!({
            "entries": (0..(MAX_PINNED_TOTAL_BYTES / MAX_PINNED_ENTRY_BYTES + 1))
                .map(|index| serde_json::json!({
                    "id": format!("full-{index}"),
                    "kind": "goal",
                    "text": full.as_str(),
                }))
                .collect::<Vec<_>>()
        });
        let payload = serde_json::to_string(&overflow).unwrap();
        let error = serde_json::from_str::<PinnedContext>(&payload).unwrap_err();
        assert!(error.to_string().contains("total"), "{error}");

        // More entries than the count cap.
        let too_many = serde_json::json!({
            "entries": (0..=MAX_PINNED_ENTRIES)
                .map(|index| serde_json::json!({
                    "id": format!("id-{index}"),
                    "kind": "constraint",
                    "text": "text",
                }))
                .collect::<Vec<_>>()
        });
        let payload = serde_json::to_string(&too_many).unwrap();
        let error = serde_json::from_str::<PinnedContext>(&payload).unwrap_err();
        assert!(error.to_string().contains("at most"), "{error}");
    }

    #[test]
    fn deserialized_registry_still_renders_and_rejects_duplicates_afterwards() {
        let json = r#"{"entries":[{"id":"goal-1","kind":"goal","text":"line 1\nline 2"}]}"#;
        let mut context: PinnedContext = serde_json::from_str(json).unwrap();
        assert_eq!(context.total_text_bytes(), "line 1\nline 2".len());
        assert!(context.render_instruction().contains("line 1\nline 2"));
        assert!(
            context
                .insert("goal-1", PinnedContextKind::Goal, "again")
                .is_err()
        );
    }
}
