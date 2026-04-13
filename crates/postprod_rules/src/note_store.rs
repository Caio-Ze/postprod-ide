use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use collections::HashMap;
use fuzzy::StringMatchCandidate;
use gpui::{App, AppContext as _, Context, EventEmitter, SharedString, Task};
use heed::{Database, RoTxn, types::{SerdeJson, Str}};
use parking_lot::RwLock;
use rope::Rope;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteId(pub Uuid);

impl NoteId {
    pub fn new() -> Self {
        NoteId(Uuid::new_v4())
    }
}

impl std::fmt::Display for NoteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteMetadata {
    pub id: NoteId,
    pub title: Option<SharedString>,
    pub default: bool,
    pub assigned_automations: Vec<String>,
    pub saved_at: DateTime<Utc>,
}

pub struct NoteStore {
    env: heed::Env,
    metadata_cache: RwLock<MetadataCache>,
    metadata: Database<SerdeJson<NoteId>, SerdeJson<NoteMetadata>>,
    bodies: Database<SerdeJson<NoteId>, Str>,
}

pub struct NotesUpdatedEvent;

impl EventEmitter<NotesUpdatedEvent> for NoteStore {}

#[derive(Default)]
struct MetadataCache {
    metadata: Vec<NoteMetadata>,
    metadata_by_id: HashMap<NoteId, NoteMetadata>,
}

impl MetadataCache {
    fn from_db(
        db: Database<SerdeJson<NoteId>, SerdeJson<NoteMetadata>>,
        txn: &RoTxn,
    ) -> Result<Self> {
        let mut cache = MetadataCache::default();
        for result in db.iter(txn)? {
            let Ok((note_id, metadata)) = result else {
                log::warn!(
                    "Skipping unreadable note record in database: {:?}",
                    result.err()
                );
                continue;
            };
            cache.metadata.push(metadata.clone());
            cache.metadata_by_id.insert(note_id, metadata);
        }
        cache.sort();
        Ok(cache)
    }

    fn insert(&mut self, metadata: NoteMetadata) {
        self.metadata_by_id.insert(metadata.id, metadata.clone());
        if let Some(old_metadata) = self.metadata.iter_mut().find(|m| m.id == metadata.id) {
            *old_metadata = metadata;
        } else {
            self.metadata.push(metadata);
        }
        self.sort();
    }

    fn remove(&mut self, id: NoteId) {
        self.metadata.retain(|metadata| metadata.id != id);
        self.metadata_by_id.remove(&id);
    }

    fn sort(&mut self) {
        self.metadata.sort_unstable_by(|a, b| {
            a.title
                .cmp(&b.title)
                .then_with(|| b.saved_at.cmp(&a.saved_at))
        });
    }
}

impl NoteStore {
    pub fn new(db_path: PathBuf, cx: &App) -> Task<Result<Self>> {
        cx.background_spawn(async move {
            std::fs::create_dir_all(&db_path)?;

            let db_env = unsafe {
                heed::EnvOpenOptions::new()
                    .map_size(64 * 1024 * 1024) // 64MB — notes are small
                    .max_dbs(2)
                    .open(db_path)?
            };

            let mut txn = db_env.write_txn()?;
            let metadata = db_env.create_database(&mut txn, Some("note_metadata"))?;
            let bodies = db_env.create_database(&mut txn, Some("note_bodies"))?;
            txn.commit()?;

            let txn = db_env.read_txn()?;
            let metadata_cache = MetadataCache::from_db(metadata, &txn)?;
            txn.commit()?;

            Ok(NoteStore {
                env: db_env,
                metadata_cache: RwLock::new(metadata_cache),
                metadata,
                bodies,
            })
        })
    }

    pub fn load(&self, id: NoteId, cx: &App) -> Task<Result<String>> {
        let env = self.env.clone();
        let bodies = self.bodies;
        cx.background_spawn(async move {
            let txn = env.read_txn()?;
            match bodies.get(&txn, &id)? {
                Some(body) => Ok(body.into()),
                None => Err(anyhow!("note not found")),
            }
        })
    }

    pub fn all_metadata(&self) -> Vec<NoteMetadata> {
        self.metadata_cache.read().metadata.clone()
    }

    pub fn default_metadata(&self) -> Vec<NoteMetadata> {
        self.metadata_cache
            .read()
            .metadata
            .iter()
            .filter(|m| m.default)
            .cloned()
            .collect()
    }

