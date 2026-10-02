//! Admin/editor endpoints for the typed documentation domain.
//!
//! These routes deliberately sit beside the generic collection API. They
//! coordinate Workspace catalog records, App-local manifests, immutable
//! publish snapshots, reader grants/sessions, and site releases. Generic
//! CRUD cannot perform these operations safely.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use rand_core::{OsRng, RngCore};
use rustbase_core::{AppId, CoreError, WorkspaceId};
use rustbase_db::{
    apps::{find_app, list_apps},
    docs::{
        LanguageInput, ReaderGrant, ReadyDeploymentRef, SiteCatalogInput, SiteReleaseInput,
        VersionCatalogInput, VersionManifest, VersionSnapshot, commit_ready, create_reader_grant,
        create_reader_session, create_rollback_ready, create_site_release, ensure_version_manifest,
        find_active_site_release, find_deployment, find_site_catalog, find_version_manifest,
        finish_publish_job, list_deployments, list_languages, list_version_catalog,
        mark_deployment_building, mark_deployment_failed, prepare_publish,
        refresh_version_manifest_languages, set_site_default_app, site_catalog_config,
        sync_version_manifest_identity, sync_version_manifest_site_config, touch_version_manifest,
        upsert_language, upsert_site_catalog, upsert_version_catalog,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::{
    auth::AdminAuth,
    docs::{ensure_app_docs, map_docs_error, open_docs_app, open_docs_workspace},
    error::ApiError,
    state::AppState,
};

#[derive(Debug, Deserialize)]
pub struct VersionManifestRequest {
    #[serde(flatten)]
    pub manifest: VersionManifest,
}

pub async fn get_site(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(workspace): Path<String>,
) -> Result<Json<rustbase_core::Record>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    let pool = open_workspace(&state, &workspace).await?;
    let site = find_site_catalog(&pool)
        .await
        .map_err(map_docs_error)?
        .ok_or(ApiError::Core(CoreError::NotFound {
            collection: "docs_site".into(),
            id: workspace,
        }))?;
    Ok(Json(site))
}

pub async fn put_site(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(workspace): Path<String>,
    Json(input): Json<SiteCatalogInput>,
) -> Result<Json<rustbase_core::Record>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    let pool = open_workspace(&state, &workspace).await?;
    let site_slug = input.slug.clone();
    let site_name = input.name.clone();
    let site = upsert_site_catalog(&pool, input)
        .await
        .map_err(map_docs_error)?;
    let site_config = site_catalog_config(&site).await.map_err(map_docs_error)?;
    let workspace_id = WorkspaceId::from(workspace.clone());
    for app in list_apps(&pool).await? {
        let app_pool = ensure_app_docs(&state, &workspace_id, &AppId::from(app.id.clone())).await?;
        sync_version_manifest_site_config(&app_pool, &site_slug, &site_name, &site_config)
            .await
            .map_err(map_docs_error)?;
    }
    Ok(Json(site))
}

pub async fn list_versions(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(workspace): Path<String>,
) -> Result<Json<Vec<rustbase_core::Record>>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    let pool = open_workspace(&state, &workspace).await?;
    Ok(Json(
        list_version_catalog(&pool).await.map_err(map_docs_error)?,
    ))
}

