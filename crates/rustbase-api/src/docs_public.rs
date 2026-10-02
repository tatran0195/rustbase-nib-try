//! Public documentation reader backed only by an App-local immutable
//! `VersionSnapshot` after the initial site/version resolution.
//!
//! Draft pages are never read by these handlers. A request resolves a READY
//! deployment, verifies the object checksum, applies the snapshot's reader
//! policy, and then serves one complete snapshot with revision-based cache
//! headers.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use rustbase_core::{AppId, CoreError, Record, WorkspaceId};
use rustbase_db::{
    docs::{
        VersionManifest, VersionSnapshot, authorize_reader, find_active_site_release,
        find_deployment, find_site_catalog, find_version_manifest, reader_session_subject,
    },
    paths,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::{
    docs::{ensure_app_docs, map_docs_error, open_docs_workspace},
    error::ApiError,
    state::AppState,
};

#[derive(Debug, Deserialize, Default)]
pub struct ReaderQuery {
    pub language: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PublicPageResponse {
    pub manifest: VersionManifest,
    pub deployment_id: String,
    pub checksum: String,
    pub language: String,
    pub page: Option<Record>,
    pub navigation: Vec<Record>,
}

pub async fn version_root(
    State(state): State<AppState>,
    Path((workspace, app)): Path<(String, String)>,
    Query(query): Query<ReaderQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    serve_version_page(
        &state,
        &workspace,
        &app,
        "/",
        query.language.as_deref(),
        None,
        &headers,
    )
    .await
}

pub async fn version_page(
    State(state): State<AppState>,
    Path((workspace, app, path)): Path<(String, String, String)>,
    Query(query): Query<ReaderQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let path = normalize_public_path(&path)?;
    if path == "/sitemap.xml" {
        return sitemap(
            &state,
            &workspace,
            &app,
            query.language.as_deref(),
            &headers,
        )
        .await;
    }
    if path == "/robots.txt" {
        return robots(&state, &workspace, &app).await;
    }
    serve_version_page(
        &state,
        &workspace,
        &app,
        &path,
        query.language.as_deref(),
        None,
        &headers,
    )
    .await
}

/// Site shell: use the immutable Workspace release manifest when one exists;
/// otherwise fall back to the site's default App active pointer. The selected
/// App snapshot is still loaded locally from that App database/storage prefix.
pub async fn site_root(
    State(state): State<AppState>,
    Path(workspace): Path<String>,
    Query(query): Query<ReaderQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let workspace_pool = open_workspace(&state, &workspace).await?;
    if let Some((_, release)) = find_active_site_release(&workspace_pool)
        .await
        .map_err(map_docs_error)?
    {
        let selected = release
            .deployments
            .get(&release.default_app_id)
            .ok_or(ApiError::Core(CoreError::Unavailable(
                "active site release has no default App".into(),
            )))?;
        return serve_version_page(
            &state,
            &workspace,
            &selected.app_id,
            "/",
            query
                .language
                .as_deref()
                .or(Some(release.default_language_code.as_str())),
            Some(&selected.deployment_id),
            &headers,
        )
        .await;
    }
    let site = find_site_catalog(&workspace_pool)
        .await
        .map_err(map_docs_error)?
        .ok_or(ApiError::Core(CoreError::Unavailable(
            "site catalog is not initialized".into(),
        )))?;
    let app = site
        .fields
        .get("default_app_id")
        .and_then(JsonValue::as_str)
        .ok_or(ApiError::Core(CoreError::Unavailable(
            "site has no default version".into(),
        )))?;
    serve_version_page(
        &state,
        &workspace,
        app,
        "/",
        query.language.as_deref(),
        None,
        &headers,
    )
    .await
}

async fn serve_version_page(
    state: &AppState,
    workspace: &str,
    app: &str,
    path: &str,
    requested_language: Option<&str>,
    deployment_id: Option<&str>,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let app_pool = open_public_app(state, workspace, app).await?;
    let (snapshot, deployment_id, checksum) =
        load_snapshot(state, &app_pool, deployment_id).await?;
    let language = resolve_language(&snapshot.manifest, requested_language);
    let path = if path.is_empty() { "/" } else { path };
    let private_snapshot = snapshot.manifest.access_mode != "public";
    authorize_reader(&app_pool, &snapshot, path, reader_token(headers))
        .await
        .map_err(map_docs_error)?;
    let page = snapshot
        .pages
        .iter()
        .find(|record| {
            record
                .fields
                .get("language_code")
                .and_then(JsonValue::as_str)
                == Some(language.as_str())
                && record.fields.get("path").and_then(JsonValue::as_str) == Some(path)
        })
        .cloned();
    if page.is_none() {
        return Err(ApiError::Core(CoreError::NotFound {
            collection: "docs_pages".into(),
            id: path.to_string(),
        }));
    }
    let navigation = snapshot
        .pages
        .iter()
        .filter(|record| {
            record
                .fields
                .get("language_code")
                .and_then(JsonValue::as_str)
                == Some(language.as_str())
                && record.fields.get("hidden").and_then(JsonValue::as_bool) != Some(true)
        })
        .cloned()
        .collect();
    let body = PublicPageResponse {
        manifest: snapshot.manifest,
        deployment_id,
        checksum: checksum.clone(),
        language,
        page,
        navigation,
    };
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        if private_snapshot {
            HeaderValue::from_static("private, no-store")
        } else {
            HeaderValue::from_static("public, max-age=60, stale-while-revalidate=300")
        },
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{checksum}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("\"invalid\"")),
    );
    if private_snapshot {
        response.headers_mut().insert(
            header::VARY,
            HeaderValue::from_static("Authorization, X-Docs-Reader"),
        );
    }
    Ok(response)
}

