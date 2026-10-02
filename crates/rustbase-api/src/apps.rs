//! Endpoints for managing apps under a workspace.
//!
//! - `GET    /api/workspaces/:workspace/apps`         list apps in a workspace
//! - `POST   /api/workspaces/:workspace/apps`         create an app + init its data.db
//! - `GET    /api/workspaces/:workspace/apps/:app`    fetch one
//! - `PATCH  /api/workspaces/:workspace/apps/:app`    rename
//! - `DELETE /api/workspaces/:workspace/apps/:app`    cascade-delete the app
//!
//! All five accept either a master admin or a workspace admin for the
//! target workspace. App admins (single-app scope) are deliberately
//! excluded — they manage their own app's data, not the app's identity.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use rustbase_core::{AppId, CoreError, WorkspaceId};
use rustbase_db::{
    App,
    apps::{create_app, delete_app, find_app, list_apps, rename_app},
    docs::{
        SiteCatalogInput, VersionCatalogInput, app_has_site_release_reference,
        ensure_version_manifest, find_site_catalog, list_version_catalog, rename_version_catalog,
        set_site_default_app, site_catalog_config, sync_version_manifest_name, upsert_site_catalog,
        upsert_version_catalog,
    },
    paths,
    workspaces::find_workspace,
};
use serde::Deserialize;
use std::sync::Arc;
use validator::Validate;

use crate::auth::AdminAuth;
use crate::docs::{ensure_app_docs, ensure_workspace_docs};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize, Validate)]
pub struct CreateAppRequest {
    pub id: String,
    #[validate(length(min = 1, max = 100))]
    pub name: String,
}

#[derive(Debug, Deserialize, Validate)]
pub struct UpdateAppRequest {
    #[validate(length(min = 1, max = 100))]
    pub name: String,
}

pub async fn list(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(workspace): Path<String>,
) -> Result<Json<Vec<App>>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    require_realm_exists(&state, &workspace).await?;
    let pool = ensure_workspace_docs(&state, &WorkspaceId::from(workspace)).await?;
    Ok(Json(list_apps(&pool).await?))
}

pub async fn create(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(workspace): Path<String>,
    Json(req): Json<CreateAppRequest>,
) -> Result<(StatusCode, Json<App>), ApiError> {
    auth.require_workspace_access(&workspace)?;
    validate_app_id(&req.id)?;
    req.validate()
        .map_err(|e| ApiError::Core(CoreError::Validation(e.to_string())))?;

    require_realm_exists(&state, &workspace).await?;
    let workspace_id = WorkspaceId::from(workspace.clone());
    let workspace_pool = ensure_workspace_docs(&state, &workspace_id).await?;

    if find_app(&workspace_pool, &req.id).await?.is_some() {
        return Err(ApiError::Core(CoreError::Conflict(format!(
            "app '{}' already exists in workspace '{}'",
            req.id, workspace
        ))));
    }

    let app = create_app(&workspace_pool, &req.id, &req.name).await?;

    // Initialize the app's data.db.
    let app_id = AppId::from(req.id.clone());
    let app_pool = ensure_app_docs(&state, &workspace_id, &app_id).await?;

    // The Workspace catalog is canonical for site administration, but every
    // App receives an effective manifest before it can be published or served
    // standalone. Existing installations are repaired here as well as at
    // server boot.
    let workspace_row = find_workspace(state.system.pool(), &workspace)
        .await?
        .ok_or(ApiError::Core(CoreError::WorkspaceNotFound(
            workspace.clone(),
        )))?;
    let site = match find_site_catalog(&workspace_pool).await? {
        Some(site) => site,
        None => {
            upsert_site_catalog(
                &workspace_pool,
                SiteCatalogInput {
                    name: workspace_row.name.clone(),
                    slug: workspace.clone(),
                    description: None,
                    default_app_id: None,
                    default_language_code: "en".into(),
                    access_mode: "public".into(),
                    canonical_url: None,
                    theme: None,
                },
            )
            .await?
        }
    };
    let existing_versions = list_version_catalog(&workspace_pool).await?;
    let is_default = existing_versions.is_empty();
    upsert_version_catalog(
        &workspace_pool,
        VersionCatalogInput {
            app_id: req.id.clone(),
            slug: req.id.clone(),
            name: req.name.clone(),
            is_default,
            enabled: true,
            position: existing_versions.len() as f64,
            source_branch_id: None,
            config: None,
        },
    )
    .await?;
    if is_default {
        set_site_default_app(&workspace_pool, &req.id).await?;
    }
    let site_config = site_catalog_config(&site).await?;
    let site_slug = site
        .fields
        .get("slug")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&workspace);
    let site_name = site
        .fields
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&workspace_row.name);
    ensure_version_manifest(
        &app_pool,
        site_slug,
        site_name,
        &req.id,
        &req.name,
        &site_config,
    )
    .await?;

    // Pick up any JS hooks dropped on disk before the app was created.
    let hooks_dir = state.data_dir.join("hooks").join(&workspace).join(&req.id);
    let bridge = crate::hook_bridge::ApiBridge::new(
        WorkspaceId::from(workspace.clone()),
        AppId::from(req.id.clone()),
        state.apps.clone(),
    )
    .into_sync();
    // Wrap the server-wide mailer in a per-(workspace, app) quota gate so a
    // runaway $app.mailer.send loop can't flood the relay. System-issued
    // mail (verify-email + password-reset endpoints) keeps using the
    // bare state.mailer and is intentionally not quota'd.
    let quoted = Arc::new(crate::mailer::QuotedMailer::new(
        state.mailer.clone(),
        WorkspaceId::from(workspace.clone()),
        AppId::from(req.id.clone()),
        state.apps.clone(),
    )) as Arc<dyn rustbase_core::Mailer>;
    let audit = crate::hook_bridge::ApiAuditBridge::new(
        WorkspaceId::from(workspace.clone()),
        AppId::from(req.id.clone()),
        state.apps.clone(),
    );
    let fetcher = crate::hook_bridge::ApiFetchBridge::new(state.hook_fetch_allowed_hosts.clone());
    if let Err(e) = state
        .hooks
        .load_app_with(
            &workspace,
            &req.id,
            &hooks_dir,
            rustbase_runtime::AppHooksConfig {
                records: Some(bridge),
                mailer: Some(quoted),
                audit: Some(audit),
                fetcher: Some(fetcher),
                limits: rustbase_runtime::SandboxLimits::default(),
            },
        )
        .await
    {
        tracing::warn!(workspace = %workspace, app = %req.id, error = %e, "loading hooks failed");
    }

    tracing::info!(workspace = %workspace, app = %req.id, "app created");
    Ok((StatusCode::CREATED, Json(app)))
}