pub async fn put_version(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(mut input): Json<VersionCatalogInput>,
) -> Result<Json<rustbase_core::Record>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    if input.app_id != app {
        return Err(ApiError::Core(CoreError::Validation(
            "version app_id must match the URL".into(),
        )));
    }
    let workspace_pool = open_workspace(&state, &workspace).await?;
    let app_pool = open_docs_app(&state, &workspace, &app).await?;
    let site = find_site_catalog(&workspace_pool)
        .await
        .map_err(map_docs_error)?
        .ok_or(ApiError::Core(CoreError::Internal(
            "site catalog is not initialized".into(),
        )))?;
    let site_config = site_catalog_config(&site).await.map_err(map_docs_error)?;
    let site_slug = site
        .fields
        .get("slug")
        .and_then(JsonValue::as_str)
        .unwrap_or(&workspace);
    let site_name = site
        .fields
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or(&workspace);
    if site
        .fields
        .get("default_app_id")
        .and_then(JsonValue::as_str)
        == Some(app.as_str())
    {
        input.is_default = true;
    }
    let row = upsert_version_catalog(&workspace_pool, input.clone())
        .await
        .map_err(map_docs_error)?;
    if input.is_default {
        set_site_default_app(&workspace_pool, &app)
            .await
            .map_err(map_docs_error)?;
    }
    ensure_version_manifest(
        &app_pool,
        site_slug,
        site_name,
        &input.slug,
        &input.name,
        &site_config,
    )
    .await
    .map_err(map_docs_error)?;
    sync_version_manifest_identity(&app_pool, &input.slug, &input.name)
        .await
        .map_err(map_docs_error)?;
    Ok(Json(row))
}

pub async fn get_manifest(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
) -> Result<Json<VersionManifest>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    let (_, manifest) = find_version_manifest(&pool).await.map_err(map_docs_error)?;
    Ok(Json(manifest))
}

pub async fn put_manifest(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(request): Json<VersionManifestRequest>,
) -> Result<Json<VersionManifest>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    let workspace_pool = open_workspace(&state, &workspace).await?;
    let site = find_site_catalog(&workspace_pool)
        .await
        .map_err(map_docs_error)?
        .ok_or(ApiError::Core(CoreError::Internal(
            "site catalog is not initialized".into(),
        )))?;
    let expected_site_slug = site
        .fields
        .get("slug")
        .and_then(JsonValue::as_str)
        .unwrap_or(&workspace);
    if request.manifest.site_slug != expected_site_slug {
        return Err(ApiError::Core(CoreError::Validation(
            "manifest site_slug must match the site catalog".into(),
        )));
    }
    if let Some(version) = list_version_catalog(&workspace_pool)
        .await
        .map_err(map_docs_error)?
        .into_iter()
        .find(|record| {
            record.fields.get("app_id").and_then(JsonValue::as_str) == Some(app.as_str())
        })
        && version.fields.get("slug").and_then(JsonValue::as_str)
            != Some(request.manifest.version_slug.as_str())
    {
        return Err(ApiError::Core(CoreError::Validation(
            "manifest version_slug must match the version catalog".into(),
        )));
    }
    let record = rustbase_db::docs::upsert_version_manifest(&pool, request.manifest.clone())
        .await
        .map_err(map_docs_error)?;
    let (_, manifest) = find_version_manifest(&pool).await.map_err(map_docs_error)?;
    // Keep the response derived from storage, not from an unchecked request.
    let _ = record;
    Ok(Json(manifest))
}

pub async fn list_app_languages(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
) -> Result<Json<Vec<rustbase_core::Record>>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    Ok(Json(list_languages(&pool).await.map_err(map_docs_error)?))
}

pub async fn put_app_language(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(input): Json<LanguageInput>,
) -> Result<Json<rustbase_core::Record>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    let row = upsert_language(&pool, input)
        .await
        .map_err(map_docs_error)?;
    // Language rows are draft/effective configuration, so refresh the
    // App-local serving manifest explicitly rather than making readers join
    // the mutable language catalog at request time.
    refresh_version_manifest_languages(&pool)
        .await
        .map_err(map_docs_error)?;
    Ok(Json(row))
}

#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    pub idempotency_key: String,
}

#[derive(Debug, Serialize)]
pub struct PublishResponse {
    pub deployment: rustbase_core::Record,
    pub checksum: String,
    pub snapshot_key: Option<String>,
    pub reused: bool,
    pub in_progress: bool,
}

