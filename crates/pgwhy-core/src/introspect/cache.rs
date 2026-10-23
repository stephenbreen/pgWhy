//! The introspection service: lazily-filled, per-(connection, database) catalog cache over
//! the shared introspection connections.
//!
//! Serve-cached semantics: every getter returns cached data when present and only goes to the
//! server on a miss; `refresh_*` drop and re-pull one level. Staleness is the UI's to surface
//! (refresh affordances, "object no longer exists" on a refetch miss) — the cache never
//! refreshes behind the user's back.
//!
//! Invalidation is epoch-based: a pull that started before an invalidation never writes its
//! (possibly pre-swap) result into the cache, though it is still returned to its caller.

use super::model::*;
use super::queries::Catalog;
use crate::config::ConnectionId;
use crate::conn::{ConnectionManager, PgConn};
use crate::AppResult;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct CacheState {
    /// Bumped by every invalidation/refresh; pulls only store results under the epoch they
    /// started in.
    epoch: u64,
    servers: HashMap<ConnectionId, ServerCache>,
}

#[derive(Default)]
struct ServerCache {
    databases: Option<Arc<Vec<DatabaseInfo>>>,
    dbs: HashMap<String, DbCache>,
}

#[derive(Default)]
struct DbCache {
    schemas: Option<Arc<Vec<SchemaInfo>>>,
    contents: HashMap<String, Arc<SchemaContents>>,
    functions: HashMap<String, Arc<Vec<FunctionInfo>>>,
    details: HashMap<QualifiedName, Arc<RelationDetails>>,
}

impl CacheState {
    fn server(&self, id: &ConnectionId) -> Option<&ServerCache> {
        self.servers.get(id)
    }

    fn db(&self, id: &ConnectionId, db: &str) -> Option<&DbCache> {
        self.servers.get(id)?.dbs.get(db)
    }

    fn db_mut(&mut self, id: &ConnectionId, db: &str) -> &mut DbCache {
        self.servers
            .entry(id.clone())
            .or_default()
            .dbs
            .entry(db.to_string())
            .or_default()
    }
}

/// Catalog access for the tree, details panel, completion, Script-as and the grid's edit
/// provenance. Cheap to share behind an `Arc`.
pub struct Introspector {
    manager: ConnectionManager,
    state: Mutex<CacheState>,
    round_trips: AtomicU64,
}

impl Introspector {
    pub fn new(manager: ConnectionManager) -> Self {
        Self {
            manager,
            state: Mutex::default(),
            round_trips: AtomicU64::new(0),
        }
    }

    pub fn manager(&self) -> &ConnectionManager {
        &self.manager
    }

    /// Catalog queries issued so far (each is one network round trip). For performance-guard
    /// tests and diagnostics.
    pub fn round_trips(&self) -> u64 {
        self.round_trips.load(Ordering::Relaxed)
    }

    fn state(&self) -> MutexGuard<'_, CacheState> {
        // a panic while holding the lock can only leave a partially-updated cache; serving
        // it is better than poisoning every later call
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn epoch(&self) -> u64 {
        self.state().epoch
    }

    /// Run `f` on the cache only if no invalidation happened since `epoch`.
    fn store(&self, epoch: u64, f: impl FnOnce(&mut CacheState)) {
        let mut st = self.state();
        if st.epoch == epoch {
            f(&mut st);
        }
    }

    fn catalog<'a>(&'a self, conn: &'a PgConn) -> Catalog<'a> {
        Catalog {
            conn,
            counter: &self.round_trips,
        }
    }

    async fn conn(&self, id: &ConnectionId, db: &str) -> AppResult<Arc<PgConn>> {
        self.manager.introspection(id, db).await
    }

    /// A short-lived connection to the entry's maintenance database for server-level reads
    /// (the database list). Deliberately not the shared introspection connection: listing
    /// databases must not leave the maintenance database looking "open" in the tree.
    async fn maintenance_conn(&self, id: &ConnectionId) -> AppResult<PgConn> {
        let maintenance_db = self.manager.entry(id)?.maintenance_db;
        self.manager
            .open(id, &maintenance_db, crate::conn::Purpose::Maintenance)
            .await
    }

    // ------------------------------------------------------------------ databases

