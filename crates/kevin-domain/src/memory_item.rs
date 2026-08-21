//! The [`MemoryItem`] aggregate (`plan/02-domain-model.md` §Memory,
//! `plan/06-memory-and-learning.md` §1).
//!
//! ```text
//! (none) ──StoreMemoryItem──▶ active
//! active ──SupersedeMemoryItem{by}──▶ superseded
//! active|superseded ──ForgetMemoryItem──▶ forgotten
//! ```
//!
//! The embedding vector itself lives in `kevin-memory`; the aggregate only
//! knows the embedding model name.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::aggregate::{Aggregate, EventMeta};
use crate::error::DomainError;
use crate::ids::MemoryItemId;
use crate::values::{MemoryKind, MemoryScope, MemorySource};

/// Aggregate type name (`EventEnvelope::aggregate_type`).
pub const MEMORY_ITEM_AGGREGATE_TYPE: &str = "memory_item";

/// Maximum `content` length in characters (`memory.memory_items.content` CHECK).
pub const MAX_CONTENT_CHARS: usize = 8000;

/// Lifecycle state of a memory item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryItemStatus {
    /// Retrievable.
    #[default]
    Active,
    /// Replaced by a newer item (kept for provenance).
    Superseded,
    /// Forgotten: content must be erased by the store.
    Forgotten,
}

impl MemoryItemStatus {
    /// `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            MemoryItemStatus::Active => "active",
            MemoryItemStatus::Superseded => "superseded",
            MemoryItemStatus::Forgotten => "forgotten",
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Stores a memory item (`memory.item_stored`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreMemoryItem {
    /// New item id.
    pub memory_item_id: MemoryItemId,
    /// Kind.
    pub kind: MemoryKind,
    /// Content (≤ 8000 chars).
    pub content: String,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Provenance.
    pub source: MemorySource,
    /// Scope.
    #[serde(default)]
    pub scope: MemoryScope,
    /// Embedding model used (`None` when embeddings are disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// Importance 0..=1.
    pub importance: f32,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// Marks the item as replaced by `superseded_by` (`memory.item_superseded`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersedeMemoryItem {
    /// The newer item.
    pub superseded_by: MemoryItemId,
}

/// Forgets the item (`memory.item_forgotten`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForgetMemoryItem {
    /// Why (audit).
    #[serde(default)]
    pub reason: String,
}

/// Every command the [`MemoryItem`] aggregate handles.
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryItemCommand {
    /// [`StoreMemoryItem`].
    Store(StoreMemoryItem),
    /// [`SupersedeMemoryItem`].
    Supersede(SupersedeMemoryItem),
    /// [`ForgetMemoryItem`].
    Forget(ForgetMemoryItem),
}

impl MemoryItemCommand {
    /// `snake_case` command name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            MemoryItemCommand::Store(_) => "store_memory_item",
            MemoryItemCommand::Supersede(_) => "supersede_memory_item",
            MemoryItemCommand::Forget(_) => "forget_memory_item",
        }
    }
}

impl From<StoreMemoryItem> for MemoryItemCommand {
    fn from(cmd: StoreMemoryItem) -> Self {
        MemoryItemCommand::Store(cmd)
    }
}

impl From<SupersedeMemoryItem> for MemoryItemCommand {
    fn from(cmd: SupersedeMemoryItem) -> Self {
        MemoryItemCommand::Supersede(cmd)
    }
}