pub async fn publish(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(request): Json<PublishRequest>,
) -> Result<(StatusCode, Json<PublishResponse>), ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    let preparation = prepare_publish(&pool, Some(&auth.admin_id), &request.idempotency_key)
        .await
        .map_err(map_docs_error)?;
    if preparation.in_progress {
        let checksum = preparation
            .deployment
            .fields
            .get("checksum")
            .and_then(JsonValue::as_str)
            .unwrap_or(&preparation.checksum)
            .to_string();
        let snapshot_key = preparation
            .deployment
            .fields
            .get("snapshot_key")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        return Ok((
            StatusCode::ACCEPTED,
            Json(PublishResponse {
                deployment: preparation.deployment,
                checksum,
                snapshot_key,
                reused: false,
                in_progress: true,
            }),
        ));
    }
    if preparation.reused {
        let checksum = preparation
            .deployment
            .fields
            .get("checksum")
            .and_then(JsonValue::as_str)
            .unwrap_or(&preparation.checksum)
            .to_string();
        let snapshot_key = preparation
            .deployment
            .fields
            .get("snapshot_key")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        return Ok((
            StatusCode::OK,
            Json(PublishResponse {
                deployment: preparation.deployment,
                checksum,
                snapshot_key,
                reused: true,
                in_progress: false,
            }),
        ));
    }

    let deployment_id = preparation.deployment.id.to_string();
    let job_id = preparation.job.id.to_string();
    mark_deployment_building(&pool, &deployment_id)
        .await
        .map_err(map_docs_error)?;
    let snapshot_key = format!(
        "docs/{workspace}/{app}/deployments/{deployment_id}/{}.snapshot.json",
        preparation.checksum
    );
    if let Err(error) = state
        .storage
        .put(&snapshot_key, preparation.snapshot_bytes.clone())
        .await
    {
        let message = format!("snapshot storage failed: {error}");
        let _ = state.storage.delete(&snapshot_key).await;
        let _ = mark_deployment_failed(&pool, &deployment_id, &message).await;
        let _ = finish_publish_job(&pool, &job_id, false, Some(&message)).await;
        return Err(ApiError::Core(CoreError::Internal(message)));
    }
    let expected_revision = preparation.snapshot.manifest.manifest_revision;
    let committed = match commit_ready(
        &pool,
        &deployment_id,
        &snapshot_key,
        &preparation.checksum,
        preparation.snapshot_bytes.len() as u64,
        expected_revision,
    )
    .await
    {
        Ok(record) => record,
        Err(error) => {
            let message = error.to_string();
            let _ = mark_deployment_failed(&pool, &deployment_id, &message).await;
            let _ = finish_publish_job(&pool, &job_id, false, Some(&message)).await;
            // A commit conflict leaves no deployment reference to this
            // object. Only delete it after checking the database; if the
            // commit actually succeeded but response reconstruction failed,
            // preserve the object rather than breaking a READY deployment.
            let keep_snapshot = match find_deployment(&pool, &deployment_id).await {
                Ok(Some(record)) => {
                    record.fields.get("status").and_then(JsonValue::as_str) == Some("ready")
                        && record
                            .fields
                            .get("snapshot_key")
                            .and_then(JsonValue::as_str)
                            == Some(snapshot_key.as_str())
                }
                Ok(None) => false,
                Err(_) => true,
            };
            if !keep_snapshot {
                let _ = state.storage.delete(&snapshot_key).await;
            }
            return Err(map_docs_error(error));
        }
    };
    finish_publish_job(&pool, &job_id, true, None)
        .await
        .map_err(map_docs_error)?;
    Ok((
        StatusCode::CREATED,
        Json(PublishResponse {
            deployment: committed,
            checksum: preparation.checksum,
            snapshot_key: Some(snapshot_key),
            reused: false,
            in_progress: false,
        }),
    ))
}

pub async fn deployments(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
) -> Result<Json<Vec<rustbase_core::Record>>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    Ok(Json(list_deployments(&pool).await.map_err(map_docs_error)?))
}

