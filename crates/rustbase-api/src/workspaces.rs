//! Master-admin endpoints for managing workspaces.
//!
//! - `GET    /api/workspaces`        — list every workspace
//! - `POST   /api/workspaces`        — create a new workspace (id + display name)
//! - `GET    /api/workspaces/:id`    — fetch one
//! - `PATCH  /api/workspaces/:id`    — rename
//! - `DELETE /api/workspaces/:id`    — cascade-delete (refuses master)
//!
//! All five require a master-admin token. Creation also initializes
//! the workspace's `workspace.db` by opening the pool and running
//! `WORKSPACE_MIGRATIONS`. Deletion evicts the workspace + every app pool under
//! it, deletes the row, and removes the workspace's folder.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use rustbase_core::{AppId, CoreError, MASTER_WORKSPACE_ID, WorkspaceId};
use rustbase_db::{
    Workspace,
    apps::list_apps,
    docs::{
        SiteCatalogInput, rename_site_catalog, sync_version_manifest_site_identity,
        upsert_site_catalog,
    },
    paths,
    workspaces::{create_realm, delete_realm, find_workspace, list_realms, rename_realm},
};
use serde::Deserialize;
use validator::Validate;

use crate::auth::AdminAuth;
use crate::docs::{ensure_app_docs, ensure_workspace_docs};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize, Validate)]
pub struct CreateRealmRequest {
    pub id: String,
    #[validate(length(min = 1, max = 100))]
    pub name: String,
}

#[derive(Debug, Deserialize, Validate)]
pub struct UpdateRealmRequest {
    #[validate(length(min = 1, max = 100))]
    pub name: String,
}

pub async fn list(
    auth: AdminAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<Workspace>>, ApiError> {
    auth.require_master()?;
    Ok(Json(list_realms(state.system.pool()).await?))
}

pub async fn create(
    auth: AdminAuth,
    State(state): State<AppState>,
    Json(req): Json<CreateRealmRequest>,
) -> Result<(StatusCode, Json<Workspace>), ApiError> {
    auth.require_master()?;
    validate_realm_id(&req.id)?;
    req.validate()
        .map_err(|e| ApiError::Core(CoreError::Validation(e.to_string())))?;

    if req.id == MASTER_WORKSPACE_ID {
        return Err(ApiError::Core(CoreError::Conflict(
            "workspace id 'master' is reserved".into(),
        )));
    }
    if find_workspace(state.system.pool(), &req.id)
        .await?
        .is_some()
    {
        return Err(ApiError::Core(CoreError::Conflict(format!(
            "workspace '{}' already exists",
            req.id
        ))));
    }

    let workspace = create_realm(state.system.pool(), &req.id, &req.name).await?;

    let workspace_id = WorkspaceId::from(req.id.clone());
    let workspace_pool = ensure_workspace_docs(&state, &workspace_id).await?;
    upsert_site_catalog(
        &workspace_pool,
        SiteCatalogInput {
            name: req.name.clone(),
            slug: req.id.clone(),
            description: None,
            default_app_id: None,
            default_language_code: "en".into(),
            access_mode: "public".into(),
            canonical_url: None,
            theme: None,
        },
    )
    .await?;

    tracing::info!(workspace = %req.id, "created workspace");
    Ok((StatusCode::CREATED, Json(workspace)))
}

pub async fn get(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Workspace>, ApiError> {
    auth.require_master()?;
    let workspace = find_workspace(state.system.pool(), &id)
        .await?
        .ok_or(ApiError::Core(CoreError::WorkspaceNotFound(id)))?;
    Ok(Json(workspace))
}

pub async fn update(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRealmRequest>,
) -> Result<Json<Workspace>, ApiError> {
    auth.require_master()?;
    req.validate()
        .map_err(|e| ApiError::Core(CoreError::Validation(e.to_string())))?;

    rename_realm(state.system.pool(), &id, &req.name)
        .await
        .map_err(|e| match e {
            rustbase_db::DbError::Sqlx(sqlx::Error::RowNotFound) => {
                ApiError::Core(CoreError::WorkspaceNotFound(id.clone()))
            }
            other => ApiError::from(other),
        })?;

    let workspace = find_workspace(state.system.pool(), &id)
        .await?
        .ok_or(ApiError::Core(CoreError::WorkspaceNotFound(id.clone())))?;
    let workspace_id = WorkspaceId::from(id.clone());
    let workspace_pool = ensure_workspace_docs(&state, &workspace_id).await?;
    let site = rename_site_catalog(&workspace_pool, &req.name).await?;
    let site_slug = site
        .as_ref()
        .and_then(|record| record.fields.get("slug"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&id);
    let site_name = site
        .as_ref()
        .and_then(|record| record.fields.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&req.name);
    for app in list_apps(&workspace_pool).await? {
        let app_pool = ensure_app_docs(&state, &workspace_id, &AppId::from(app.id.clone())).await?;
        sync_version_manifest_site_identity(&app_pool, site_slug, site_name).await?;
    }
    Ok(Json(workspace))
}

pub async fn delete(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require_master()?;
    if id == MASTER_WORKSPACE_ID {
        return Err(ApiError::Core(CoreError::Forbidden));
    }

    let workspace = find_workspace(state.system.pool(), &id)
        .await?
        .ok_or(ApiError::Core(CoreError::WorkspaceNotFound(id.clone())))?;
    if workspace.is_master {
        return Err(ApiError::Core(CoreError::Forbidden));
    }

    let workspace_id = WorkspaceId::from(id.clone());
    // File bytes live in the global Storage backend, not in the database
    // transaction. Enumerate each App's storage prefix before removing the
    // Workspace row so a transient S3 failure leaves the Workspace retryable.
    let workspace_db = paths::workspace_db(state.data_dir.as_ref(), &workspace_id);
    if workspace_db.is_file() {
        let workspace_pool = ensure_workspace_docs(&state, &workspace_id).await?;
        for app in list_apps(&workspace_pool).await? {
            let prefix = format!("workspaces/{id}/apps/{}/storage/", app.id);
            let removed_objects = state.storage.delete_prefix(&prefix).await.map_err(|e| {
                ApiError::Core(CoreError::Internal(format!(
                    "failed to remove workspace storage: {e}"
                )))
            })?;
            tracing::debug!(
                workspace = %id,
                app = %app.id,
                removed_objects,
                "workspace app storage objects removed"
            );
        }
    }

    state.workspaces.evict(&workspace_id);
    state.apps.evict_realm(&workspace_id);

    delete_realm(state.system.pool(), &id).await?;

    let dir = paths::workspace_dir(state.data_dir.as_ref(), &workspace_id);
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await.map_err(|e| {
            ApiError::Core(CoreError::Internal(format!(
                "failed to remove workspace folder: {e}"
            )))
        })?;
    }

    tracing::info!(workspace = %id, "deleted workspace");
    Ok(StatusCode::NO_CONTENT)
}

/// Workspace ids are slugs: 2–50 chars, `[a-z0-9-]`, no leading/trailing dash.
fn validate_realm_id(id: &str) -> Result<(), ApiError> {
    let len = id.len();
    if !(2..=50).contains(&len) {
        return Err(ApiError::Core(CoreError::Validation(
            "workspace id must be 2-50 characters".into(),
        )));
    }
    if id.starts_with('-') || id.ends_with('-') {
        return Err(ApiError::Core(CoreError::Validation(
            "workspace id must not start or end with '-'".into(),
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(ApiError::Core(CoreError::Validation(
            "workspace id may only contain lowercase letters, digits, and '-'".into(),
        )));
    }
    Ok(())
}