    /// Databases on the server, read through the entry's maintenance database.
    pub async fn databases(&self, id: &ConnectionId) -> AppResult<Arc<Vec<DatabaseInfo>>> {
        if let Some(d) = self.state().server(id).and_then(|s| s.databases.clone()) {
            return Ok(d);
        }
        self.load_databases(id).await
    }

    pub async fn refresh_databases(&self, id: &ConnectionId) -> AppResult<Arc<Vec<DatabaseInfo>>> {
        {
            let mut st = self.state();
            st.epoch += 1;
            if let Some(s) = st.servers.get_mut(id) {
                s.databases = None;
            }
        }
        self.load_databases(id).await
    }

    async fn load_databases(&self, id: &ConnectionId) -> AppResult<Arc<Vec<DatabaseInfo>>> {
        let epoch = self.epoch();
        let conn = self.maintenance_conn(id).await?;
        let dbs = Arc::new(self.catalog(&conn).databases().await?);
        self.store(epoch, |st| {
            st.servers.entry(id.clone()).or_default().databases = Some(dbs.clone());
        });
        Ok(dbs)
    }

    /// On-demand database size (never cached: it walks the database directory).
    /// `None` when the role may not connect to it.
    pub async fn database_size(&self, id: &ConnectionId, db: &str) -> AppResult<Option<i64>> {
        let conn = self.maintenance_conn(id).await?;
        self.catalog(&conn).database_size(db).await
    }

    // ------------------------------------------------------------------ schemas

    /// Schemas of `db` (opens that database's introspection connection lazily).
    pub async fn schemas(&self, id: &ConnectionId, db: &str) -> AppResult<Arc<Vec<SchemaInfo>>> {
        if let Some(s) = self.state().db(id, db).and_then(|d| d.schemas.clone()) {
            return Ok(s);
        }
        self.load_schemas(id, db).await
    }

    pub async fn refresh_schemas(
        &self,
        id: &ConnectionId,
        db: &str,
    ) -> AppResult<Arc<Vec<SchemaInfo>>> {
        {
            let mut st = self.state();
            st.epoch += 1;
            st.db_mut(id, db).schemas = None;
        }
        self.load_schemas(id, db).await
    }

    async fn load_schemas(&self, id: &ConnectionId, db: &str) -> AppResult<Arc<Vec<SchemaInfo>>> {
        let epoch = self.epoch();
        let conn = self.conn(id, db).await?;
        let schemas = Arc::new(self.catalog(&conn).schemas().await?);
        self.store(epoch, |st| {
            st.db_mut(id, db).schemas = Some(schemas.clone())
        });
        Ok(schemas)
    }

    // ------------------------------------------------------------------ schema contents

    /// Relations of one schema with their columns — exactly one round trip on a miss.
    pub async fn schema(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
    ) -> AppResult<Arc<SchemaContents>> {
        if let Some(c) = self
            .state()
            .db(id, db)
            .and_then(|d| d.contents.get(schema).cloned())
        {
            return Ok(c);
        }
        self.load_schema(id, db, schema).await
    }

    /// Drop everything cached for one schema (contents, functions, details) and re-pull its
    /// contents.
    pub async fn refresh_schema(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
    ) -> AppResult<Arc<SchemaContents>> {
        {
            let mut st = self.state();
            st.epoch += 1;
            let d = st.db_mut(id, db);
            d.contents.remove(schema);
            d.functions.remove(schema);
            d.details.retain(|k, _| k.schema != schema);
        }
        self.load_schema(id, db, schema).await
    }

    async fn load_schema(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
    ) -> AppResult<Arc<SchemaContents>> {
        let epoch = self.epoch();
        let conn = self.conn(id, db).await?;
        let relations = self.catalog(&conn).relations(schema, None).await?;
        let contents = Arc::new(SchemaContents {
            schema: schema.to_string(),
            relations,
        });
        self.store(epoch, |st| {
            st.db_mut(id, db)
                .contents
                .insert(schema.to_string(), contents.clone());
        });
        Ok(contents)
    }

    /// One relation (with columns) from the schema's cached contents, loading the schema on
    /// a miss. `None` when no such relation exists.
    pub async fn relation(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
        name: &str,
    ) -> AppResult<Option<RelationInfo>> {
        Ok(self.schema(id, db, schema).await?.relation(name).cloned())
    }