pub async fn rollback(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app, deployment)): Path<(String, String, String)>,
) -> Result<Json<rustbase_core::Record>, ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    let target = find_deployment(&pool, &deployment)
        .await
        .map_err(map_docs_error)?
        .ok_or(ApiError::Core(CoreError::NotFound {
            collection: "docs_deployments".into(),
            id: deployment.clone(),
        }))?;
    let snapshot_key = target
        .fields
        .get("snapshot_key")
        .and_then(JsonValue::as_str)
        .ok_or(ApiError::Core(CoreError::Conflict(
            "target deployment has no snapshot".into(),
        )))?
        .to_string();
    let checksum = target
        .fields
        .get("checksum")
        .and_then(JsonValue::as_str)
        .ok_or(ApiError::Core(CoreError::Conflict(
            "target deployment has no checksum".into(),
        )))?
        .to_string();
    let bytes = state.storage.get(&snapshot_key).await.map_err(|error| {
        ApiError::Core(CoreError::Unavailable(format!(
            "rollback snapshot unavailable: {error}"
        )))
    })?;
    if sha256_hex(&bytes) != checksum {
        return Err(ApiError::Core(CoreError::Conflict(
            "rollback snapshot checksum mismatch".into(),
        )));
    }
    let snapshot: VersionSnapshot = serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::Core(CoreError::Conflict(format!(
            "rollback snapshot is unreadable: {error}"
        )))
    })?;
    snapshot.manifest.validate().map_err(map_docs_error)?;
    let committed =
        create_rollback_ready(&pool, &deployment, Some(&auth.admin_id), bytes.len() as u64)
            .await
            .map_err(map_docs_error)?;
    Ok(Json(committed))
}

#[derive(Debug, Deserialize)]
pub struct ReaderGrantRequest {
    pub subject: String,
    pub audience: Option<String>,
    pub path_prefix: String,
    pub expires_at: Option<String>,
}

pub async fn create_grant(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(request): Json<ReaderGrantRequest>,
) -> Result<(StatusCode, Json<rustbase_core::Record>), ApiError> {
    auth.require_app_access(&workspace, &app)?;
    let pool = open_docs_app(&state, &workspace, &app).await?;
    let row = create_reader_grant(
        &pool,
        ReaderGrant {
            subject: request.subject,
            audience: request.audience,
            path_prefix: request.path_prefix,
            effect: "allow".into(),
            expires_at: request.expires_at,
        },
        Some(&auth.admin_id),
    )
    .await
    .map_err(map_docs_error)?;
    touch_version_manifest(&pool)
        .await
        .map_err(map_docs_error)?;
    Ok((StatusCode::CREATED, Json(row)))
}

#[derive(Debug, Deserialize)]
pub struct ReaderSessionRequest {
    pub subject: String,
    #[serde(default = "default_session_ttl")]
    pub ttl_seconds: i64,
}

fn default_session_ttl() -> i64 {
    86_400
}

#[derive(Debug, Serialize)]
pub struct ReaderSessionResponse {
    pub token: String,
    pub session: rustbase_core::Record,
}