async fn load_snapshot(
    state: &AppState,
    app_pool: &sqlx::SqlitePool,
    requested_deployment: Option<&str>,
) -> Result<(VersionSnapshot, String, String), ApiError> {
    let (manifest_record, _) = find_version_manifest(app_pool)
        .await
        .map_err(map_docs_error)?;
    let deployment_id = requested_deployment
        .map(str::to_string)
        .or_else(|| {
            manifest_record
                .fields
                .get("active_deployment_id")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .ok_or(ApiError::Core(CoreError::Unavailable(
            "version has no active READY deployment".into(),
        )))?;
    let deployment = find_deployment(app_pool, &deployment_id)
        .await
        .map_err(map_docs_error)?
        .ok_or(ApiError::Core(CoreError::Unavailable(
            "active deployment record is missing".into(),
        )))?;
    let status = deployment.fields.get("status").and_then(JsonValue::as_str);
    let acceptable = if requested_deployment.is_some() {
        matches!(status, Some("ready" | "superseded"))
    } else {
        status == Some("ready")
    };
    if !acceptable {
        return Err(ApiError::Core(CoreError::Unavailable(
            "selected deployment is not complete".into(),
        )));
    }
    let key = deployment
        .fields
        .get("snapshot_key")
        .and_then(JsonValue::as_str)
        .ok_or(ApiError::Core(CoreError::Unavailable(
            "READY deployment has no snapshot".into(),
        )))?;
    let checksum = deployment
        .fields
        .get("checksum")
        .and_then(JsonValue::as_str)
        .ok_or(ApiError::Core(CoreError::Unavailable(
            "READY deployment has no checksum".into(),
        )))?;
    let bytes = state.storage.get(key).await.map_err(|error| {
        ApiError::Core(CoreError::Unavailable(format!(
            "snapshot unavailable: {error}"
        )))
    })?;
    if sha256_hex(&bytes) != checksum {
        return Err(ApiError::Core(CoreError::Internal(
            "snapshot checksum mismatch".into(),
        )));
    }
    let snapshot: VersionSnapshot = serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::Core(CoreError::Internal(format!(
            "snapshot is unreadable: {error}"
        )))
    })?;
    if snapshot.manifest.snapshot_schema_version == 0
        || snapshot.manifest.renderer_version.is_empty()
    {
        return Err(ApiError::Core(CoreError::Internal(
            "snapshot schema is unsupported".into(),
        )));
    }
    Ok((snapshot, deployment_id, checksum.to_string()))
}