    /// Functions, procedures, aggregates and window functions of one schema (its own lazy
    /// pull, so the tree's schema expansion stays at one round trip).
    pub async fn functions(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
    ) -> AppResult<Arc<Vec<FunctionInfo>>> {
        if let Some(f) = self
            .state()
            .db(id, db)
            .and_then(|d| d.functions.get(schema).cloned())
        {
            return Ok(f);
        }
        let epoch = self.epoch();
        let conn = self.conn(id, db).await?;
        let functions = Arc::new(self.catalog(&conn).functions(schema).await?);
        self.store(epoch, |st| {
            st.db_mut(id, db)
                .functions
                .insert(schema.to_string(), functions.clone());
        });
        Ok(functions)
    }

    // ------------------------------------------------------------------ details

    /// Details-panel data for one relation (cached after the first select). `Ok(None)` means
    /// the relation no longer exists — the panel's "object no longer exists" state.
    pub async fn relation_details(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
        name: &str,
    ) -> AppResult<Option<Arc<RelationDetails>>> {
        let key = QualifiedName::new(schema, name);
        if let Some(d) = self
            .state()
            .db(id, db)
            .and_then(|d| d.details.get(&key).cloned())
        {
            return Ok(Some(d));
        }
        self.load_details(id, db, key).await
    }

    /// Re-read one relation's details *and* its column list (patching the cached schema
    /// contents), e.g. on the panel's refresh button or before a grid commit revalidates.
    pub async fn refresh_relation(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
        name: &str,
    ) -> AppResult<Option<Arc<RelationDetails>>> {
        let key = QualifiedName::new(schema, name);
        {
            let mut st = self.state();
            st.epoch += 1;
            st.db_mut(id, db).details.remove(&key);
        }
        self.load_details(id, db, key).await
    }

    /// The grid's edit key for a relation (from cached details). `Ok(None)` when the relation
    /// no longer exists.
    pub async fn edit_provenance(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
        name: &str,
    ) -> AppResult<Option<EditProvenance>> {
        Ok(self
            .relation_details(id, db, schema, name)
            .await?
            .map(|d| d.provenance.clone()))
    }

    async fn load_details(
        &self,
        id: &ConnectionId,
        db: &str,
        key: QualifiedName,
    ) -> AppResult<Option<Arc<RelationDetails>>> {
        let epoch = self.epoch();
        let conn = self.conn(id, db).await?;
        let cat = self.catalog(&conn);
        let (schema, name) = (key.schema.as_str(), key.name.as_str());
        // four statements, pipelined on the one connection (sent back to back)
        let (mut relation, constraints, indexes, facts) = futures_util::try_join!(
            cat.relations(schema, Some(name)),
            cat.constraints(schema, name),
            cat.indexes(schema, name),
            cat.facts(schema, name),
        )?;
        let (Some(relation), Some(facts)) = (relation.pop(), facts) else {
            self.store(epoch, |st| {
                let d = st.db_mut(id, db);
                d.details.remove(&key);
                patch_contents(d, &key, None);
            });
            return Ok(None);
        };
        let provenance = EditProvenance::derive(relation.kind, &indexes);
        let details = Arc::new(RelationDetails {
            relation,
            constraints,
            indexes,
            stats: facts.stats,
            view_definition: facts.view_definition,
            updatability: facts.updatability,
            sequence: facts.sequence,
            provenance,
        });
        self.store(epoch, |st| {
            let d = st.db_mut(id, db);
            patch_contents(d, &key, Some(&details.relation));
            d.details.insert(key, details.clone());
        });
        Ok(Some(details))
    }

    // ------------------------------------------------------------------ search

    /// Go-to-object: tables, views, sequences and routines whose name contains `pattern`
    /// (case-insensitive, `%`/`_` literal), outside system schemas. Exact matches first, then
    /// prefix matches, then shorter names. Answered from the cache when every user schema of
    /// `db` is fully loaded, otherwise by one name-only catalog query.
    pub async fn search_objects(
        &self,
        id: &ConnectionId,
        db: &str,
        pattern: &str,
        limit: usize,
    ) -> AppResult<Vec<SearchHit>> {
        if let Some(hits) = self.search_cached(id, db, pattern, limit) {
            return Ok(hits);
        }
        let conn = self.conn(id, db).await?;
        self.catalog(&conn)
            .search(pattern, i64::try_from(limit).unwrap_or(i64::MAX))
            .await
    }