    pub fn metadata(&self, id: NoteId) -> Option<NoteMetadata> {
        self.metadata_cache.read().metadata_by_id.get(&id).cloned()
    }

    pub fn first(&self) -> Option<NoteMetadata> {
        self.metadata_cache.read().metadata.first().cloned()
    }

    /// Returns all notes that should be injected for a given automation:
    /// notes marked as default, plus notes explicitly assigned to the automation ID.
    pub fn notes_for_automation(&self, automation_id: &str) -> Vec<NoteMetadata> {
        self.metadata_cache
            .read()
            .metadata
            .iter()
            .filter(|m| m.default || m.assigned_automations.iter().any(|a| a == automation_id))
            .cloned()
            .collect()
    }

    pub fn search(
        &self,
        query: String,
        cancellation_flag: Arc<AtomicBool>,
        cx: &App,
    ) -> Task<Vec<NoteMetadata>> {
        let cached_metadata = self.metadata_cache.read().metadata.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut matches = if query.is_empty() {
                cached_metadata
            } else {
                let candidates = cached_metadata
                    .iter()
                    .enumerate()
                    .filter_map(|(ix, metadata)| {
                        Some(StringMatchCandidate::new(ix, metadata.title.as_ref()?))
                    })
                    .collect::<Vec<_>>();
                let matches = fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &cancellation_flag,
                    executor,
                )
                .await;
                matches
                    .into_iter()
                    .map(|mat| cached_metadata[mat.candidate_id].clone())
                    .collect()
            };
            matches.sort_by_key(|metadata| Reverse(metadata.default));
            matches
        })
    }

    pub fn save(
        &self,
        id: NoteId,
        title: Option<SharedString>,
        default: bool,
        assigned_automations: Vec<String>,
        body: Rope,
        cx: &Context<Self>,
    ) -> Task<Result<()>> {
        let body = body.to_string();
        let metadata = NoteMetadata {
            id,
            title,
            default,
            assigned_automations,
            saved_at: Utc::now(),
        };

        self.metadata_cache.write().insert(metadata.clone());

        let db_connection = self.env.clone();
        let bodies_db = self.bodies;
        let metadata_db = self.metadata;

        let task = cx.background_spawn(async move {
            let mut txn = db_connection.write_txn()?;
            metadata_db.put(&mut txn, &id, &metadata)?;
            bodies_db.put(&mut txn, &id, &body)?;
            txn.commit()?;
            anyhow::Ok(())
        });

        cx.spawn(async move |this, cx| {
            task.await?;
            this.update(cx, |_, cx| cx.emit(NotesUpdatedEvent)).ok();
            anyhow::Ok(())
        })
    }

    pub fn save_metadata(
        &self,
        id: NoteId,
        title: Option<SharedString>,
        default: bool,
        assigned_automations: Vec<String>,
        cx: &Context<Self>,
    ) -> Task<Result<()>> {
        let metadata = NoteMetadata {
            id,
            title,
            default,
            assigned_automations,
            saved_at: Utc::now(),
        };

        self.metadata_cache.write().insert(metadata.clone());

        let db_connection = self.env.clone();
        let metadata_db = self.metadata;

        let task = cx.background_spawn(async move {
            let mut txn = db_connection.write_txn()?;
            metadata_db.put(&mut txn, &id, &metadata)?;
            txn.commit()?;
            anyhow::Ok(())
        });

        cx.spawn(async move |this, cx| {
            task.await?;
            this.update(cx, |_, cx| cx.emit(NotesUpdatedEvent)).ok();
            anyhow::Ok(())
        })
    }

    pub fn delete(&self, id: NoteId, cx: &Context<Self>) -> Task<Result<()>> {
        self.metadata_cache.write().remove(id);

        let db_connection = self.env.clone();
        let bodies_db = self.bodies;
        let metadata_db = self.metadata;

        let task = cx.background_spawn(async move {
            let mut txn = db_connection.write_txn()?;
            metadata_db.delete(&mut txn, &id)?;
            bodies_db.delete(&mut txn, &id)?;
            txn.commit()?;
            anyhow::Ok(())
        });

        cx.spawn(async move |this, cx| {
            task.await?;
            this.update(cx, |_, cx| cx.emit(NotesUpdatedEvent)).ok();
            anyhow::Ok(())
        })
    }
}
