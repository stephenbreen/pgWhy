//! Top-N grid + editable layer: each grid owns a dedicated connection; sort
//! and filter re-query the server; edits stay pending in the core-side result set until a
//! single-transaction, row-count-verified commit.

use crate::state::AppState;
use pgwhy_core::config::{ConnectionId, ServerKey};
use pgwhy_core::conn::{PgConn, Purpose};
use pgwhy_core::query::{
    build_topn, commit_edits, plan_edits, run_topn, CommitOptions, CommitReport, Criterion,
    ResultSet, SortKey, TopNQuery, TopNRequest,
};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::State;

pub struct Grid {
    conn: PgConn,
    server: ServerKey,
    database: String,
    request: TopNRequest,
    query: TopNQuery,
    result: Arc<ResultSet>,
}

#[derive(Default)]
pub struct Grids {
    grids: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Grid>>>>,
}

impl Grids {
    fn get(&self, id: &str) -> AppResult<Arc<tokio::sync::Mutex<Grid>>> {
        self.grids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                AppError::new(ErrorKind::Disconnected, "this grid is closed")
                    .with_hint("open the table again")
            })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiSort {
    pub column: String,
    pub dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GridColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
    pub key: bool,
    pub writable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GridView {
    pub result_id: String,
    pub columns: Vec<GridColumn>,
    pub row_count: usize,
    pub sql: String,
    pub editable: bool,
    pub read_only_reason: Option<String>,
    pub n: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingInfo {
    pub pending: usize,
    pub row_count: usize,
}

fn view(g: &Grid) -> GridView {
    GridView {
        result_id: g.result.id().to_string(),
        columns: g
            .query
            .columns
            .iter()
            .map(|c| GridColumn {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                key: c.is_key,
                writable: c.writable,
            })
            .collect(),
        row_count: g.result.visible_row_count(),
        sql: g.query.display_sql.clone(),
        editable: g.result.edit_target().is_some(),
        read_only_reason: g.query.read_only_reason.clone(),
        n: g.request.limit,
    }
}

fn pending(g: &Grid) -> PendingInfo {
    PendingInfo {
        pending: g.result.pending_counts().total_changes,
        row_count: g.result.visible_row_count(),
    }
}

fn sort_keys(sort: Vec<UiSort>) -> Vec<SortKey> {
    sort.into_iter()
        .map(|s| SortKey {
            column: s.column,
            descending: s.dir == "desc",
        })
        .collect()
}

/// Build + run the query into a fresh result set registered to the grid.
async fn execute(
    state: &AppState,
    grid_id: &str,
    conn: &PgConn,
    request: &TopNRequest,
) -> AppResult<(TopNQuery, Arc<ResultSet>)> {
    // With no explicit sort, rows come back in key order so "top N" is stable across edits
    // and re-queries (an UPDATE moves a row to the end of the heap; unordered, it would
    // silently drop out of the view).
    let mut request = request.clone();
    if request.sort.is_empty() {
        request.sort = request
            .key_columns
            .iter()
            .map(|k| SortKey {
                column: k.clone(),
                descending: false,
            })
            .collect();
    }
    let query = build_topn(&request)?;
    let result = Arc::new(query.new_result_set(1_000_000));
    state.results.register(grid_id, result.clone());
    run_topn(&conn.client, &query, &result).await?;
    Ok((query, result))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn grid_open(
    state: State<'_, AppState>,
    grid_id: String,
    connection_id: ConnectionId,
    database: String,
    schema: String,
    name: String,
    n: Option<u64>,
    criteria: Vec<Criterion>,
    sort: Vec<UiSort>,
) -> AppResult<GridView> {
    let details = state
        .introspector
        .refresh_relation(&connection_id, &database, &schema, &name)
        .await?
        .ok_or_else(|| {
            AppError::new(
                ErrorKind::Config,
                format!("{schema}.{name} no longer exists"),
            )
        })?;
    let limit = n.unwrap_or_else(|| {
        state
            .settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .top_n as u64
    });
    let mut request = TopNRequest::for_relation(&details.relation, &details.provenance, limit);
    request.criteria = criteria;
    request.sort = sort_keys(sort);
    let entry = state.manager.entry(&connection_id)?;
    let conn = state
        .manager
        .open(&connection_id, &database, Purpose::Grid)
        .await?;
    let (query, result) = execute(&state, &grid_id, &conn, &request).await?;
    let grid = Grid {
        conn,
        server: entry.server_key(),
        database,
        request,
        query,
        result,
    };
    let v = view(&grid);
    if let Some(old) = state
        .grids
        .grids
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(grid_id.clone(), Arc::new(tokio::sync::Mutex::new(grid)))
    {
        drop(old);
    }
    Ok(v)
}

/// Re-run with new criteria/sort/N (server-side). Pending edits must be discarded first —
/// the UI asks; this refuses rather than silently dropping them.
#[tauri::command]
pub async fn grid_requery(
    state: State<'_, AppState>,
    grid_id: String,
    criteria: Vec<Criterion>,
    sort: Vec<UiSort>,
    n: Option<u64>,
) -> AppResult<GridView> {
    let grid = state.grids.get(&grid_id)?;
    let mut g = grid.lock().await;
    if g.result.has_pending() {
        return Err(AppError::new(
            ErrorKind::Refused,
            "discard or commit the pending changes before re-running",
        ));
    }
    let mut request = g.request.clone();
    request.criteria = criteria;
    request.sort = sort_keys(sort);
    if let Some(n) = n {
        request.limit = n;
    }
    let old = g.result.id().to_string();
    let (query, result) = execute(&state, &grid_id, &g.conn, &request).await?;
    state.release_result(&old);
    g.request = request;
    g.query = query;
    g.result = result;
    Ok(view(&g))
}

#[tauri::command]
pub async fn grid_edit_cell(
    state: State<'_, AppState>,
    grid_id: String,
    row: usize,
    col: usize,
    value: Option<String>,
) -> AppResult<PendingInfo> {
    let grid = state.grids.get(&grid_id)?;
    let g = grid.lock().await;
    g.result.set_cell(row, col, value)?;
    Ok(pending(&g))
}

#[tauri::command]
pub async fn grid_insert_row(
    state: State<'_, AppState>,
    grid_id: String,
) -> AppResult<PendingInfo> {
    let grid = state.grids.get(&grid_id)?;
    let g = grid.lock().await;
    g.result.insert_row()?;
    Ok(pending(&g))
}

#[tauri::command]
pub async fn grid_delete_rows(
    state: State<'_, AppState>,
    grid_id: String,
    rows: Vec<usize>,
    undelete: bool,
) -> AppResult<PendingInfo> {
    let grid = state.grids.get(&grid_id)?;
    let g = grid.lock().await;
    // delete from the bottom so inserted-row indexes stay valid
    let mut rows = rows;
    rows.sort_unstable_by(|a, b| b.cmp(a));
    for r in rows {
        if undelete {
            g.result.undelete_row(r)?;
        } else {
            g.result.delete_row(r)?;
        }
    }
    Ok(pending(&g))
}

fn commit_options(state: &AppState) -> CommitOptions {
    CommitOptions {
        lock_timeout_ms: state
            .settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .grid_lock_timeout_ms,
        ..CommitOptions::default()
    }
}

#[tauri::command]
pub async fn grid_preview(state: State<'_, AppState>, grid_id: String) -> AppResult<Vec<String>> {
    let grid = state.grids.get(&grid_id)?;
    let g = grid.lock().await;
    let target = g
        .result
        .edit_target()
        .ok_or_else(|| AppError::new(ErrorKind::Refused, "this grid is read-only"))?;
    Ok(plan_edits(&g.result, &target, &commit_options(&state))?.preview())
}

/// Commit pending edits as one transaction. Holds the manager's edit guard so a Refresh
/// swap can't start mid-commit (and refuses while one is quiesced).
#[tauri::command]
pub async fn grid_commit(state: State<'_, AppState>, grid_id: String) -> AppResult<CommitReport> {
    let grid = state.grids.get(&grid_id)?;
    let g = grid.lock().await;
    let target = g
        .result
        .edit_target()
        .ok_or_else(|| AppError::new(ErrorKind::Refused, "this grid is read-only"))?;
    let _guard = state.manager.edit_guard(&g.server, &g.database).await?;
    if g.conn.is_closed() {
        return Err(AppError::new(
            ErrorKind::Disconnected,
            "the grid's connection was closed; your pending edits are kept",
        )
        .with_hint("reopen the table to commit them"));
    }
    commit_edits(&g.conn.client, &g.result, &target, &commit_options(&state)).await
}

#[tauri::command]
pub async fn grid_discard(state: State<'_, AppState>, grid_id: String) -> AppResult<PendingInfo> {
    let grid = state.grids.get(&grid_id)?;
    let g = grid.lock().await;
    g.result.discard_pending()?;
    Ok(pending(&g))
}

#[tauri::command]
pub fn grid_close(state: State<'_, AppState>, grid_id: String) {
    state
        .grids
        .grids
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&grid_id);
    state.release_results_of(&grid_id);
}
