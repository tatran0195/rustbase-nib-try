//! Documentation page endpoints backed by RustBase's reserved dynamic
//! collections.
//!
//! These handlers intentionally do not use the generic collection/record
//! routes. The latter are protected for reserved docs collections so page
//! tree invariants remain inside `rustbase_db::docs`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use rustbase_core::{AppId, CoreError, FilterNode, Record, Schema, WorkspaceId};
use rustbase_db::{
    APP_MIGRATIONS, DbError, ListPage, WORKSPACE_MIGRATIONS, apply_migrations,
    apps::find_app,
    collections::find_collection,
    docs::{
        PageDraftInput, PagePatch, bootstrap_app_collections, bootstrap_site_version_metadata,
        create_page as db_create_page, delete_page as db_delete_page, touch_version_manifest,
        update_page as db_update_page,
    },
    records::{find_record, list_records_ordered},
    workspaces::find_workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::auth::AdminAuth;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct PageListQuery {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_per_page")]
    pub per_page: u32,
    pub language_code: Option<String>,
}

fn default_page() -> u32 {
    1
}

fn default_per_page() -> u32 {
    100
}

#[derive(Debug, Serialize)]
pub struct PageListResponse {
    pub items: Vec<Record>,
    pub page: u32,
    pub per_page: u32,
    pub total_items: u64,
    pub total_pages: u64,
}

pub async fn list_pages(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Query(query): Query<PageListQuery>,
) -> Result<Json<PageListResponse>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let app_pool = open_docs_app(&state, &workspace, &app).await?;
    let schema = docs_pages_schema(&app_pool).await?;
    let filter = query
        .language_code
        .map(|language| FilterNode::Eq("language_code".into(), JsonValue::String(language)));
    let listed = list_records_ordered(
        &app_pool,
        &schema,
        ListPage {
            page: query.page,
            per_page: query.per_page,
        },
        filter.as_ref(),
        "position",
        false,
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(PageListResponse {
        items: listed.items,
        page: listed.page,
        per_page: listed.per_page,
        total_items: listed.total_items,
        total_pages: listed.total_items.div_ceil(listed.per_page.max(1) as u64),
    }))
}

pub async fn create_page(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(input): Json<PageDraftInput>,
) -> Result<(StatusCode, Json<Record>), ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let app_pool = open_docs_app(&state, &workspace, &app).await?;
    let page = db_create_page(&app_pool, input)
        .await
        .map_err(map_docs_error)?;
    touch_version_manifest(&app_pool)
        .await
        .map_err(map_docs_error)?;
    Ok((StatusCode::CREATED, Json(page)))
}

pub async fn get_page(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app, id)): Path<(String, String, String)>,
) -> Result<Json<Record>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let app_pool = open_docs_app(&state, &workspace, &app).await?;
    let schema = docs_pages_schema(&app_pool).await?;
    let page = find_record(&app_pool, &schema, &id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::Core(CoreError::NotFound {
            collection: "docs_pages".into(),
            id,
        }))?;
    Ok(Json(page))
}

pub async fn update_page(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app, id)): Path<(String, String, String)>,
    Json(patch): Json<PagePatch>,
) -> Result<Json<Record>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let app_pool = open_docs_app(&state, &workspace, &app).await?;
    let page = db_update_page(&app_pool, &id, patch)
        .await
        .map_err(map_docs_error)?;
    touch_version_manifest(&app_pool)
        .await
        .map_err(map_docs_error)?;
    Ok(Json(page))
}

pub async fn delete_page(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app, id)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let app_pool = open_docs_app(&state, &workspace, &app).await?;
    db_delete_page(&app_pool, &id)
        .await
        .map_err(map_docs_error)?;
    touch_version_manifest(&app_pool)
        .await
        .map_err(map_docs_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Ensure a Workspace database is migrated and its reserved site collections
/// exist. All API paths that open a Workspace database use this helper so
/// request-time repair and startup repair share one initialization contract.
pub async fn ensure_workspace_docs(
    state: &AppState,
    workspace_id: &WorkspaceId,
) -> Result<sqlx::SqlitePool, ApiError> {
    let pool = state.workspaces.pool_for(workspace_id).await?;
    apply_migrations(pool.clone(), WORKSPACE_MIGRATIONS).await?;
    rustbase_db::docs::bootstrap_workspace_collections(&pool)
        .await
        .map_err(map_docs_error)?;
    Ok(pool)
}

/// Ensure an App database is migrated and its reserved version collections
/// exist. The App remains the serving/deployment boundary; this helper does
/// not open or join a Workspace database.
pub async fn ensure_app_docs(
    state: &AppState,
    workspace_id: &WorkspaceId,
    app_id: &AppId,
) -> Result<sqlx::SqlitePool, ApiError> {
    let pool = state.apps.pool_for(workspace_id, app_id).await?;
    apply_migrations(pool.clone(), APP_MIGRATIONS).await?;
    bootstrap_app_collections(&pool).await?;
    Ok(pool)
}

pub(crate) async fn open_docs_workspace(
    state: &AppState,
    workspace: &str,
) -> Result<sqlx::SqlitePool, ApiError> {
    find_workspace(state.system.pool(), workspace)
        .await?
        .ok_or(ApiError::Core(CoreError::WorkspaceNotFound(
            workspace.to_string(),
        )))?;
    ensure_workspace_docs(state, &WorkspaceId::from(workspace.to_string())).await
}

pub(crate) async fn open_docs_app(
    state: &AppState,
    workspace: &str,
    app: &str,
) -> Result<sqlx::SqlitePool, ApiError> {
    let workspace_row = find_workspace(state.system.pool(), workspace)
        .await?
        .ok_or(ApiError::Core(CoreError::WorkspaceNotFound(
            workspace.to_string(),
        )))?;
    let workspace_id = WorkspaceId::from(workspace.to_string());
    let workspace_pool = ensure_workspace_docs(state, &workspace_id).await?;
    let app_row = find_app(&workspace_pool, app).await?.ok_or_else(|| {
        ApiError::Core(CoreError::AppNotFound {
            workspace: workspace.to_string(),
            app: app.to_string(),
        })
    })?;
    let app_id = AppId::from(app.to_string());
    let app_pool = ensure_app_docs(state, &workspace_id, &app_id).await?;
    bootstrap_site_version_metadata(
        &workspace_pool,
        &app_pool,
        workspace,
        &workspace_row.name,
        app,
        &app_row.name,
    )
    .await
    .map_err(map_docs_error)?;
    Ok(app_pool)
}

async fn docs_pages_schema(pool: &sqlx::SqlitePool) -> Result<Schema, ApiError> {
    find_collection(pool, "docs_pages")
        .await?
        .map(|collection| collection.schema)
        .ok_or(ApiError::Core(CoreError::Internal(
            "docs_pages collection is not bootstrapped".into(),
        )))
}

pub(crate) fn map_docs_error(error: DbError) -> ApiError {
    match error {
        DbError::Sqlx(sqlx::Error::RowNotFound) => ApiError::Core(CoreError::NotFound {
            collection: "docs_pages".into(),
            id: String::new(),
        }),
        DbError::InvalidIdentifier(message) => ApiError::Core(CoreError::Validation(message)),
        DbError::Invariant(message) => ApiError::Core(CoreError::Conflict(message)),
        DbError::Conflict(message) => ApiError::Core(CoreError::Conflict(message)),
        DbError::Sqlx(sqlx::Error::Database(_)) => ApiError::Core(CoreError::Conflict(
            "documentation page constraint violated".into(),
        )),
        other => ApiError::from(other),
    }
}
