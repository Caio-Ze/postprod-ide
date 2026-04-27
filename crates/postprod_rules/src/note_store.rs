use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use collections::HashMap;
use futures::{FutureExt as _, future::Shared};
use fuzzy::StringMatchCandidate;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use heed::{
    Database, RoTxn,
    types::{SerdeJson, Str},
};
use parking_lot::{Mutex, RwLock};
use rope::Rope;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
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

    /// Synchronous body read. LMDB reads are microseconds.
    /// Use for note injection in the prompt assembly path.
    pub fn load_body_sync(&self, id: NoteId) -> Result<String> {
        let txn = self.env.read_txn()?;
        match self.bodies.get(&txn, &id)? {
            Some(body) => Ok(body.into()),
            None => Err(anyhow!("note not found")),
        }
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

pub type SharedNoteStoreFuture =
    Shared<Task<std::result::Result<Entity<NoteStore>, Arc<anyhow::Error>>>>;

#[derive(Default)]
pub struct GlobalNoteStores {
    by_path: Arc<Mutex<HashMap<PathBuf, SharedNoteStoreFuture>>>,
}

impl Global for GlobalNoteStores {}

impl NoteStore {
    /// Process-singleton accessor. Returns a shared future that resolves to
    /// `Entity<NoteStore>`; concurrent callers for the same canonical path
    /// receive the same future (and after resolution, the same Entity), so
    /// LMDB never sees a duplicate `open()` on one path in one process.
    ///
    /// Path canonicalization uses `std::fs::canonicalize` to match heed's
    /// own internal dedup keying. Falls back to the input path verbatim
    /// when the parent directory does not exist yet (LMDB will create the
    /// dir on open).
    pub fn for_path(db_path: PathBuf, cx: &mut App) -> SharedNoteStoreFuture {
        let canonical = canonicalize_for_singleton(&db_path);

        // Clone the Arc first so the `&mut App` borrow obtained via
        // `default_global` ends here. After this line, `cx` is free to
        // re-borrow for `NoteStore::new` and `cx.spawn`, while the lock
        // is held across both — making the lookup → create → insert
        // sequence atomic by construction.
        let map_arc = cx.default_global::<GlobalNoteStores>().by_path.clone();
        let mut map = map_arc.lock();

        if let Some(shared) = map.get(&canonical) {
            return shared.clone();
        }

        let open_task = NoteStore::new(canonical.clone(), cx);
        let pending: SharedNoteStoreFuture = cx
            .spawn(async move |cx| {
                open_task
                    .await
                    .map(|store| cx.new(|_| store))
                    .map_err(Arc::new)
            })
            .shared();

        map.insert(canonical, pending.clone());
        pending
    }
}

fn canonicalize_for_singleton(path: &Path) -> PathBuf {
    // For a yet-to-be-created LMDB dir, `canonicalize` fails. Canonicalize
    // the deepest existing ancestor and re-attach the missing tail so two
    // callers passing the same logical path always hash to the same key
    // even when the dir hasn't been created yet.
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    let mut existing = path;
    let mut tail = PathBuf::new();
    while !existing.exists() {
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        if let Some(name) = existing.file_name() {
            tail = PathBuf::from(name).join(&tail);
        }
        existing = parent;
    }
    std::fs::canonicalize(existing)
        .map(|p| p.join(tail))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext};
    use rope::Rope;

    async fn create_test_store(cx: &mut TestAppContext) -> (tempfile::TempDir, Entity<NoteStore>) {
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("notes-db");
        let store = cx.update(|cx| NoteStore::new(db_path, cx)).await.unwrap();
        let entity = cx.new(|_cx| store);
        (temp_dir, entity)
    }

    // -----------------------------------------------------------------------
    // 8.1 — CRUD tests
    // -----------------------------------------------------------------------

    #[gpui::test]
    async fn create_and_load(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        let id = NoteId::new();
        store
            .update(cx, |s, cx| {
                s.save(
                    id,
                    Some("Test Note".into()),
                    false,
                    vec![],
                    Rope::from("Hello, world!"),
                    cx,
                )
            })
            .await
            .unwrap();

        let body = store.update(cx, |s, _cx| s.load_body_sync(id)).unwrap();
        assert_eq!(body, "Hello, world!");

        let meta = store.update(cx, |s, _cx| s.metadata(id)).unwrap();
        assert_eq!(meta.title, Some(SharedString::from("Test Note")));
        assert!(!meta.default);
        assert!(meta.assigned_automations.is_empty());
    }

    #[gpui::test]
    async fn update_title_and_body(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        let id = NoteId::new();
        store
            .update(cx, |s, cx| {
                s.save(
                    id,
                    Some("Original".into()),
                    false,
                    vec![],
                    Rope::from("original body"),
                    cx,
                )
            })
            .await
            .unwrap();

        store
            .update(cx, |s, cx| {
                s.save(
                    id,
                    Some("Updated".into()),
                    false,
                    vec![],
                    Rope::from("updated body"),
                    cx,
                )
            })
            .await
            .unwrap();

        let meta = store.update(cx, |s, _cx| s.metadata(id)).unwrap();
        assert_eq!(meta.title, Some(SharedString::from("Updated")));

        let body = store.update(cx, |s, _cx| s.load_body_sync(id)).unwrap();
        assert_eq!(body, "updated body");
    }

    #[gpui::test]
    async fn delete_removes(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        let id = NoteId::new();
        store
            .update(cx, |s, cx| {
                s.save(
                    id,
                    Some("Doomed".into()),
                    false,
                    vec![],
                    Rope::from("bye"),
                    cx,
                )
            })
            .await
            .unwrap();

        store.update(cx, |s, cx| s.delete(id, cx)).await.unwrap();

        let meta = store.update(cx, |s, _cx| s.metadata(id));
        assert!(meta.is_none());

        let body = store.update(cx, |s, _cx| s.load_body_sync(id));
        assert!(body.is_err());
    }

    #[gpui::test]
    async fn all_metadata_returns_all(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        for i in 0..3 {
            store
                .update(cx, |s, cx| {
                    s.save(
                        NoteId::new(),
                        Some(SharedString::from(format!("Note {i}"))),
                        false,
                        vec![],
                        Rope::from("body"),
                        cx,
                    )
                })
                .await
                .unwrap();
        }

        let all = store.update(cx, |s, _cx| s.all_metadata());
        assert_eq!(all.len(), 3);
    }

    #[gpui::test]
    async fn first_returns_alphabetically(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        store
            .update(cx, |s, cx| {
                s.save(
                    NoteId::new(),
                    Some("Zebra".into()),
                    false,
                    vec![],
                    Rope::from("z"),
                    cx,
                )
            })
            .await
            .unwrap();

        store
            .update(cx, |s, cx| {
                s.save(
                    NoteId::new(),
                    Some("Alpha".into()),
                    false,
                    vec![],
                    Rope::from("a"),
                    cx,
                )
            })
            .await
            .unwrap();

        let first = store.update(cx, |s, _cx| s.first()).unwrap();
        assert_eq!(first.title, Some(SharedString::from("Alpha")));
    }

    // -----------------------------------------------------------------------
    // 8.2 — Query tests
    // -----------------------------------------------------------------------

    #[gpui::test]
    async fn default_metadata_filters(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        store
            .update(cx, |s, cx| {
                s.save(
                    NoteId::new(),
                    Some("Default note".into()),
                    true,
                    vec![],
                    Rope::from("d"),
                    cx,
                )
            })
            .await
            .unwrap();

        store
            .update(cx, |s, cx| {
                s.save(
                    NoteId::new(),
                    Some("Regular note".into()),
                    false,
                    vec![],
                    Rope::from("r"),
                    cx,
                )
            })
            .await
            .unwrap();

        let defaults = store.update(cx, |s, _cx| s.default_metadata());
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].title, Some(SharedString::from("Default note")));
    }

    #[gpui::test]
    async fn notes_for_automation_filters(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        // A default note (applies to all)
        store
            .update(cx, |s, cx| {
                s.save(
                    NoteId::new(),
                    Some("Default".into()),
                    true,
                    vec![],
                    Rope::from("d"),
                    cx,
                )
            })
            .await
            .unwrap();

        // Assigned to "build-report"
        store
            .update(cx, |s, cx| {
                s.save(
                    NoteId::new(),
                    Some("Build rules".into()),
                    false,
                    vec!["build-report".into()],
                    Rope::from("b"),
                    cx,
                )
            })
            .await
            .unwrap();

        // Assigned to "mix-check"
        store
            .update(cx, |s, cx| {
                s.save(
                    NoteId::new(),
                    Some("Mix rules".into()),
                    false,
                    vec!["mix-check".into()],
                    Rope::from("m"),
                    cx,
                )
            })
            .await
            .unwrap();

        let build_notes = store.update(cx, |s, _cx| s.notes_for_automation("build-report"));
        assert_eq!(build_notes.len(), 2); // default + assigned

        let unknown_notes = store.update(cx, |s, _cx| s.notes_for_automation("unknown"));
        assert_eq!(unknown_notes.len(), 1); // default only
    }

    // -----------------------------------------------------------------------
    // 8.3 — Assignment round-trip tests
    // -----------------------------------------------------------------------

    #[gpui::test]
    async fn save_metadata_updates_assignments(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        let id = NoteId::new();
        store
            .update(cx, |s, cx| {
                s.save(
                    id,
                    Some("Note".into()),
                    false,
                    vec!["a".into(), "b".into()],
                    Rope::from("body"),
                    cx,
                )
            })
            .await
            .unwrap();

        store
            .update(cx, |s, cx| {
                s.save_metadata(
                    id,
                    Some("Note".into()),
                    false,
                    vec!["b".into(), "c".into()],
                    cx,
                )
            })
            .await
            .unwrap();

        let meta = store.update(cx, |s, _cx| s.metadata(id)).unwrap();
        assert_eq!(meta.assigned_automations, vec!["b", "c"]);
    }

    #[gpui::test]
    async fn clear_assignments(cx: &mut TestAppContext) {
        let (_temp_dir, store) = create_test_store(cx).await;

        let id = NoteId::new();
        store
            .update(cx, |s, cx| {
                s.save(
                    id,
                    Some("Note".into()),
                    false,
                    vec!["a".into()],
                    Rope::from("body"),
                    cx,
                )
            })
            .await
            .unwrap();

        store
            .update(cx, |s, cx| {
                s.save_metadata(id, Some("Note".into()), false, vec![], cx)
            })
            .await
            .unwrap();

        let notes = store.update(cx, |s, _cx| s.notes_for_automation("a"));
        assert!(
            notes.iter().all(|n| n.id != id),
            "cleared note should not appear for automation 'a'"
        );
    }

    // -----------------------------------------------------------------------
    // Singleton accessor (`NoteStore::for_path`) — covers the multi-workspace
    // bug where two `DashboardItem`s on the same config-root state dir would
    // each call `NoteStore::new` and the second `heed::EnvOpenOptions::open`
    // would fail with `EnvAlreadyOpened`.
    // -----------------------------------------------------------------------

    #[gpui::test]
    async fn for_path_dedupes_same_path(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("notes.mdb");

        let entity_a = cx
            .update(|cx| NoteStore::for_path(db_path.clone(), cx))
            .await
            .unwrap();
        let entity_b = cx
            .update(|cx| NoteStore::for_path(db_path.clone(), cx))
            .await
            .unwrap();

        assert_eq!(
            entity_a.entity_id(),
            entity_b.entity_id(),
            "second for_path call must return the same Entity, not open a duplicate env"
        );
    }

    #[gpui::test]
    async fn for_path_isolates_distinct_paths(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let db_a = tmp.path().join("notes_a.mdb");
        let db_b = tmp.path().join("notes_b.mdb");

        let entity_a = cx
            .update(|cx| NoteStore::for_path(db_a.clone(), cx))
            .await
            .unwrap();
        let entity_b = cx
            .update(|cx| NoteStore::for_path(db_b.clone(), cx))
            .await
            .unwrap();

        assert_ne!(
            entity_a.entity_id(),
            entity_b.entity_id(),
            "distinct canonical paths must produce distinct Entities"
        );

        let id = NoteId::new();
        entity_a
            .update(cx, |store, cx| {
                store.save(id, None, false, Vec::new(), Rope::from("A only"), cx)
            })
            .await
            .unwrap();

        let a_metadata = entity_a.read_with(cx, |store, _| store.all_metadata());
        let b_metadata = entity_b.read_with(cx, |store, _| store.all_metadata());
        assert!(
            a_metadata.iter().any(|m| m.id == id),
            "A must contain the note it just saved"
        );
        assert!(
            b_metadata.iter().all(|m| m.id != id),
            "B must NOT see notes saved against A's env"
        );
    }

    #[gpui::test]
    async fn for_path_concurrent_callers_share(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("notes.mdb");

        let f_a = cx.update(|cx| NoteStore::for_path(db.clone(), cx));
        let f_b = cx.update(|cx| NoteStore::for_path(db.clone(), cx));

        let (a, b) = futures::join!(f_a, f_b);
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.entity_id(), b.entity_id());
    }
}