    fn search_cached(
        &self,
        id: &ConnectionId,
        db: &str,
        pattern: &str,
        limit: usize,
    ) -> Option<Vec<SearchHit>> {
        let st = self.state();
        let d = st.db(id, db)?;
        let schemas = d.schemas.as_ref()?;
        let needle = pattern.to_lowercase();
        let mut hits = Vec::new();
        for s in schemas.iter().filter(|s| !s.is_system) {
            let contents = d.contents.get(&s.name)?;
            let functions = d.functions.get(&s.name)?;
            for r in &contents.relations {
                if r.name.to_lowercase().contains(&needle) {
                    hits.push(SearchHit {
                        schema: r.schema.clone(),
                        name: r.name.clone(),
                        kind: r.kind.into(),
                        identity_args: None,
                    });
                }
            }
            for f in functions.iter() {
                if f.name.to_lowercase().contains(&needle) {
                    hits.push(SearchHit {
                        schema: f.schema.clone(),
                        name: f.name.clone(),
                        kind: f.kind.into(),
                        identity_args: Some(f.identity_args.clone()),
                    });
                }
            }
        }
        hits.sort_by(|a, b| {
            let rank = |h: &SearchHit| {
                let l = h.name.to_lowercase();
                (l != needle, !l.starts_with(&needle), h.name.len())
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.schema.cmp(&b.schema))
        });
        hits.truncate(limit);
        Some(hits)
    }

    // ------------------------------------------------------------------ cache peeks

    /// Cached schema contents without any I/O (completion, the tree's filter box).
    pub fn cached_schema(
        &self,
        id: &ConnectionId,
        db: &str,
        schema: &str,
    ) -> Option<Arc<SchemaContents>> {
        self.state().db(id, db)?.contents.get(schema).cloned()
    }

    /// Every loaded schema's contents for `db`, without any I/O.
    pub fn cached_contents(&self, id: &ConnectionId, db: &str) -> Vec<Arc<SchemaContents>> {
        let st = self.state();
        let mut out: Vec<_> = st
            .db(id, db)
            .map(|d| d.contents.values().cloned().collect())
            .unwrap_or_default();
        out.sort_by(|a, b| a.schema.cmp(&b.schema));
        out
    }

    // ------------------------------------------------------------------ invalidation

    /// Forget everything cached for one database (it was dropped, replaced, or DDL ran).
    pub fn invalidate_db(&self, id: &ConnectionId, db: &str) {
        let mut st = self.state();
        st.epoch += 1;
        if let Some(s) = st.servers.get_mut(id) {
            s.dbs.remove(db);
        }
    }

    /// Forget everything cached for a connection entry: database list and every database
    /// (post-swap, entry edited, user disconnect).
    pub fn invalidate_server(&self, id: &ConnectionId) {
        let mut st = self.state();
        st.epoch += 1;
        st.servers.remove(id);
    }
}

/// Keep the schema-contents cache consistent with a fresher single-relation read.
fn patch_contents(d: &mut DbCache, key: &QualifiedName, fresh: Option<&RelationInfo>) {
    let Some(contents) = d.contents.get_mut(&key.schema) else {
        return;
    };
    let pos = contents.relations.iter().position(|r| r.name == key.name);
    let unchanged = match (pos, fresh) {
        (Some(i), Some(f)) => &contents.relations[i] == f,
        (None, None) => true,
        _ => false,
    };
    if unchanged {
        return;
    }
    let c = Arc::make_mut(contents);
    match (pos, fresh) {
        (Some(i), Some(f)) => c.relations[i] = f.clone(),
        (Some(i), None) => {
            c.relations.remove(i);
        }
        (None, Some(f)) => {
            let at = c
                .relations
                .binary_search_by(|r| r.name.as_str().cmp(&f.name))
                .unwrap_or_else(|i| i);
            c.relations.insert(at, f.clone());
        }
        (None, None) => {}
    }
}