pub async fn get(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
) -> Result<Json<App>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    require_realm_exists(&state, &workspace).await?;
    let pool = ensure_workspace_docs(&state, &WorkspaceId::from(workspace.clone())).await?;
    let row = find_app(&pool, &app)
        .await?
        .ok_or(ApiError::Core(CoreError::AppNotFound { workspace, app }))?;
    Ok(Json(row))
}

pub async fn update(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(req): Json<UpdateAppRequest>,
) -> Result<Json<App>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    req.validate()
        .map_err(|e| ApiError::Core(CoreError::Validation(e.to_string())))?;
    require_realm_exists(&state, &workspace).await?;

    let pool = ensure_workspace_docs(&state, &WorkspaceId::from(workspace.clone())).await?;
    rename_app(&pool, &app, &req.name)
        .await
        .map_err(|e| match e {
            rustbase_db::DbError::Sqlx(sqlx::Error::RowNotFound) => {
                ApiError::Core(CoreError::AppNotFound {
                    workspace: workspace.clone(),
                    app: app.clone(),
                })
            }
            other => ApiError::from(other),
        })?;

    rename_version_catalog(&pool, &app, &req.name).await?;
    let app_pool = ensure_app_docs(
        &state,
        &WorkspaceId::from(workspace.clone()),
        &AppId::from(app.clone()),
    )
    .await?;
    sync_version_manifest_name(&app_pool, &req.name).await?;
    let row = find_app(&pool, &app)
        .await?
        .ok_or(ApiError::Core(CoreError::AppNotFound { workspace, app }))?;
    Ok(Json(row))
}

pub async fn delete(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.require_workspace_access(&workspace)?;
    require_realm_exists(&state, &workspace).await?;

    let workspace_id = WorkspaceId::from(workspace.clone());
    let app_id = AppId::from(app.clone());
    let workspace_pool = ensure_workspace_docs(&state, &workspace_id).await?;

    find_app(&workspace_pool, &app).await?.ok_or_else(|| {
        ApiError::Core(CoreError::AppNotFound {
            workspace: workspace.clone(),
            app: app.clone(),
        })
    })?;
    if app_has_site_release_reference(&workspace_pool, &app).await? {
        return Err(ApiError::Core(CoreError::Conflict(
            "cannot delete an App referenced by a site release".into(),
        )));
    }

    // Remove binary objects before deleting the catalog row. This keeps a
    // failed backend cleanup retryable: the App remains addressable until
    // its storage prefix has been enumerated and deleted successfully.
    let storage_prefix = format!("workspaces/{workspace}/apps/{app}/storage/");
    let removed_objects = state
        .storage
        .delete_prefix(&storage_prefix)
        .await
        .map_err(|e| {
            ApiError::Core(CoreError::Internal(format!(
                "failed to remove app storage: {e}"
            )))
        })?;
    tracing::debug!(
        workspace = %workspace,
        app = %app,
        removed_objects,
        "app storage objects removed"
    );

    state.apps.evict(&workspace_id, &app_id);
    delete_app(&workspace_pool, &app).await?;

    let dir = paths::app_dir(state.data_dir.as_ref(), &workspace_id, &app_id);
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await.map_err(|e| {
            ApiError::Core(CoreError::Internal(format!(
                "failed to remove app folder: {e}"
            )))
        })?;
    }

    tracing::info!(workspace = %workspace, app = %app, "app deleted");
    Ok(StatusCode::NO_CONTENT)
}

async fn require_realm_exists(state: &AppState, workspace: &str) -> Result<(), ApiError> {
    find_workspace(state.system.pool(), workspace)
        .await?
        .ok_or(ApiError::Core(CoreError::WorkspaceNotFound(
            workspace.to_string(),
        )))?;
    Ok(())
}

/// App ids share the workspace-id slug rules.
fn validate_app_id(id: &str) -> Result<(), ApiError> {
    let len = id.len();
    if !(2..=50).contains(&len) {
        return Err(ApiError::Core(CoreError::Validation(
            "app id must be 2-50 characters".into(),
        )));
    }
    if id.starts_with('-') || id.ends_with('-') {
        return Err(ApiError::Core(CoreError::Validation(
            "app id must not start or end with '-'".into(),
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(ApiError::Core(CoreError::Validation(
            "app id may only contain lowercase letters, digits, and '-'".into(),
        )));
    }
    Ok(())
}