pub async fn create_reader_session_admin(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Json(request): Json<ReaderSessionRequest>,
) -> Result<(StatusCode, Json<ReaderSessionResponse>), ApiError> {
    auth.require_app_access(&workspace, &app)?;
    if !(60..=86_400 * 30).contains(&request.ttl_seconds) {
        return Err(ApiError::Core(CoreError::Validation(
            "reader session ttl_seconds must be between 60 and 2592000".into(),
        )));
    }
    let pool = open_docs_app(&state, &workspace, &app).await?;
    let token = random_reader_token();
    let expires_at =
        (chrono::Utc::now() + chrono::Duration::seconds(request.ttl_seconds)).to_rfc3339();
    let session = create_reader_session(&pool, &request.subject, &token, &expires_at)
        .await
        .map_err(map_docs_error)?;
    Ok((
        StatusCode::CREATED,
        Json(ReaderSessionResponse { token, session }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct SiteReleaseRequest {
    /// Map of App id to an immutable deployment id. The handler resolves
    /// the checksum from each App-local READY row before writing the
    /// Workspace manifest.
    pub deployments: BTreeMap<String, String>,
    pub default_app_id: String,
    #[serde(default = "default_language_code")]
    pub default_language_code: String,
}

fn default_language_code() -> String {
    "en".into()
}

pub async fn create_release(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(workspace): Path<String>,
    Json(request): Json<SiteReleaseRequest>,
) -> Result<(StatusCode, Json<rustbase_core::Record>), ApiError> {
    auth.require_workspace_access(&workspace)?;
    let workspace_pool = open_workspace(&state, &workspace).await?;
    let workspace_id = WorkspaceId::from(workspace.clone());
    let SiteReleaseRequest {
        deployments,
        default_app_id,
        default_language_code,
    } = request;
    let mut refs = BTreeMap::new();
    for (app_id, deployment_id) in deployments {
        find_app(&workspace_pool, &app_id)
            .await?
            .ok_or(ApiError::Core(CoreError::AppNotFound {
                workspace: workspace.clone(),
                app: app_id.clone(),
            }))?;
        let app_pool = ensure_app_docs(&state, &workspace_id, &AppId::from(app_id.clone())).await?;
        let deployment = find_deployment(&app_pool, &deployment_id)
            .await
            .map_err(map_docs_error)?
            .ok_or(ApiError::Core(CoreError::NotFound {
                collection: "docs_deployments".into(),
                id: deployment_id.clone(),
            }))?;
        if deployment.fields.get("status").and_then(JsonValue::as_str) != Some("ready") {
            return Err(ApiError::Core(CoreError::Conflict(format!(
                "deployment {app_id}/{deployment_id} is not READY"
            ))));
        }
        let checksum = deployment
            .fields
            .get("checksum")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(ApiError::Core(CoreError::Conflict(
                "READY deployment has no checksum".into(),
            )))?;
        let snapshot_key = deployment
            .fields
            .get("snapshot_key")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(ApiError::Core(CoreError::Conflict(
                "READY deployment has no snapshot".into(),
            )))?;
        let bytes = state.storage.get(snapshot_key).await.map_err(|error| {
            ApiError::Core(CoreError::Conflict(format!(
                "READY deployment snapshot is unavailable: {error}"
            )))
        })?;
        if sha256_hex(&bytes) != checksum {
            return Err(ApiError::Core(CoreError::Conflict(
                "READY deployment snapshot checksum mismatch".into(),
            )));
        }
        let snapshot: VersionSnapshot = serde_json::from_slice(&bytes).map_err(|error| {
            ApiError::Core(CoreError::Conflict(format!(
                "READY deployment snapshot is unreadable: {error}"
            )))
        })?;
        snapshot.manifest.validate().map_err(map_docs_error)?;
        if app_id == default_app_id
            && !snapshot
                .manifest
                .languages
                .iter()
                .any(|language| language.code == default_language_code)
        {
            return Err(ApiError::Core(CoreError::Conflict(format!(
                "default language '{}' is not available in the default App deployment",
                default_language_code
            ))));
        }
        refs.insert(
            app_id.clone(),
            ReadyDeploymentRef {
                app_id,
                deployment_id,
                checksum: checksum.to_string(),
            },
        );
    }
    let release = create_site_release(
        &workspace_pool,
        SiteReleaseInput {
            deployments: refs,
            default_app_id,
            default_language_code,
        },
        Some(&auth.admin_id),
    )
    .await
    .map_err(map_docs_error)?;
    Ok((StatusCode::CREATED, Json(release)))
}

pub async fn active_release(
    auth: AdminAuth,
    State(state): State<AppState>,
    Path(workspace): Path<String>,
) -> Result<Json<Option<rustbase_core::Record>>, ApiError> {
    auth.require_workspace_access(&workspace)?;
    let pool = open_workspace(&state, &workspace).await?;
    let release = find_active_site_release(&pool)
        .await
        .map_err(map_docs_error)?
        .map(|(record, _)| record);
    Ok(Json(release))
}

async fn open_workspace(state: &AppState, workspace: &str) -> Result<sqlx::SqlitePool, ApiError> {
    open_docs_workspace(state, workspace).await
}

fn random_reader_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut token = String::from("rdr_");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