async fn sitemap(
    state: &AppState,
    workspace: &str,
    app: &str,
    requested_language: Option<&str>,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let pool = open_public_app(state, workspace, app).await?;
    let (snapshot, _, checksum) = load_snapshot(state, &pool, None).await?;
    let private_snapshot = snapshot.manifest.access_mode != "public";
    if private_snapshot {
        reader_session_subject(&pool, reader_token(headers))
            .await
            .map_err(map_docs_error)?;
    }
    let language = resolve_language(&snapshot.manifest, requested_language);
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">",
    );
    for page in snapshot.pages.iter().filter(|page| {
        page.fields.get("language_code").and_then(JsonValue::as_str) == Some(language.as_str())
    }) {
        if page.fields.get("hidden").and_then(JsonValue::as_bool) == Some(true) {
            continue;
        }
        if let Some(path) = page.fields.get("path").and_then(JsonValue::as_str) {
            if private_snapshot
                && authorize_reader(&pool, &snapshot, path, reader_token(headers))
                    .await
                    .is_err()
            {
                continue;
            }
            body.push_str("<url><loc>");
            body.push_str(&xml_escape(path));
            body.push_str("</loc></url>");
        }
    }
    body.push_str("</urlset>");
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        if private_snapshot {
            HeaderValue::from_static("private, no-store")
        } else {
            HeaderValue::from_static("public, max-age=60")
        },
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{checksum}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("\"invalid\"")),
    );
    if private_snapshot {
        response.headers_mut().insert(
            header::VARY,
            HeaderValue::from_static("Authorization, X-Docs-Reader"),
        );
    }
    Ok(response)
}

async fn robots(state: &AppState, workspace: &str, app: &str) -> Result<Response, ApiError> {
    let pool = open_public_app(state, workspace, app).await?;
    let (snapshot, _, checksum) = load_snapshot(state, &pool, None).await?;
    let private_snapshot = snapshot.manifest.access_mode != "public";
    let body = if !private_snapshot {
        "User-agent: *\nAllow: /\n"
    } else {
        "User-agent: *\nDisallow: /\n"
    };
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        if private_snapshot {
            HeaderValue::from_static("private, no-store")
        } else {
            HeaderValue::from_static("public, max-age=60")
        },
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{checksum}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("\"invalid\"")),
    );
    if private_snapshot {
        response.headers_mut().insert(
            header::VARY,
            HeaderValue::from_static("Authorization, X-Docs-Reader"),
        );
    }
    Ok(response)
}

fn resolve_language(manifest: &VersionManifest, requested: Option<&str>) -> String {
    let Some(requested) = requested
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "*")
    else {
        return manifest.default_language_code.clone();
    };

    let normalized = requested.replace('_', "-").to_ascii_lowercase();
    let mut candidates = Vec::new();
    let mut candidate = normalized.as_str();
    loop {
        candidates.push(candidate);
        let Some((base, _)) = candidate.rsplit_once('-') else {
            break;
        };
        candidate = base;
    }
    manifest
        .languages
        .iter()
        .find(|language| {
            candidates
                .iter()
                .any(|candidate| language.code.to_ascii_lowercase() == *candidate)
        })
        .map(|language| language.code.clone())
        .unwrap_or_else(|| manifest.default_language_code.clone())
}

fn reader_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("x-docs-reader")
                .and_then(|value| value.to_str().ok())
        })
}

fn normalize_public_path(path: &str) -> Result<String, ApiError> {
    let decoded = format!("/{path}");
    if decoded.contains("//")
        || decoded.contains("..")
        || decoded.contains('?')
        || decoded.contains('#')
    {
        return Err(ApiError::Core(CoreError::Validation(
            "invalid public documentation path".into(),
        )));
    }
    Ok(if decoded == "/" {
        decoded
    } else {
        decoded.trim_end_matches('/').to_string()
    })
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

async fn open_public_app(
    state: &AppState,
    workspace: &str,
    app: &str,
) -> Result<sqlx::SqlitePool, ApiError> {
    validate_scope_segment(workspace, "workspace")?;
    validate_scope_segment(app, "app")?;
    let workspace_id = WorkspaceId::from(workspace.to_string());
    let app_id = AppId::from(app.to_string());
    let db_path = paths::app_db(&state.data_dir, &workspace_id, &app_id);
    if !db_path.is_file() {
        return Err(ApiError::Core(CoreError::AppNotFound {
            workspace: workspace.to_string(),
            app: app.to_string(),
        }));
    }
    // Explicit version serving does not query workspace.db or another App.
    // The shared helper only repairs this App-local package; the active
    // pointer, effective manifest, and immutable snapshot remain local.
    ensure_app_docs(state, &workspace_id, &app_id).await
}

async fn open_workspace(state: &AppState, workspace: &str) -> Result<sqlx::SqlitePool, ApiError> {
    open_docs_workspace(state, workspace).await
}

fn validate_scope_segment(value: &str, label: &str) -> Result<(), ApiError> {
    if !(2..=50).contains(&value.len())
        || value.starts_with('-')
        || value.ends_with('-')
        || !value.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(ApiError::Core(CoreError::Validation(format!(
            "{label} has an invalid identifier"
        ))));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