impl From<ForgetMemoryItem> for MemoryItemCommand {
    fn from(cmd: ForgetMemoryItem) -> Self {
        MemoryItemCommand::Forget(cmd)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events of the `memory_item` stream (internally tagged on `type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MemoryItemEvent {
    /// `memory.item_stored`
    #[serde(rename = "memory.item_stored")]
    Stored {
        /// Item id.
        memory_item_id: MemoryItemId,
        /// Kind.
        kind: MemoryKind,
        /// Content.
        content: String,
        /// Tags.
        tags: Vec<String>,
        /// Provenance.
        source: MemorySource,
        /// Scope.
        scope: MemoryScope,
        /// Embedding model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embedding_model: Option<String>,
        /// Importance.
        importance: f32,
        /// Created at.
        created_at: DateTime<Utc>,
    },
    /// `memory.item_superseded`
    #[serde(rename = "memory.item_superseded")]
    Superseded {
        /// The newer item.
        superseded_by: MemoryItemId,
    },
    /// `memory.item_forgotten`
    #[serde(rename = "memory.item_forgotten")]
    Forgotten {
        /// Why.
        reason: String,
    },
}

impl MemoryItemEvent {
    /// Every event type of the `memory_item` stream, in catalog order.
    pub const TYPES: [&'static str; 3] = [
        "memory.item_stored",
        "memory.item_superseded",
        "memory.item_forgotten",
    ];
}

impl EventMeta for MemoryItemEvent {
    fn event_type(&self) -> &'static str {
        match self {
            MemoryItemEvent::Stored { .. } => "memory.item_stored",
            MemoryItemEvent::Superseded { .. } => "memory.item_superseded",
            MemoryItemEvent::Forgotten { .. } => "memory.item_forgotten",
        }
    }

    fn schema_version(&self) -> u16 {
        1
    }

    fn aggregate_type(&self) -> &'static str {
        MEMORY_ITEM_AGGREGATE_TYPE
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// The memory item aggregate.
#[derive(Debug, Clone)]
pub struct MemoryItem {
    version: u64,
    id: MemoryItemId,
    kind: Option<MemoryKind>,
    content: String,
    tags: Vec<String>,
    source: Option<MemorySource>,
    scope: MemoryScope,
    embedding_model: Option<String>,
    importance: f32,
    created_at: Option<DateTime<Utc>>,
    superseded_by: Option<MemoryItemId>,
    status: MemoryItemStatus,
}

impl Default for MemoryItem {
    fn default() -> Self {
        Self {
            version: 0,
            id: MemoryItemId::nil(),
            kind: None,
            content: String::new(),
            tags: Vec::new(),
            source: None,
            scope: MemoryScope::Global,
            embedding_model: None,
            importance: 0.0,
            created_at: None,
            superseded_by: None,
            status: MemoryItemStatus::Active,
        }
    }
}

impl MemoryItem {
    /// Typed id.
    #[must_use]
    pub const fn memory_item_id(&self) -> MemoryItemId {
        self.id
    }

    /// Kind (after `memory.item_stored`).
    #[must_use]
    pub const fn kind(&self) -> Option<MemoryKind> {
        self.kind
    }

    /// Content.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Tags.
    #[must_use]
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Provenance.
    #[must_use]
    pub const fn source(&self) -> Option<&MemorySource> {
        self.source.as_ref()
    }

    /// Scope.
    #[must_use]
    pub const fn scope(&self) -> &MemoryScope {
        &self.scope
    }

    /// Embedding model.
    #[must_use]
    pub fn embedding_model(&self) -> Option<&str> {
        self.embedding_model.as_deref()
    }

    /// Importance 0..=1.
    #[must_use]
    pub const fn importance(&self) -> f32 {
        self.importance
    }

    /// Created at.
    #[must_use]
    pub const fn created_at(&self) -> Option<DateTime<Utc>> {
        self.created_at
    }

    /// The item that replaced this one.
    #[must_use]
    pub const fn superseded_by(&self) -> Option<MemoryItemId> {
        self.superseded_by
    }

    /// Status.
    #[must_use]
    pub const fn status(&self) -> MemoryItemStatus {
        self.status
    }

    /// `status == active`.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == MemoryItemStatus::Active
    }

    fn handle_store(&self, cmd: &StoreMemoryItem) -> Result<Vec<MemoryItemEvent>, DomainError> {
        if self.version > 0 {
            return Err(DomainError::AlreadyExists {
                aggregate: MEMORY_ITEM_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        if cmd.content.trim().is_empty() {
            return Err(DomainError::invalid_value("content", "must not be empty"));
        }
        if cmd.content.chars().count() > MAX_CONTENT_CHARS {
            return Err(DomainError::invalid_value(
                "content",
                format!("longer than {MAX_CONTENT_CHARS} characters"),
            ));
        }
        if !(0.0..=1.0).contains(&cmd.importance) {
            return Err(DomainError::invalid_value(
                "importance",
                "must be within 0..=1",
            ));
        }
        Ok(vec![MemoryItemEvent::Stored {
            memory_item_id: cmd.memory_item_id,
            kind: cmd.kind,
            content: cmd.content.clone(),
            tags: cmd.tags.clone(),
            source: cmd.source.clone(),
            scope: cmd.scope.clone(),
            embedding_model: cmd.embedding_model.clone(),
            importance: cmd.importance,
            created_at: cmd.created_at,
        }])
    }
}

impl Aggregate for MemoryItem {
    type Command = MemoryItemCommand;
    type Event = MemoryItemEvent;

    const TYPE: &'static str = MEMORY_ITEM_AGGREGATE_TYPE;

    fn id(&self) -> Uuid {
        self.id.as_uuid()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn handle(&self, cmd: &MemoryItemCommand) -> Result<Vec<MemoryItemEvent>, DomainError> {
        match cmd {
            MemoryItemCommand::Store(c) => self.handle_store(c),
            MemoryItemCommand::Supersede(c) => {
                self.require_exists()?;
                if self.status != MemoryItemStatus::Active {
                    return Err(DomainError::invalid_transition(
                        MEMORY_ITEM_AGGREGATE_TYPE,
                        self.status.as_str(),
                        cmd.name(),
                    ));
                }
                if c.superseded_by == self.id {
                    return Err(DomainError::invalid_value(
                        "superseded_by",
                        "an item cannot supersede itself",
                    ));
                }
                Ok(vec![MemoryItemEvent::Superseded {
                    superseded_by: c.superseded_by,
                }])
            }
            MemoryItemCommand::Forget(c) => {
                self.require_exists()?;
                if self.status == MemoryItemStatus::Forgotten {
                    return Err(DomainError::invalid_transition(
                        MEMORY_ITEM_AGGREGATE_TYPE,
                        self.status.as_str(),
                        cmd.name(),
                    ));
                }
                Ok(vec![MemoryItemEvent::Forgotten {
                    reason: c.reason.clone(),
                }])
            }
        }
    }

    fn apply(&mut self, event: &MemoryItemEvent) {
        self.version += 1;
        match event {
            MemoryItemEvent::Stored {
                memory_item_id,
                kind,
                content,
                tags,
                source,
                scope,
                embedding_model,
                importance,
                created_at,
            } => {
                self.id = *memory_item_id;
                self.kind = Some(*kind);
                self.content.clone_from(content);
                self.tags.clone_from(tags);
                self.source = Some(source.clone());
                self.scope = scope.clone();
                self.embedding_model.clone_from(embedding_model);
                self.importance = *importance;
                self.created_at = Some(*created_at);
                self.status = MemoryItemStatus::Active;
            }
            MemoryItemEvent::Superseded { superseded_by } => {
                self.superseded_by = Some(*superseded_by);
                self.status = MemoryItemStatus::Superseded;
            }
            MemoryItemEvent::Forgotten { .. } => {
                self.content.clear();
                self.status = MemoryItemStatus::Forgotten;
            }
        }
    }
}

impl MemoryItem {
    fn require_exists(&self) -> Result<(), DomainError> {
        if self.version == 0 {
            return Err(DomainError::NotFound {
                aggregate: MEMORY_ITEM_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        Ok(())
    }
}
