//! Reserved documentation collection schemas and bootstrap helpers.
//!
//! Documentation data deliberately uses RustBase's existing dynamic
//! collection/record substrate. This module owns the reserved schemas and
//! protects their shape; docs domain services should own the invariants and
//! must not expose raw generic CRUD as the public content API.

use crate::collections::{Collection, create_collection, find_collection, patch_collection};
use crate::error::{DbError, Result};
use crate::records::{
    create_record, create_record_in_tx, delete_record, find_record, list_records,
    list_records_ordered, update_record, update_record_in_tx,
};
use rustbase_core::{CollectionId, CollectionKind, Field, FieldType, FilterNode, Record, Schema};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// Collections created in every documentation Workspace database.
pub const WORKSPACE_COLLECTION_IDS: &[&str] = &[
    "docs_site",
    "docs_versions",
    "docs_site_releases",
    "docs_readers",
    "docs_audiences",
    "docs_domains",
];

/// Collections created in every documentation version App database.
pub const APP_COLLECTION_IDS: &[&str] = &[
    "docs_version_meta",
    "docs_languages",
    "docs_pages",
    "docs_assets",
    "docs_redirects",
    "docs_openapi",
    "docs_deployments",
    "docs_jobs",
    "docs_comments",
    "docs_reader_grants",
    "docs_reader_sessions",
    "docs_search_documents",
];

/// Generic collection administration must not alter these schemas after the
/// docs module has bootstrapped them. Domain migrations are the only supported
/// way to change a reserved docs collection.
pub fn is_reserved_docs_collection(id: &str) -> bool {
    WORKSPACE_COLLECTION_IDS
        .iter()
        .chain(APP_COLLECTION_IDS.iter())
        .any(|reserved| *reserved == id)
}

/// Ensure the Workspace-level documentation collections exist and have the
/// expected schema. Safe to call on every Workspace open/create path.
pub async fn bootstrap_workspace_collections(pool: &SqlitePool) -> Result<()> {
    for schema in workspace_schemas() {
        ensure_collection(pool, &schema).await?;
    }
    ensure_workspace_indexes(pool).await
}

/// Ensure the App-level documentation collections exist and have the expected
/// schema. Safe to call on every version App open/create path.
pub async fn bootstrap_app_collections(pool: &SqlitePool) -> Result<()> {
    for schema in app_schemas() {
        ensure_collection(pool, &schema).await?;
    }
    ensure_app_indexes(pool).await?;
    ensure_default_language(pool).await
}

/// Input for creating a documentation page/group. The version boundary is
/// the App, so no branch/version ID is stored in this record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageDraftInput {
    pub language_code: String,
    pub parent_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub slug: String,
    pub path: String,
    pub position: f64,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub hidden: bool,
    pub content: String,
    pub content_format: String,
    pub config: Option<Json>,
    pub translation_key: Option<String>,
    pub source_metadata: Option<Json>,
}

impl PageDraftInput {
    fn validate(&self) -> Result<()> {
        validate_language_code(&self.language_code)?;
        if !matches!(self.kind.as_str(), "page" | "group") {
            return Err(DbError::InvalidIdentifier(
                "page kind must be 'page' or 'group'".into(),
            ));
        }
        if self.title.trim().is_empty() {
            return Err(DbError::InvalidIdentifier(
                "page title must not be empty".into(),
            ));
        }
        if self.slug.trim().is_empty() || self.path.trim().is_empty() {
            return Err(DbError::InvalidIdentifier(
                "page slug and path must not be empty".into(),
            ));
        }
        validate_slug(&self.slug, "page slug")?;
        validate_page_path(&self.path)?;
        if self.kind == "group" && !self.content.trim().is_empty() {
            return Err(DbError::Invariant(
                "group pages cannot contain content".into(),
            ));
        }
        if self.content.len() > 2 * 1024 * 1024 {
            return Err(DbError::InvalidIdentifier(
                "page content exceeds 2 MiB".into(),
            ));
        }
        if !matches!(self.content_format.as_str(), "markdown" | "mdx") {
            return Err(DbError::InvalidIdentifier(
                "content_format must be 'markdown' or 'mdx'".into(),
            ));
        }
        if !self.position.is_finite() {
            return Err(DbError::InvalidIdentifier(
                "page position must be finite".into(),
            ));
        }
        Ok(())
    }

    fn fields(&self) -> BTreeMap<String, Json> {
        BTreeMap::from([
            ("language_code".into(), json!(self.language_code.clone())),
            (
                "parent_id".into(),
                self.parent_id.clone().map_or(Json::Null, Json::String),
            ),
            ("kind".into(), json!(self.kind.clone())),
            ("title".into(), json!(self.title.clone())),
            ("slug".into(), json!(self.slug.clone())),
            ("path".into(), json!(self.path.clone())),
            ("position".into(), json!(self.position)),
            (
                "description".into(),
                self.description.clone().map_or(Json::Null, Json::String),
            ),
            (
                "icon".into(),
                self.icon.clone().map_or(Json::Null, Json::String),
            ),
            ("hidden".into(), json!(self.hidden)),
            ("content".into(), json!(self.content.clone())),
            ("content_format".into(), json!(self.content_format.clone())),
            ("config".into(), self.config.clone().unwrap_or(Json::Null)),
            (
                "translation_key".into(),
                self.translation_key
                    .clone()
                    .map_or(Json::Null, Json::String),
            ),
            (
                "source_metadata".into(),
                self.source_metadata.clone().unwrap_or(Json::Null),
            ),
        ])
    }
}

/// Patch accepted by page-specific update/move services. `parent_id` uses a
/// nested option so callers can explicitly clear a parent (`Some(None)`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PagePatch {
    pub parent_id: Option<Option<String>>,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub path: Option<String>,
    pub position: Option<f64>,
    pub description: Option<Option<String>>,
    pub icon: Option<Option<String>>,
    pub hidden: Option<bool>,
    pub content: Option<String>,
    pub content_format: Option<String>,
    pub config: Option<Option<Json>>,
    pub translation_key: Option<Option<String>>,
    pub source_metadata: Option<Option<Json>>,
}

/// Create a page through the dynamic record engine after enforcing the first
/// page-tree invariants. Composite path uniqueness is enforced by the
/// `docs_pages_language_path` SQLite index.
pub async fn create_page(pool: &SqlitePool, input: PageDraftInput) -> Result<Record> {
    input.validate()?;
    let schema = docs_pages_schema(pool).await?;
    validate_parent(
        pool,
        &schema,
        input.parent_id.as_deref(),
        &input.language_code,
        Some(&input.path),
    )
    .await?;
    create_record(pool, &schema, input.fields()).await
}

/// Update page content/metadata while preserving tree invariants. This method
/// does not run Markdown/MDX rendering; publish validation owns that policy.
pub async fn update_page(pool: &SqlitePool, id: &str, patch: PagePatch) -> Result<Record> {
    let schema = docs_pages_schema(pool).await?;
    let current = find_record(pool, &schema, id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;

    let language = string_field(&current, "language_code")?;
    let current_parent = optional_string_field(&current, "parent_id");
    let parent = patch.parent_id.clone().unwrap_or(current_parent.clone());
    let current_path = string_field(&current, "path")?;
    let proposed_path = patch.path.clone().unwrap_or_else(|| current_path.clone());
    validate_parent(
        pool,
        &schema,
        parent.as_deref(),
        &language,
        Some(&proposed_path),
    )
    .await?;
    if let Some(parent_id) = parent.as_deref() {
        ensure_no_cycle(pool, &schema, id, parent_id).await?;
    }

    let mut fields = BTreeMap::new();
    if let Some(parent_id) = patch.parent_id {
        fields.insert(
            "parent_id".into(),
            parent_id.map_or(Json::Null, Json::String),
        );
    }
    if let Some(value) = patch.kind {
        if !matches!(value.as_str(), "page" | "group") {
            return Err(DbError::InvalidIdentifier(
                "page kind must be 'page' or 'group'".into(),
            ));
        }
        fields.insert("kind".into(), json!(value));
    }
    if let Some(value) = patch.title {
        if value.trim().is_empty() {
            return Err(DbError::InvalidIdentifier(
                "page title must not be empty".into(),
            ));
        }
        fields.insert("title".into(), json!(value));
    }
    if let Some(value) = patch.slug {
        if value.trim().is_empty() {
            return Err(DbError::InvalidIdentifier(
                "page slug must not be empty".into(),
            ));
        }
        validate_slug(&value, "page slug")?;
        fields.insert("slug".into(), json!(value));
    }
    if let Some(value) = patch.path {
        if value.trim().is_empty() {
            return Err(DbError::InvalidIdentifier(
                "page path must not be empty".into(),
            ));
        }
        validate_page_path(&value)?;
        fields.insert("path".into(), json!(value));
    }
    if let Some(value) = patch.position {
        if !value.is_finite() {
            return Err(DbError::InvalidIdentifier(
                "page position must be finite".into(),
            ));
        }
        fields.insert("position".into(), json!(value));
    }
    if let Some(value) = patch.description {
        fields.insert("description".into(), value.map_or(Json::Null, Json::String));
    }
    if let Some(value) = patch.icon {
        fields.insert("icon".into(), value.map_or(Json::Null, Json::String));
    }
    if let Some(value) = patch.hidden {
        fields.insert("hidden".into(), json!(value));
    }
    if let Some(value) = patch.content {
        fields.insert("content".into(), json!(value));
    }
    if let Some(value) = patch.content_format {
        if !matches!(value.as_str(), "markdown" | "mdx") {
            return Err(DbError::InvalidIdentifier(
                "content_format must be 'markdown' or 'mdx'".into(),
            ));
        }
        fields.insert("content_format".into(), json!(value));
    }
    if let Some(value) = patch.config {
        fields.insert("config".into(), value.unwrap_or(Json::Null));
    }
    if let Some(value) = patch.translation_key {
        fields.insert(
            "translation_key".into(),
            value.map_or(Json::Null, Json::String),
        );
    }
    if let Some(value) = patch.source_metadata {
        fields.insert("source_metadata".into(), value.unwrap_or(Json::Null));
    }

    let final_kind = fields
        .get("kind")
        .and_then(Json::as_str)
        .or_else(|| current.fields.get("kind").and_then(Json::as_str))
        .unwrap_or_default();
    let final_content = fields
        .get("content")
        .and_then(Json::as_str)
        .or_else(|| current.fields.get("content").and_then(Json::as_str))
        .unwrap_or_default();
    if final_kind == "group" && !final_content.trim().is_empty() {
        return Err(DbError::Invariant(
            "group pages cannot contain content".into(),
        ));
    }
    if final_content.len() > 2 * 1024 * 1024 {
        return Err(DbError::InvalidIdentifier(
            "page content exceeds 2 MiB".into(),
        ));
    }

    if fields.is_empty() {
        return Ok(current);
    }
    let path_updates = if proposed_path != current_path {
        descendant_path_updates(pool, &schema, id, &language, &current_path, &proposed_path).await?
    } else {
        Vec::new()
    };
    let mut tx = pool.begin().await?;
    // Rewrite descendants before the root. This frees any destination path
    // currently occupied by a descendant, while the transaction keeps the
    // intermediate state invisible to other readers.
    for (child_id, path) in path_updates {
        update_record_in_tx(
            &mut tx,
            &schema,
            &child_id,
            BTreeMap::from([("path".into(), Json::String(path))]),
        )
        .await?;
    }
    update_record_in_tx(&mut tx, &schema, id, fields).await?;
    tx.commit().await?;
    find_record(pool, &schema, id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))
}

/// Delete a page only when it has no children. Recursive deletion is an
/// explicit future operation because it must also define redirect/comment/
/// asset semantics.
pub async fn delete_page(pool: &SqlitePool, id: &str) -> Result<()> {
    let schema = docs_pages_schema(pool).await?;
    let current = find_record(pool, &schema, id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;
    let language = string_field(&current, "language_code")?;
    let child_filter = FilterNode::and(
        FilterNode::Eq("parent_id".into(), Json::String(id.to_string())),
        FilterNode::Eq("language_code".into(), Json::String(language)),
    );
    let children = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 1,
        },
        Some(&child_filter),
    )
    .await?;
    if children.total_items > 0 {
        return Err(DbError::Invariant(
            "cannot delete a page that still has children".into(),
        ));
    }
    delete_record(pool, &schema, id).await
}

async fn docs_pages_schema(pool: &SqlitePool) -> Result<Schema> {
    find_collection(pool, "docs_pages")
        .await?
        .map(|collection| collection.schema)
        .ok_or_else(|| DbError::Invariant("docs_pages collection is not bootstrapped".into()))
}

async fn validate_parent(
    pool: &SqlitePool,
    schema: &Schema,
    parent_id: Option<&str>,
    language_code: &str,
    proposed_path: Option<&str>,
) -> Result<()> {
    let Some(parent_id) = parent_id else {
        return Ok(());
    };
    let parent = find_record(pool, schema, parent_id)
        .await?
        .ok_or_else(|| DbError::Invariant("page parent does not exist".into()))?;
    if string_field(&parent, "language_code")? != language_code {
        return Err(DbError::Invariant(
            "page parent must use the same language".into(),
        ));
    }
    if let Some(proposed_path) = proposed_path {
        let parent_path = string_field(&parent, "path")?;
        let is_descendant = parent_path == "/"
            || proposed_path == parent_path
            || proposed_path.starts_with(&format!("{parent_path}/"));
        if !is_descendant {
            return Err(DbError::Invariant(
                "page path must be inside its parent path".into(),
            ));
        }
    }
    Ok(())
}

async fn ensure_no_cycle(
    pool: &SqlitePool,
    schema: &Schema,
    page_id: &str,
    proposed_parent_id: &str,
) -> Result<()> {
    let mut current = Some(proposed_parent_id.to_string());
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if id == page_id {
            return Err(DbError::Invariant("page tree cycle detected".into()));
        }
        if !visited.insert(id.clone()) {
            return Err(DbError::Invariant(
                "existing page tree cycle detected".into(),
            ));
        }
        let Some(record) = find_record(pool, schema, &id).await? else {
            return Err(DbError::Invariant("page parent does not exist".into()));
        };
        current = optional_string_field(&record, "parent_id");
        if visited.len() > 10_000 {
            return Err(DbError::Invariant("page tree is too deep".into()));
        }
    }
    Ok(())
}

/// Compute the path changes required when a page with descendants moves.
/// Parent IDs remain stable; every descendant receives the same path suffix
/// under the new root path. The collision check happens before any write so
/// the subsequent transaction cannot leave a half-moved subtree because of a
/// predictable unique-path conflict.
async fn descendant_path_updates(
    pool: &SqlitePool,
    schema: &Schema,
    root_id: &str,
    language: &str,
    old_root: &str,
    new_root: &str,
) -> Result<Vec<(String, String)>> {
    let pages = list_all_records(pool, schema, "position").await?;
    let mut children: BTreeMap<String, Vec<&Record>> = BTreeMap::new();
    for page in &pages {
        if string_field(page, "language_code")? != language {
            continue;
        }
        if let Some(parent_id) = optional_string_field(page, "parent_id") {
            children.entry(parent_id).or_default().push(page);
        }
    }

    let mut stack = vec![root_id.to_string()];
    let mut subtree = HashSet::from([root_id.to_string()]);
    let mut updates = Vec::new();
    while let Some(parent_id) = stack.pop() {
        for child in children.get(&parent_id).into_iter().flatten() {
            if !subtree.insert(child.id.to_string()) {
                return Err(DbError::Invariant(
                    "existing page tree cycle detected".into(),
                ));
            }
            let old_path = string_field(child, "path")?;
            let suffix = if old_root == "/" {
                old_path.as_str()
            } else {
                old_path.strip_prefix(old_root).ok_or_else(|| {
                    DbError::Invariant("page descendant path is outside its parent subtree".into())
                })?
            };
            let new_path = if new_root == "/" {
                if suffix.is_empty() {
                    "/".to_string()
                } else {
                    suffix.to_string()
                }
            } else {
                format!("{new_root}{suffix}")
            };
            validate_page_path(&new_path)?;
            updates.push((child.id.to_string(), new_path));
            stack.push(child.id.to_string());
        }
    }

    let mut final_paths = vec![new_root.to_string()];
    final_paths.extend(updates.iter().map(|(_, path)| path.clone()));
    let distinct_paths: HashSet<&str> = final_paths.iter().map(String::as_str).collect();
    if distinct_paths.len() != final_paths.len() {
        return Err(DbError::Invariant(
            "moving the page subtree would create duplicate paths".into(),
        ));
    }
    let proposed_paths: HashSet<&str> = distinct_paths;
    for page in pages {
        if page.fields.get("language_code").and_then(Json::as_str) != Some(language)
            || subtree.contains(page.id.as_str())
        {
            continue;
        }
        if let Some(path) = page.fields.get("path").and_then(Json::as_str)
            && proposed_paths.contains(path)
        {
            return Err(DbError::Invariant(
                "moving the page subtree would collide with an existing path".into(),
            ));
        }
    }
    // Move descendants deepest-first so a destination that is currently
    // occupied by a shallower member of this same subtree is freed before
    // the root path is updated.
    updates.sort_by(|left, right| right.1.len().cmp(&left.1.len()));
    Ok(updates)
}

fn string_field(record: &Record, field: &str) -> Result<String> {
    match record.fields.get(field) {
        Some(Json::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        _ => Err(DbError::Invariant(format!(
            "page field '{field}' is missing or not text"
        ))),
    }
}

fn optional_string_field(record: &Record, field: &str) -> Option<String> {
    match record.fields.get(field) {
        Some(Json::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

async fn ensure_collection(pool: &SqlitePool, expected: &Schema) -> Result<Collection> {
    if let Some(existing) = find_collection(pool, expected.id.as_str()).await? {
        if existing.kind != expected.kind {
            return Err(DbError::Invariant(format!(
                "reserved documentation collection '{}' has an incompatible kind",
                expected.id
            )));
        }
        // Reserved docs schemas evolve additively through this bootstrap
        // lock. Never drop an existing field implicitly; a field removal or
        // type change is a deliberate migration and fails closed.
        let (updated, _) = patch_collection(pool, expected, false).await.map_err(|_| {
            DbError::Invariant(format!(
                "reserved documentation collection '{}' has an incompatible schema",
                expected.id
            ))
        })?;
        return Ok(updated);
    }

    create_collection(pool, expected).await
}

async fn ensure_workspace_indexes(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(
        r#"
        CREATE INDEX IF NOT EXISTS docs_versions_position
            ON "docs_versions"("position");
        CREATE INDEX IF NOT EXISTS docs_site_releases_status
            ON "docs_site_releases"("status", "created_at");
        CREATE INDEX IF NOT EXISTS docs_readers_status
            ON "docs_readers"("status");
        CREATE INDEX IF NOT EXISTS docs_domains_status
            ON "docs_domains"("status", "hostname");
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_app_indexes(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS docs_pages_language_path
            ON "docs_pages"("language_code", "path");
        CREATE UNIQUE INDEX IF NOT EXISTS docs_pages_language_slug
            ON "docs_pages"("language_code", "slug");
        CREATE INDEX IF NOT EXISTS docs_pages_tree_order
            ON "docs_pages"("language_code", "parent_id", "position");
        CREATE INDEX IF NOT EXISTS docs_deployments_status
            ON "docs_deployments"("status", "created_at");
        CREATE INDEX IF NOT EXISTS docs_jobs_claim
            ON "docs_jobs"("status", "run_after", "lease_until");
        CREATE INDEX IF NOT EXISTS docs_search_documents_deployment
            ON "docs_search_documents"("deployment_id", "language_code");
        CREATE INDEX IF NOT EXISTS docs_reader_grants_subject
            ON "docs_reader_grants"("subject", "enabled");
        CREATE INDEX IF NOT EXISTS docs_reader_sessions_token
            ON "docs_reader_sessions"("token_hash", "revoked");
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_default_language(pool: &SqlitePool) -> Result<()> {
    let schema = find_collection(pool, "docs_languages")
        .await?
        .map(|collection| collection.schema)
        .ok_or_else(|| {
            DbError::Invariant("docs_languages collection is not bootstrapped".into())
        })?;
    let existing = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 200,
        },
        None,
    )
    .await?;
    if existing.total_items == 0 {
        create_record(
            pool,
            &schema,
            BTreeMap::from([
                ("code".into(), Json::String("en".into())),
                ("label".into(), Json::String("English".into())),
                ("direction".into(), Json::String("ltr".into())),
                ("is_default".into(), Json::Bool(true)),
                ("enabled".into(), Json::Bool(true)),
                ("position".into(), json!(0.0)),
                ("config".into(), json!({})),
            ]),
        )
        .await?;
        return Ok(());
    }

    let default_id = existing.items.iter().find_map(|record| {
        let is_default = record
            .fields
            .get("is_default")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let enabled = record
            .fields
            .get("enabled")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        (is_default && enabled).then(|| record.id.to_string())
    });
    let fallback_default = default_id.or_else(|| {
        existing.items.iter().find_map(|record| {
            record
                .fields
                .get("enabled")
                .and_then(Json::as_bool)
                .unwrap_or(false)
                .then(|| record.id.to_string())
        })
    });
    let Some(default_id) = fallback_default else {
        return Err(DbError::Invariant(
            "at least one enabled language is required".into(),
        ));
    };
    for record in existing.items {
        let is_default = record.id.as_str() == default_id.as_str();
        if record.fields.get("is_default").and_then(Json::as_bool) != Some(is_default) {
            update_record(
                pool,
                &schema,
                record.id.as_str(),
                BTreeMap::from([("is_default".into(), Json::Bool(is_default))]),
            )
            .await?;
        }
    }
    Ok(())
}

fn workspace_schemas() -> Vec<Schema> {
    vec![
        schema(
            "docs_site",
            vec![
                text("name", true, false),
                text("slug", true, true),
                json("config", false),
                text("default_app_id", false, false),
                text("active_release_id", false, false),
                number("manifest_revision", false),
            ],
        ),
        schema(
            "docs_versions",
            vec![
                text("app_id", true, true),
                text("slug", true, true),
                text("name", true, false),
                boolean("is_default", false),
                boolean("enabled", false),
                number("position", false),
                text("source_branch_id", false, false),
                json("config", false),
            ],
        ),
        schema(
            "docs_site_releases",
            vec![
                number("version", true),
                text("status", true, false),
                json("deployment_map", true),
                text("checksum", false, true),
                text("created_by", false, false),
                text("error", false, false),
                text("completed_at", false, false),
            ],
        ),
        schema(
            "docs_readers",
            vec![
                text("subject", true, true),
                text("email", false, true),
                text("status", true, false),
                json("metadata", false),
            ],
        ),
        schema(
            "docs_audiences",
            vec![
                text("slug", true, true),
                text("name", true, false),
                boolean("enabled", false),
                json("metadata", false),
            ],
        ),
        schema(
            "docs_domains",
            vec![
                text("hostname", true, true),
                text("status", true, false),
                text("verification_token", false, false),
                boolean("primary", false),
                text("app_id", false, false),
                text("last_error", false, false),
            ],
        ),
    ]
}

fn app_schemas() -> Vec<Schema> {
    vec![
        schema(
            "docs_version_meta",
            vec![
                text("site_slug", true, false),
                text("version_slug", true, true),
                text("version_name", true, false),
                json("manifest", true),
                text("renderer_version", true, false),
                text("active_deployment_id", false, false),
                number("manifest_revision", false),
            ],
        ),
        schema(
            "docs_languages",
            vec![
                text("code", true, true),
                text("label", true, false),
                text("direction", true, false),
                boolean("is_default", false),
                boolean("enabled", false),
                number("position", false),
                json("config", false),
            ],
        ),
        schema(
            "docs_pages",
            vec![
                text("language_code", true, false),
                relation("parent_id", "docs_pages", false),
                text("kind", true, false),
                text("title", true, false),
                text("slug", true, false),
                text("path", true, false),
                number("position", false),
                text("description", false, false),
                text("icon", false, false),
                boolean("hidden", false),
                text("content", false, false),
                text("content_format", true, false),
                json("config", false),
                text("translation_key", false, false),
                json("source_metadata", false),
            ],
        ),
        schema(
            "docs_assets",
            vec![
                text("key", true, true),
                text("storage_key", true, true),
                text("content_type", true, false),
                number("size", true),
                text("checksum", true, false),
                text("visibility", true, false),
                number("reference_count", false),
                text("created_by", false, false),
            ],
        ),
        schema(
            "docs_redirects",
            vec![
                text("source_path", true, true),
                text("target_path", true, false),
                number("status", true),
                text("language_code", false, false),
            ],
        ),
        schema(
            "docs_openapi",
            vec![
                text("title", true, false),
                text("path", true, false),
                text("source_type", true, false),
                json("document", true),
                text("content_hash", true, true),
            ],
        ),
        schema(
            "docs_deployments",
            vec![
                number("version", true),
                text("status", true, false),
                // Multiple deployment records may reference the same
                // immutable object (rollback), so neither reference is
                // a uniqueness constraint.
                text("snapshot_key", false, false),
                text("checksum", false, false),
                number("snapshot_size", false),
                text("source_revision", false, false),
                text("renderer_version", false, false),
                number("pages_count", false),
                text("created_by", false, false),
                text("error", false, false),
                json("error_details", false),
                text("completed_at", false, false),
            ],
        ),
        schema(
            "docs_jobs",
            vec![
                text("kind", true, false),
                text("status", true, false),
                json("payload", true),
                text("idempotency_key", false, true),
                number("attempts", false),
                text("run_after", false, false),
                text("lease_owner", false, false),
                text("lease_until", false, false),
                text("last_error", false, false),
                text("started_at", false, false),
                text("completed_at", false, false),
            ],
        ),
        schema(
            "docs_comments",
            vec![
                relation("page_id", "docs_pages", false),
                text("user_id", true, false),
                text("body", true, false),
                boolean("resolved", false),
                json("anchor", false),
            ],
        ),
        schema(
            "docs_reader_grants",
            vec![
                text("subject", true, false),
                text("audience", false, false),
                text("path_prefix", true, false),
                text("effect", true, false),
                text("expires_at", false, false),
                boolean("enabled", false),
                text("created_by", false, false),
            ],
        ),
        schema(
            "docs_reader_sessions",
            vec![
                text("token_hash", true, true),
                text("subject", true, false),
                text("expires_at", true, false),
                boolean("revoked", false),
            ],
        ),
        schema(
            "docs_search_documents",
            vec![
                relation("page_id", "docs_pages", true),
                text("deployment_id", true, false),
                text("language_code", true, false),
                text("path", true, false),
                text("title", true, false),
                text("body", true, false),
                text("visibility", true, false),
            ],
        ),
    ]
}

fn schema(id: &'static str, fields: Vec<Field>) -> Schema {
    Schema {
        id: CollectionId::from(id),
        kind: CollectionKind::Base,
        fields,
    }
}

fn text(name: &'static str, required: bool, unique: bool) -> Field {
    Field {
        name: name.to_string(),
        ty: FieldType::Text {
            min: None,
            max: None,
        },
        required,
        unique,
    }
}

fn json(name: &'static str, required: bool) -> Field {
    Field {
        name: name.to_string(),
        ty: FieldType::Json,
        required,
        unique: false,
    }
}

fn number(name: &'static str, required: bool) -> Field {
    Field {
        name: name.to_string(),
        ty: FieldType::Number {
            min: None,
            max: None,
        },
        required,
        unique: false,
    }
}

fn boolean(name: &'static str, required: bool) -> Field {
    Field {
        name: name.to_string(),
        ty: FieldType::Bool,
        required,
        unique: false,
    }
}

fn relation(name: &'static str, target: &'static str, required: bool) -> Field {
    Field {
        name: name.to_string(),
        ty: FieldType::Relation {
            target: CollectionId::from(target),
            cascade_delete: false,
        },
        required,
        unique: false,
    }
}

/// Reserved documentation site/catalog input. A site is stored in the
/// Workspace database, while its effective serving values are copied into
/// each version App manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteCatalogInput {
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default_app_id: Option<String>,
    #[serde(default = "default_language_code")]
    pub default_language_code: String,
    #[serde(default = "default_access_mode")]
    pub access_mode: String,
    #[serde(default)]
    pub canonical_url: Option<String>,
    #[serde(default)]
    pub theme: Option<Json>,
}

fn default_language_code() -> String {
    "en".into()
}

fn default_access_mode() -> String {
    "public".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteCatalogConfig {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_language_code")]
    pub default_language_code: String,
    #[serde(default = "default_access_mode")]
    pub access_mode: String,
    #[serde(default)]
    pub canonical_url: Option<String>,
    #[serde(default = "default_theme")]
    pub theme: Json,
}

fn default_theme() -> Json {
    json!({})
}

impl SiteCatalogInput {
    fn validate(&self) -> Result<()> {
        validate_slug(&self.slug, "site slug")?;
        if self.name.trim().is_empty() {
            return Err(DbError::InvalidIdentifier(
                "site name must not be empty".into(),
            ));
        }
        validate_language_code(&self.default_language_code)?;
        validate_access_mode(&self.access_mode)?;
        if let Some(url) = &self.canonical_url
            && !url.is_empty()
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(DbError::InvalidIdentifier(
                "canonical_url must use http:// or https://".into(),
            ));
        }
        Ok(())
    }

    fn config(&self) -> Json {
        serde_json::to_value(SiteCatalogConfig {
            description: self.description.clone(),
            default_language_code: self.default_language_code.clone(),
            access_mode: self.access_mode.clone(),
            canonical_url: self.canonical_url.clone(),
            theme: self.theme.clone().unwrap_or_else(|| json!({})),
        })
        .unwrap_or_else(|_| json!({}))
    }
}

/// Create or update the single site catalog record in a Workspace database.
pub async fn upsert_site_catalog(pool: &SqlitePool, input: SiteCatalogInput) -> Result<Record> {
    input.validate()?;
    let schema = collection_schema(pool, "docs_site").await?;
    if let Some(default_app_id) = input.default_app_id.as_deref() {
        let versions_schema = collection_schema(pool, "docs_versions").await?;
        let filter = FilterNode::Eq("app_id".into(), Json::String(default_app_id.to_string()));
        let versions = list_records(
            pool,
            &versions_schema,
            crate::records::ListPage {
                page: 1,
                per_page: 1,
            },
            Some(&filter),
        )
        .await?;
        if versions.total_items == 0 {
            return Err(DbError::Invariant(
                "site default_app_id is not present in the version catalog".into(),
            ));
        }
    }
    let existing = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        None,
    )
    .await?;
    let config = input.config();
    let fields = BTreeMap::from([
        ("name".into(), Json::String(input.name)),
        ("slug".into(), Json::String(input.slug)),
        ("config".into(), config),
        (
            "default_app_id".into(),
            input.default_app_id.map_or(Json::Null, Json::String),
        ),
        ("active_release_id".into(), Json::Null),
        ("manifest_revision".into(), json!(1.0)),
    ]);
    match existing.items.into_iter().next() {
        Some(record) => {
            let mut patch = fields;
            // An update without a release change must preserve the active
            // pointer and monotonically advance the manifest revision.
            patch.insert(
                "active_release_id".into(),
                record
                    .fields
                    .get("active_release_id")
                    .cloned()
                    .unwrap_or(Json::Null),
            );
            let revision = number_value(&record, "manifest_revision").unwrap_or(0.0) + 1.0;
            patch.insert("manifest_revision".into(), json!(revision));
            update_record(pool, &schema, record.id.as_str(), patch).await
        }
        None => create_record(pool, &schema, fields).await,
    }
}

/// Return the site catalog. More than one row indicates storage corruption;
/// callers must not silently choose an arbitrary site record.
pub async fn find_site_catalog(pool: &SqlitePool) -> Result<Option<Record>> {
    let schema = collection_schema(pool, "docs_site").await?;
    let listed = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        None,
    )
    .await?;
    if listed.total_items > 1 {
        return Err(DbError::Invariant(
            "docs_site must contain one catalog record".into(),
        ));
    }
    Ok(listed.items.into_iter().next())
}

pub async fn rename_site_catalog(pool: &SqlitePool, name: &str) -> Result<Option<Record>> {
    if name.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "site name must not be empty".into(),
        ));
    }
    let schema = collection_schema(pool, "docs_site").await?;
    let Some(record) = find_site_catalog(pool).await? else {
        return Ok(None);
    };
    update_record(
        pool,
        &schema,
        record.id.as_str(),
        BTreeMap::from([("name".into(), Json::String(name.to_string()))]),
    )
    .await
    .map(Some)
}

pub async fn site_catalog_config(record: &Record) -> Result<SiteCatalogConfig> {
    let value = record
        .fields
        .get("config")
        .cloned()
        .filter(|value| !value.is_null())
        .unwrap_or_else(|| json!({}));
    serde_json::from_value(value)
        .map_err(|e| DbError::Invariant(format!("invalid site catalog config: {e}")))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionCatalogInput {
    pub app_id: String,
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub source_branch_id: Option<String>,
    #[serde(default)]
    pub config: Option<Json>,
}

fn default_enabled() -> bool {
    true
}

pub async fn upsert_version_catalog(
    pool: &SqlitePool,
    input: VersionCatalogInput,
) -> Result<Record> {
    validate_slug(&input.slug, "version slug")?;
    if input.app_id.trim().is_empty() || input.name.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "version app_id and name are required".into(),
        ));
    }
    let app_exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apps WHERE id = ?")
        .bind(&input.app_id)
        .fetch_one(pool)
        .await?;
    if app_exists == 0 {
        return Err(DbError::Invariant(
            "version app_id is not present in the Workspace apps table".into(),
        ));
    }
    if !input.position.is_finite() {
        return Err(DbError::InvalidIdentifier(
            "version position must be finite".into(),
        ));
    }
    let schema = collection_schema(pool, "docs_versions").await?;
    let filter = FilterNode::Eq("app_id".into(), Json::String(input.app_id.clone()));
    let existing = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        Some(&filter),
    )
    .await?;
    if existing.total_items > 1 {
        return Err(DbError::Invariant(format!(
            "multiple docs_versions rows for app '{}'",
            input.app_id
        )));
    }
    let site_default = find_site_catalog(pool).await?.and_then(|site| {
        site.fields
            .get("default_app_id")
            .and_then(Json::as_str)
            .map(str::to_string)
    });
    let is_default = input.is_default || site_default.as_deref() == Some(input.app_id.as_str());
    if is_default {
        clear_default_version_flags(pool, &schema, existing.items.first().map(|r| r.id.as_str()))
            .await?;
    }
    let fields = BTreeMap::from([
        ("app_id".into(), Json::String(input.app_id)),
        ("slug".into(), Json::String(input.slug)),
        ("name".into(), Json::String(input.name)),
        ("is_default".into(), Json::Bool(is_default)),
        ("enabled".into(), Json::Bool(input.enabled)),
        ("position".into(), json!(input.position)),
        (
            "source_branch_id".into(),
            input.source_branch_id.map_or(Json::Null, Json::String),
        ),
        ("config".into(), input.config.unwrap_or_else(|| json!({}))),
    ]);
    match existing.items.into_iter().next() {
        Some(record) => update_record(pool, &schema, record.id.as_str(), fields).await,
        None => create_record(pool, &schema, fields).await,
    }
}

async fn clear_default_version_flags(
    pool: &SqlitePool,
    schema: &Schema,
    except: Option<&str>,
) -> Result<()> {
    let all = list_records(
        pool,
        schema,
        crate::records::ListPage {
            page: 1,
            per_page: 200,
        },
        None,
    )
    .await?;
    for record in all.items {
        if Some(record.id.as_str()) != except
            && record.fields.get("is_default") == Some(&Json::Bool(true))
        {
            update_record(
                pool,
                schema,
                record.id.as_str(),
                BTreeMap::from([("is_default".into(), Json::Bool(false))]),
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn list_version_catalog(pool: &SqlitePool) -> Result<Vec<Record>> {
    let schema = collection_schema(pool, "docs_versions").await?;
    let listed = list_records_ordered(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 200,
        },
        None,
        "position",
        false,
    )
    .await?;
    Ok(listed.items)
}

pub async fn rename_version_catalog(
    pool: &SqlitePool,
    app_id: &str,
    name: &str,
) -> Result<Option<Record>> {
    if name.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "version name must not be empty".into(),
        ));
    }
    let schema = collection_schema(pool, "docs_versions").await?;
    let filter = FilterNode::Eq("app_id".into(), Json::String(app_id.to_string()));
    let listed = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        Some(&filter),
    )
    .await?;
    let Some(record) = listed.items.into_iter().next() else {
        return Ok(None);
    };
    update_record(
        pool,
        &schema,
        record.id.as_str(),
        BTreeMap::from([("name".into(), Json::String(name.to_string()))]),
    )
    .await
    .map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestLanguage {
    pub code: String,
    pub label: String,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifest {
    pub site_slug: String,
    pub site_name: String,
    pub version_slug: String,
    pub version_name: String,
    pub default_language_code: String,
    pub languages: Vec<ManifestLanguage>,
    pub access_mode: String,
    pub renderer_version: String,
    pub snapshot_schema_version: u32,
    pub canonical_url: Option<String>,
    pub public_path: String,
    pub theme: Json,
    pub seo: Json,
    pub source_workspace_revision: u64,
    pub manifest_revision: u64,
}

impl VersionManifest {
    pub fn validate(&self) -> Result<()> {
        validate_slug(&self.site_slug, "manifest site slug")?;
        validate_slug(&self.version_slug, "manifest version slug")?;
        if self.site_name.trim().is_empty() || self.version_name.trim().is_empty() {
            return Err(DbError::InvalidIdentifier(
                "manifest names must not be empty".into(),
            ));
        }
        validate_access_mode(&self.access_mode)?;
        validate_language_code(&self.default_language_code)?;
        if self.renderer_version.trim().is_empty() || self.snapshot_schema_version == 0 {
            return Err(DbError::InvalidIdentifier(
                "manifest renderer/schema version is required".into(),
            ));
        }
        if self.languages.is_empty() {
            return Err(DbError::InvalidIdentifier(
                "manifest must contain a language".into(),
            ));
        }
        let mut seen = HashSet::new();
        for language in &self.languages {
            validate_language_code(&language.code)?;
            if !seen.insert(language.code.clone()) {
                return Err(DbError::Invariant(
                    "manifest contains duplicate languages".into(),
                ));
            }
            if !matches!(language.direction.as_str(), "ltr" | "rtl") {
                return Err(DbError::InvalidIdentifier(
                    "language direction must be ltr or rtl".into(),
                ));
            }
        }
        if !seen.contains(&self.default_language_code) {
            return Err(DbError::Invariant(
                "manifest default language is not enabled".into(),
            ));
        }
        Ok(())
    }
}

pub async fn upsert_version_manifest(
    pool: &SqlitePool,
    mut manifest: VersionManifest,
) -> Result<Record> {
    manifest.validate()?;
    let schema = collection_schema(pool, "docs_version_meta").await?;
    let existing = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        None,
    )
    .await?;
    if existing.total_items > 1 {
        return Err(DbError::Invariant(
            "docs_version_meta must contain one manifest record".into(),
        ));
    }
    if let Some(record) = existing.items.first() {
        let current = number_value(record, "manifest_revision")
            .unwrap_or(manifest.manifest_revision as f64) as u64;
        manifest.manifest_revision = current.saturating_add(1).max(manifest.manifest_revision);
    }
    let fields = BTreeMap::from([
        ("site_slug".into(), Json::String(manifest.site_slug.clone())),
        (
            "version_slug".into(),
            Json::String(manifest.version_slug.clone()),
        ),
        (
            "version_name".into(),
            Json::String(manifest.version_name.clone()),
        ),
        (
            "manifest".into(),
            serde_json::to_value(&manifest)
                .map_err(|e| DbError::Invariant(format!("manifest serialization: {e}")))?,
        ),
        (
            "renderer_version".into(),
            Json::String(manifest.renderer_version),
        ),
        (
            "active_deployment_id".into(),
            existing
                .items
                .first()
                .and_then(|r| r.fields.get("active_deployment_id"))
                .cloned()
                .unwrap_or(Json::Null),
        ),
        (
            "manifest_revision".into(),
            json!(manifest.manifest_revision as f64),
        ),
    ]);
    match existing.items.into_iter().next() {
        Some(record) => update_record(pool, &schema, record.id.as_str(), fields).await,
        None => create_record(pool, &schema, fields).await,
    }
}

pub async fn find_version_manifest(pool: &SqlitePool) -> Result<(Record, VersionManifest)> {
    let schema = collection_schema(pool, "docs_version_meta").await?;
    let listed = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        None,
    )
    .await?;
    if listed.total_items > 1 {
        return Err(DbError::Invariant(
            "docs_version_meta must contain one manifest record".into(),
        ));
    }
    let record = listed
        .items
        .into_iter()
        .next()
        .ok_or_else(|| DbError::Invariant("version manifest is not initialized".into()))?;
    let value = record
        .fields
        .get("manifest")
        .cloned()
        .ok_or_else(|| DbError::Invariant("version manifest payload is missing".into()))?;
    let manifest: VersionManifest = serde_json::from_value(value)
        .map_err(|e| DbError::Invariant(format!("invalid version manifest: {e}")))?;
    manifest.validate()?;
    Ok((record, manifest))
}

pub async fn rename_version_manifest(pool: &SqlitePool, name: &str) -> Result<Record> {
    let (_, mut manifest) = find_version_manifest(pool).await?;
    manifest.version_name = name.to_string();
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await
}

pub async fn sync_version_manifest_name(pool: &SqlitePool, name: &str) -> Result<Option<Record>> {
    if name.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "version name must not be empty".into(),
        ));
    }
    let (record, mut manifest) = match find_version_manifest(pool).await {
        Ok(value) => value,
        Err(DbError::Invariant(message)) if message == "version manifest is not initialized" => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if manifest.version_name == name {
        return Ok(Some(record));
    }
    manifest.version_name = name.to_string();
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await.map(Some)
}

pub async fn sync_version_manifest_site_identity(
    pool: &SqlitePool,
    slug: &str,
    name: &str,
) -> Result<Option<Record>> {
    validate_slug(slug, "site slug")?;
    if name.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "site name must not be empty".into(),
        ));
    }
    let (record, mut manifest) = match find_version_manifest(pool).await {
        Ok(value) => value,
        Err(DbError::Invariant(message)) if message == "version manifest is not initialized" => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if manifest.site_slug == slug && manifest.site_name == name {
        return Ok(Some(record));
    }
    manifest.site_slug = slug.to_string();
    manifest.site_name = name.to_string();
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await.map(Some)
}

pub async fn rename_version_site(pool: &SqlitePool, slug: &str, name: &str) -> Result<Record> {
    validate_slug(slug, "site slug")?;
    let (_, mut manifest) = find_version_manifest(pool).await?;
    manifest.site_slug = slug.to_string();
    manifest.site_name = name.to_string();
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await
}

pub async fn sync_version_manifest_identity(
    pool: &SqlitePool,
    version_slug: &str,
    version_name: &str,
) -> Result<Option<Record>> {
    validate_slug(version_slug, "version slug")?;
    if version_name.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "version name must not be empty".into(),
        ));
    }
    let (record, mut manifest) = match find_version_manifest(pool).await {
        Ok(value) => value,
        Err(DbError::Invariant(message)) if message == "version manifest is not initialized" => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if manifest.version_slug == version_slug && manifest.version_name == version_name {
        return Ok(Some(record));
    }
    manifest.version_slug = version_slug.to_string();
    manifest.version_name = version_name.to_string();
    manifest.public_path = format!("/{version_slug}");
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await.map(Some)
}

pub async fn sync_version_manifest_site_config(
    pool: &SqlitePool,
    site_slug: &str,
    site_name: &str,
    site_config: &SiteCatalogConfig,
) -> Result<Option<Record>> {
    validate_slug(site_slug, "site slug")?;
    if site_name.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "site name must not be empty".into(),
        ));
    }
    let (record, mut manifest) = match find_version_manifest(pool).await {
        Ok(value) => value,
        Err(DbError::Invariant(message)) if message == "version manifest is not initialized" => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let language_codes: HashSet<&str> = manifest
        .languages
        .iter()
        .map(|language| language.code.as_str())
        .collect();
    let desired_default = if language_codes.contains(site_config.default_language_code.as_str()) {
        site_config.default_language_code.clone()
    } else {
        manifest.default_language_code.clone()
    };
    let changed = manifest.site_slug != site_slug
        || manifest.site_name != site_name
        || manifest.access_mode != site_config.access_mode
        || manifest.default_language_code != desired_default
        || manifest.canonical_url != site_config.canonical_url
        || manifest.theme != site_config.theme;
    if !changed {
        return Ok(Some(record));
    }
    manifest.site_slug = site_slug.to_string();
    manifest.site_name = site_name.to_string();
    manifest.access_mode = site_config.access_mode.clone();
    manifest.default_language_code = desired_default;
    manifest.canonical_url = site_config.canonical_url.clone();
    manifest.theme = site_config.theme.clone();
    manifest.source_workspace_revision = manifest.source_workspace_revision.saturating_add(1);
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await.map(Some)
}

/// Advance the App-local source revision after a draft-affecting mutation.
/// The effective manifest is copied into snapshots, so this revision lets a
/// publish commit reject a snapshot built before the latest draft change.
pub async fn touch_version_manifest(pool: &SqlitePool) -> Result<Record> {
    let (_, mut manifest) = find_version_manifest(pool).await?;
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await
}

pub async fn set_active_deployment(
    pool: &SqlitePool,
    manifest_id: &str,
    deployment_id: Option<&str>,
    revision: u64,
) -> Result<Record> {
    let schema = collection_schema(pool, "docs_version_meta").await?;
    update_record(
        pool,
        &schema,
        manifest_id,
        BTreeMap::from([
            (
                "active_deployment_id".into(),
                deployment_id.map_or(Json::Null, |id| Json::String(id.to_string())),
            ),
            ("manifest_revision".into(), json!(revision as f64)),
        ]),
    )
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageInput {
    pub code: String,
    pub label: String,
    pub direction: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub config: Option<Json>,
}

pub async fn upsert_language(pool: &SqlitePool, input: LanguageInput) -> Result<Record> {
    validate_language_code(&input.code)?;
    if input.label.trim().is_empty() || !matches!(input.direction.as_str(), "ltr" | "rtl") {
        return Err(DbError::InvalidIdentifier(
            "language label and direction are invalid".into(),
        ));
    }
    if !input.position.is_finite() {
        return Err(DbError::InvalidIdentifier(
            "language position must be finite".into(),
        ));
    }
    if input.is_default && !input.enabled {
        return Err(DbError::Invariant(
            "a disabled language cannot be default".into(),
        ));
    }
    let schema = collection_schema(pool, "docs_languages").await?;
    let filter = FilterNode::Eq("code".into(), Json::String(input.code.clone()));
    let existing = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        Some(&filter),
    )
    .await?;
    if existing.total_items > 1 {
        return Err(DbError::Invariant(format!(
            "multiple language rows for '{}'",
            input.code
        )));
    }
    if input.is_default {
        clear_default_language_flags(pool, &schema, existing.items.first().map(|r| r.id.as_str()))
            .await?;
    }
    let fields = BTreeMap::from([
        ("code".into(), Json::String(input.code)),
        ("label".into(), Json::String(input.label)),
        ("direction".into(), Json::String(input.direction)),
        ("is_default".into(), Json::Bool(input.is_default)),
        ("enabled".into(), Json::Bool(input.enabled)),
        ("position".into(), json!(input.position)),
        ("config".into(), input.config.unwrap_or_else(|| json!({}))),
    ]);
    match existing.items.into_iter().next() {
        Some(record) => update_record(pool, &schema, record.id.as_str(), fields).await,
        None => create_record(pool, &schema, fields).await,
    }
}

async fn clear_default_language_flags(
    pool: &SqlitePool,
    schema: &Schema,
    except: Option<&str>,
) -> Result<()> {
    let all = list_records(
        pool,
        schema,
        crate::records::ListPage {
            page: 1,
            per_page: 200,
        },
        None,
    )
    .await?;
    for record in all.items {
        if Some(record.id.as_str()) != except
            && record.fields.get("is_default") == Some(&Json::Bool(true))
        {
            update_record(
                pool,
                schema,
                record.id.as_str(),
                BTreeMap::from([("is_default".into(), Json::Bool(false))]),
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn list_languages(pool: &SqlitePool) -> Result<Vec<Record>> {
    let schema = collection_schema(pool, "docs_languages").await?;
    Ok(list_records_ordered(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 200,
        },
        None,
        "position",
        false,
    )
    .await?
    .items)
}

pub async fn refresh_version_manifest_languages(pool: &SqlitePool) -> Result<Record> {
    let (_, mut manifest) = find_version_manifest(pool).await?;
    let languages = list_languages(pool).await?;
    let enabled = languages
        .iter()
        .filter(|record| {
            record
                .fields
                .get("enabled")
                .and_then(Json::as_bool)
                .unwrap_or(false)
        })
        .map(|record| {
            Ok(ManifestLanguage {
                code: required_text(record, "code")?,
                label: required_text(record, "label")?,
                direction: required_text(record, "direction")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if enabled.is_empty() {
        return Err(DbError::Invariant(
            "at least one enabled language is required".into(),
        ));
    }
    if !enabled
        .iter()
        .any(|language| language.code == manifest.default_language_code)
    {
        manifest.default_language_code = enabled[0].code.clone();
    }
    manifest.languages = enabled;
    manifest.manifest_revision = manifest.manifest_revision.saturating_add(1);
    upsert_version_manifest(pool, manifest).await
}

pub async fn set_site_default_app(pool: &SqlitePool, app_id: &str) -> Result<Record> {
    let schema = collection_schema(pool, "docs_site").await?;
    let site = find_site_catalog(pool)
        .await?
        .ok_or_else(|| DbError::Invariant("site catalog is not initialized".into()))?;
    update_record(
        pool,
        &schema,
        site.id.as_str(),
        BTreeMap::from([("default_app_id".into(), Json::String(app_id.to_string()))]),
    )
    .await
}

/// Seed an App-local effective manifest from the Workspace catalog. Once
/// created, serving and publishing use this copy; callers must explicitly
/// regenerate it when site defaults change.
pub async fn bootstrap_site_version_metadata(
    workspace_pool: &SqlitePool,
    app_pool: &SqlitePool,
    site_slug: &str,
    site_name: &str,
    app_id: &str,
    app_name: &str,
) -> Result<()> {
    let site = match find_site_catalog(workspace_pool).await? {
        Some(site) => site,
        None => {
            upsert_site_catalog(
                workspace_pool,
                SiteCatalogInput {
                    name: site_name.to_string(),
                    slug: site_slug.to_string(),
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
    let effective_site_slug = site
        .fields
        .get("slug")
        .and_then(Json::as_str)
        .unwrap_or(site_slug)
        .to_string();
    let effective_site_name = site
        .fields
        .get("name")
        .and_then(Json::as_str)
        .unwrap_or(site_name)
        .to_string();
    let versions = list_version_catalog(workspace_pool).await?;
    let existing_version = versions
        .iter()
        .find(|record| record.fields.get("app_id").and_then(Json::as_str) == Some(app_id));
    let catalog_has_default = versions.iter().any(|record| {
        record
            .fields
            .get("is_default")
            .and_then(Json::as_bool)
            .unwrap_or(false)
    });
    let first_catalog_app = versions
        .first()
        .and_then(|record| record.fields.get("app_id"))
        .and_then(Json::as_str);
    let is_default = existing_version
        .and_then(|record| record.fields.get("is_default").and_then(Json::as_bool))
        .unwrap_or(false)
        || (!catalog_has_default && (versions.is_empty() || first_catalog_app == Some(app_id)));
    let version_slug = existing_version
        .and_then(|record| record.fields.get("slug").and_then(Json::as_str))
        .unwrap_or(app_id)
        .to_string();
    let version_name = existing_version
        .and_then(|record| record.fields.get("name").and_then(Json::as_str))
        .unwrap_or(app_name)
        .to_string();
    let enabled = existing_version
        .and_then(|record| record.fields.get("enabled").and_then(Json::as_bool))
        .unwrap_or(true);
    let position = existing_version
        .and_then(|record| number_value(record, "position"))
        .unwrap_or(versions.len() as f64);
    let source_branch_id =
        existing_version.and_then(|record| optional_text(record, "source_branch_id"));
    let config = existing_version.and_then(|record| record.fields.get("config").cloned());
    upsert_version_catalog(
        workspace_pool,
        VersionCatalogInput {
            app_id: app_id.to_string(),
            slug: version_slug.clone(),
            name: version_name.clone(),
            is_default,
            enabled,
            position,
            source_branch_id,
            config,
        },
    )
    .await?;
    if is_default {
        set_site_default_app(workspace_pool, app_id).await?;
    }
    let config = site_catalog_config(&site).await?;
    ensure_version_manifest(
        app_pool,
        &effective_site_slug,
        &effective_site_name,
        &version_slug,
        &version_name,
        &config,
    )
    .await?;
    // Existing manifests may predate the Workspace catalog or carry stale
    // effective metadata. Reconcile only the App-local copy; this does not
    // touch the active deployment pointer or any immutable snapshot.
    sync_version_manifest_identity(app_pool, &version_slug, &version_name).await?;
    sync_version_manifest_site_config(
        app_pool,
        &effective_site_slug,
        &effective_site_name,
        &config,
    )
    .await?;
    Ok(())
}

pub async fn ensure_version_manifest(
    pool: &SqlitePool,
    site_slug: &str,
    site_name: &str,
    version_slug: &str,
    version_name: &str,
    site_config: &SiteCatalogConfig,
) -> Result<Record> {
    match find_version_manifest(pool).await {
        Ok((record, _)) => return Ok(record),
        Err(DbError::Invariant(message)) if message == "version manifest is not initialized" => {}
        Err(error) => return Err(error),
    }
    let languages = list_languages(pool).await?;
    let manifest_languages = languages
        .iter()
        .map(|record| {
            Ok(ManifestLanguage {
                code: required_text(record, "code")?,
                label: required_text(record, "label")?,
                direction: required_text(record, "direction")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let default_language_code = if manifest_languages
        .iter()
        .any(|language| language.code == site_config.default_language_code)
    {
        site_config.default_language_code.clone()
    } else {
        manifest_languages
            .first()
            .map(|language| language.code.clone())
            .ok_or_else(|| {
                DbError::Invariant(
                    "at least one language is required for a version manifest".into(),
                )
            })?
    };
    let manifest = VersionManifest {
        site_slug: site_slug.to_string(),
        site_name: site_name.to_string(),
        version_slug: version_slug.to_string(),
        version_name: version_name.to_string(),
        default_language_code,
        languages: manifest_languages,
        access_mode: site_config.access_mode.clone(),
        renderer_version: "rustbase-markdown-v1".into(),
        snapshot_schema_version: 1,
        canonical_url: site_config.canonical_url.clone(),
        public_path: format!("/{version_slug}"),
        theme: site_config.theme.clone(),
        seo: json!({}),
        source_workspace_revision: 1,
        manifest_revision: 1,
    };
    upsert_version_manifest(pool, manifest).await
}

async fn collection_schema(pool: &SqlitePool, id: &str) -> Result<Schema> {
    find_collection(pool, id)
        .await?
        .map(|collection| collection.schema)
        .ok_or_else(|| {
            DbError::Invariant(format!(
                "reserved docs collection '{id}' is not bootstrapped"
            ))
        })
}

fn validate_slug(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 80
        || value.starts_with('-')
        || value.ends_with('-')
        || !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(DbError::InvalidIdentifier(format!(
            "{label} must be a lowercase slug"
        )));
    }
    Ok(())
}

fn validate_language_code(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 35
        || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(DbError::InvalidIdentifier(
            "language code must be BCP-47-like".into(),
        ));
    }
    Ok(())
}

fn validate_access_mode(value: &str) -> Result<()> {
    if matches!(value, "public" | "private") {
        Ok(())
    } else {
        Err(DbError::InvalidIdentifier(
            "access_mode must be public or private".into(),
        ))
    }
}

fn number_value(record: &Record, field: &str) -> Option<f64> {
    record.fields.get(field).and_then(Json::as_f64)
}

/// Immutable representation served to readers. It intentionally contains
/// the effective manifest and all page records needed by one App, so a
/// version package never has to read a Workspace catalog at request time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionSnapshot {
    pub snapshot_schema_version: u32,
    pub renderer_version: String,
    pub manifest: VersionManifest,
    pub pages: Vec<Record>,
    pub reader_grants: Vec<ReaderGrant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReaderGrant {
    pub subject: String,
    pub audience: Option<String>,
    pub path_prefix: String,
    pub effect: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PublishPreparation {
    pub deployment: Record,
    pub job: Record,
    pub snapshot: VersionSnapshot,
    pub snapshot_bytes: Vec<u8>,
    pub checksum: String,
    pub source_revision: String,
    pub reused: bool,
    pub in_progress: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentSummary {
    pub id: String,
    pub status: String,
    pub checksum: Option<String>,
    pub snapshot_key: Option<String>,
    pub source_revision: Option<String>,
}

/// Validate and serialize one App's complete published view. No Workspace
/// or other App database is consulted here.
pub async fn build_version_snapshot(
    pool: &SqlitePool,
) -> Result<(VersionSnapshot, Vec<u8>, String)> {
    let (_, manifest) = find_version_manifest(pool).await?;
    let page_schema = collection_schema(pool, "docs_pages").await?;
    let pages = list_all_records(pool, &page_schema, "position").await?;
    for page in &pages {
        validate_publish_page(page)?;
    }
    validate_publish_tree(pool, &page_schema, &pages).await?;
    let grant_schema = collection_schema(pool, "docs_reader_grants").await?;
    let grant_records = list_all_records(pool, &grant_schema, "created_at").await?;
    let reader_grants = grant_records
        .iter()
        .filter(|record| {
            record
                .fields
                .get("enabled")
                .and_then(Json::as_bool)
                .unwrap_or(false)
        })
        .map(record_to_reader_grant)
        .collect::<Result<Vec<_>>>()?;
    let snapshot = VersionSnapshot {
        snapshot_schema_version: manifest.snapshot_schema_version,
        renderer_version: manifest.renderer_version.clone(),
        manifest,
        pages,
        reader_grants,
    };
    let bytes = serde_json::to_vec(&snapshot)
        .map_err(|e| DbError::Invariant(format!("snapshot serialization failed: {e}")))?;
    let checksum = sha256_hex(&bytes);
    Ok((snapshot, bytes, checksum))
}

async fn existing_publish_preparation(
    pool: &SqlitePool,
    job: Record,
    snapshot: VersionSnapshot,
    snapshot_bytes: Vec<u8>,
    checksum: String,
) -> Result<PublishPreparation> {
    let deployment_id = job
        .fields
        .get("payload")
        .and_then(|value| value.get("deployment_id"))
        .and_then(Json::as_str)
        .ok_or_else(|| DbError::Invariant("publish job payload has no deployment_id".into()))?;
    let deployment = find_deployment(pool, deployment_id)
        .await?
        .ok_or_else(|| DbError::Invariant("publish job points to missing deployment".into()))?;
    let reused = deployment.fields.get("status").and_then(Json::as_str) == Some("ready");
    let in_progress = !reused
        && matches!(
            job.fields.get("status").and_then(Json::as_str),
            Some("queued" | "running")
        );
    Ok(PublishPreparation {
        deployment,
        job,
        snapshot,
        snapshot_bytes,
        checksum: checksum.clone(),
        source_revision: checksum,
        reused,
        in_progress,
    })
}

fn is_unique_violation(error: &DbError) -> bool {
    matches!(
        error,
        DbError::Sqlx(sqlx::Error::Database(database))
            if database.message().to_ascii_lowercase().contains("unique")
    )
}

pub async fn prepare_publish(
    pool: &SqlitePool,
    requested_by: Option<&str>,
    idempotency_key: &str,
) -> Result<PublishPreparation> {
    if idempotency_key.trim().is_empty() || idempotency_key.len() > 200 {
        return Err(DbError::InvalidIdentifier(
            "publish idempotency_key is required".into(),
        ));
    }
    let (_, manifest) = find_version_manifest(pool).await?;
    let (snapshot, snapshot_bytes, checksum) = build_version_snapshot(pool).await?;
    let jobs_schema = collection_schema(pool, "docs_jobs").await?;
    let existing_job_filter = FilterNode::Eq(
        "idempotency_key".into(),
        Json::String(idempotency_key.to_string()),
    );
    let existing_jobs = list_records(
        pool,
        &jobs_schema,
        crate::records::ListPage {
            page: 1,
            per_page: 2,
        },
        Some(&existing_job_filter),
    )
    .await?;
    if let Some(job) = existing_jobs.items.into_iter().next() {
        return existing_publish_preparation(pool, job, snapshot, snapshot_bytes, checksum).await;
    }

    let deployments_schema = collection_schema(pool, "docs_deployments").await?;
    let jobs_schema = collection_schema(pool, "docs_jobs").await?;
    let mut tx = pool.begin().await?;
    let deployment = create_record_in_tx(
        &mut tx,
        &deployments_schema,
        BTreeMap::from([
            ("version".into(), json!(manifest.manifest_revision as f64)),
            ("status".into(), Json::String("pending".into())),
            ("snapshot_key".into(), Json::Null),
            ("checksum".into(), Json::String(checksum.clone())),
            ("snapshot_size".into(), Json::Null),
            ("source_revision".into(), Json::String(checksum.clone())),
            (
                "renderer_version".into(),
                Json::String(manifest.renderer_version.clone()),
            ),
            ("pages_count".into(), json!(snapshot.pages.len() as f64)),
            (
                "created_by".into(),
                requested_by.map_or(Json::Null, |v| Json::String(v.to_string())),
            ),
            ("error".into(), Json::Null),
            ("error_details".into(), Json::Null),
            ("completed_at".into(), Json::Null),
        ]),
    )
    .await?;
    let job = match create_record_in_tx(
        &mut tx,
        &jobs_schema,
        BTreeMap::from([
            ("kind".into(), Json::String("publish".into())),
            ("status".into(), Json::String("queued".into())),
            (
                "payload".into(),
                json!({
                    "deployment_id": deployment.id.as_str(),
                    "source_revision": checksum.clone(),
                    "manifest_revision": manifest.manifest_revision
                }),
            ),
            (
                "idempotency_key".into(),
                Json::String(idempotency_key.to_string()),
            ),
            ("attempts".into(), json!(0.0)),
            (
                "run_after".into(),
                Json::String(chrono::Utc::now().to_rfc3339()),
            ),
            ("lease_owner".into(), Json::Null),
            ("lease_until".into(), Json::Null),
            ("last_error".into(), Json::Null),
            ("started_at".into(), Json::Null),
            ("completed_at".into(), Json::Null),
        ]),
    )
    .await
    {
        Ok(job) => job,
        Err(error) if is_unique_violation(&error) => {
            tx.rollback().await?;
            let existing_jobs = list_records(
                pool,
                &jobs_schema,
                crate::records::ListPage {
                    page: 1,
                    per_page: 2,
                },
                Some(&existing_job_filter),
            )
            .await?;
            if let Some(job) = existing_jobs.items.into_iter().next() {
                return existing_publish_preparation(pool, job, snapshot, snapshot_bytes, checksum)
                    .await;
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    tx.commit().await?;
    Ok(PublishPreparation {
        deployment,
        job,
        snapshot,
        snapshot_bytes,
        checksum: checksum.clone(),
        source_revision: checksum,
        reused: false,
        in_progress: false,
    })
}

pub async fn mark_deployment_building(pool: &SqlitePool, deployment_id: &str) -> Result<Record> {
    let schema = collection_schema(pool, "docs_deployments").await?;
    let deployment = find_record(pool, &schema, deployment_id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;
    let status = deployment
        .fields
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if status == "ready" {
        return Ok(deployment);
    }
    if !matches!(status, "pending" | "building" | "failed") {
        return Err(DbError::Invariant(format!(
            "deployment cannot enter building from '{status}'"
        )));
    }
    update_record(
        pool,
        &schema,
        deployment_id,
        BTreeMap::from([
            ("status".into(), Json::String("building".into())),
            ("error".into(), Json::Null),
            ("error_details".into(), Json::Null),
            ("completed_at".into(), Json::Null),
        ]),
    )
    .await
}

/// Commit a complete immutable snapshot and switch the App-local active
/// pointer. The expected manifest revision prevents a stale publish from
/// becoming public after draft/config changes.
pub async fn commit_ready(
    pool: &SqlitePool,
    deployment_id: &str,
    snapshot_key: &str,
    checksum: &str,
    snapshot_size: u64,
    expected_manifest_revision: u64,
) -> Result<Record> {
    if snapshot_key.trim().is_empty() || checksum.trim().is_empty() {
        return Err(DbError::InvalidIdentifier(
            "ready deployment needs snapshot key and checksum".into(),
        ));
    }
    let (manifest_record, manifest) = find_version_manifest(pool).await?;
    if manifest.manifest_revision != expected_manifest_revision {
        return Err(DbError::Conflict(
            "publish source is stale; manifest changed".into(),
        ));
    }
    let deployment_schema = collection_schema(pool, "docs_deployments").await?;
    let deployment = find_record(pool, &deployment_schema, deployment_id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;
    let status = deployment
        .fields
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if !matches!(status, "pending" | "building") {
        if status == "ready"
            && deployment.fields.get("checksum").and_then(Json::as_str) == Some(checksum)
            && deployment.fields.get("snapshot_key").and_then(Json::as_str) == Some(snapshot_key)
        {
            return Ok(deployment);
        }
        return Err(DbError::Invariant(format!(
            "deployment cannot become ready from '{status}'"
        )));
    }
    if deployment.fields.get("checksum").and_then(Json::as_str) != Some(checksum) {
        return Err(DbError::Invariant(
            "deployment checksum does not match prepared snapshot".into(),
        ));
    }
    let source_revision = deployment
        .fields
        .get("source_revision")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if !source_revision.starts_with("rollback:") {
        let (_, _, current_checksum) = build_version_snapshot(pool).await?;
        if current_checksum != checksum {
            return Err(DbError::Conflict(
                "publish source changed while the snapshot was being written".into(),
            ));
        }
    }
    let previous = manifest_record
        .fields
        .get("active_deployment_id")
        .and_then(Json::as_str)
        .map(str::to_string);
    let now = chrono::Utc::now();
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE \"docs_deployments\" SET status = ?, snapshot_key = ?, checksum = ?, snapshot_size = ?, completed_at = ?, error = ?, error_details = ?, updated_at = ? WHERE id = ? AND checksum = ? AND status IN ('pending', 'building')",
    )
    .bind("ready")
    .bind(snapshot_key)
    .bind(checksum)
    .bind(snapshot_size as f64)
    .bind(now.to_rfc3339())
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .bind(now)
    .bind(deployment_id)
    .bind(checksum)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(DbError::Conflict(
            "deployment changed while being committed".into(),
        ));
    }
    let pointer = sqlx::query(
        "UPDATE \"docs_version_meta\" SET active_deployment_id = ?, updated_at = ? WHERE id = ? AND manifest_revision = ?",
    )
    .bind(deployment_id)
    .bind(now)
    .bind(manifest_record.id.as_str())
    .bind(expected_manifest_revision as f64)
    .execute(&mut *tx)
    .await?;
    if pointer.rows_affected() != 1 {
        return Err(DbError::Invariant(
            "version manifest disappeared while committing publish".into(),
        ));
    }
    if let Some(previous_id) = previous.as_deref().filter(|id| *id != deployment_id) {
        sqlx::query("UPDATE \"docs_deployments\" SET status = ?, updated_at = ? WHERE id = ?")
            .bind("superseded")
            .bind(now)
            .bind(previous_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    find_record(pool, &deployment_schema, deployment_id)
        .await?
        .ok_or_else(|| DbError::Invariant("ready deployment disappeared after commit".into()))
}

pub async fn mark_deployment_failed(
    pool: &SqlitePool,
    deployment_id: &str,
    error: &str,
) -> Result<Record> {
    let schema = collection_schema(pool, "docs_deployments").await?;
    let current = find_record(pool, &schema, deployment_id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;
    let status = current
        .fields
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if matches!(status, "ready" | "superseded") {
        return Err(DbError::Conflict(
            "completed deployments are immutable".into(),
        ));
    }
    let safe = error.chars().take(500).collect::<String>();
    update_record(
        pool,
        &schema,
        deployment_id,
        BTreeMap::from([
            ("status".into(), Json::String("failed".into())),
            ("error".into(), Json::String(safe.to_string())),
            ("error_details".into(), json!({"code":"publish_failed"})),
            (
                "completed_at".into(),
                Json::String(chrono::Utc::now().to_rfc3339()),
            ),
        ]),
    )
    .await
}

pub async fn finish_publish_job(
    pool: &SqlitePool,
    job_id: &str,
    success: bool,
    error: Option<&str>,
) -> Result<Record> {
    let schema = collection_schema(pool, "docs_jobs").await?;
    let current = find_record(pool, &schema, job_id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;
    if current.fields.get("kind").and_then(Json::as_str) != Some("publish") {
        return Err(DbError::Invariant(
            "only publish jobs can be completed by this service".into(),
        ));
    }
    let current_status = current
        .fields
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if matches!(current_status, "succeeded" | "failed") {
        return Ok(current);
    }
    if !matches!(current_status, "queued" | "running") {
        return Err(DbError::Conflict(format!(
            "publish job cannot finish from '{current_status}'"
        )));
    }
    let status = if success { "succeeded" } else { "failed" };
    let safe_error = error.map(|value| value.chars().take(500).collect::<String>());
    update_record(
        pool,
        &schema,
        job_id,
        BTreeMap::from([
            ("status".into(), Json::String(status.into())),
            (
                "last_error".into(),
                safe_error.map_or(Json::Null, Json::String),
            ),
            (
                "completed_at".into(),
                Json::String(chrono::Utc::now().to_rfc3339()),
            ),
            ("lease_owner".into(), Json::Null),
            ("lease_until".into(), Json::Null),
        ]),
    )
    .await
}

/// Claim one queued or lease-expired job. The job record remains in the App
/// database, so a standalone version can recover work after a worker restart.
/// Deployments use this contract even when the current API executes the first
/// publish synchronously.
pub async fn claim_next_job(
    pool: &SqlitePool,
    lease_owner: &str,
    lease_seconds: i64,
) -> Result<Option<Record>> {
    if lease_owner.trim().is_empty() || !(1..=86_400).contains(&lease_seconds) {
        return Err(DbError::InvalidIdentifier(
            "job lease owner/seconds are invalid".into(),
        ));
    }
    let schema = collection_schema(pool, "docs_jobs").await?;
    let now = chrono::Utc::now().to_rfc3339();
    let lease_until = (chrono::Utc::now() + chrono::Duration::seconds(lease_seconds)).to_rfc3339();
    // SQLite's default deferred transaction lets two workers select the same
    // candidate before either writes. Acquire the writer lock before the
    // candidate read so only one claimant can proceed.
    let mut connection = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *connection)
        .await?;
    let candidate = sqlx::query_as::<_, (String, f64)>(
        "SELECT id, COALESCE(attempts, 0.0) FROM \"docs_jobs\" WHERE (run_after IS NULL OR run_after <= ?) AND (status = 'queued' OR (status = 'running' AND lease_until IS NOT NULL AND lease_until < ?)) ORDER BY created_at ASC LIMIT 1",
    )
    .bind(&now)
    .bind(&now)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((job_id, attempts)) = candidate else {
        sqlx::query("ROLLBACK").execute(&mut *connection).await?;
        return Ok(None);
    };
    let claimed = sqlx::query(
        "UPDATE \"docs_jobs\" SET status = 'running', attempts = ?, lease_owner = ?, lease_until = ?, started_at = ?, updated_at = ? WHERE id = ? AND (run_after IS NULL OR run_after <= ?) AND (status = 'queued' OR (status = 'running' AND lease_until IS NOT NULL AND lease_until < ?))",
    )
    .bind(attempts + 1.0)
    .bind(lease_owner)
    .bind(&lease_until)
    .bind(&now)
    .bind(chrono::Utc::now())
    .bind(&job_id)
    .bind(&now)
    .bind(&now)
    .execute(&mut *connection)
    .await?;
    if claimed.rows_affected() != 1 {
        sqlx::query("ROLLBACK").execute(&mut *connection).await?;
        return Ok(None);
    }
    sqlx::query("COMMIT").execute(&mut *connection).await?;
    Ok(find_record(pool, &schema, &job_id).await?)
}

pub async fn reap_expired_jobs(pool: &SqlitePool) -> Result<u64> {
    let schema = collection_schema(pool, "docs_jobs").await?;
    let now = chrono::Utc::now().to_rfc3339();
    let filter = FilterNode::and(
        FilterNode::Eq("status".into(), Json::String("running".into())),
        FilterNode::Lt("lease_until".into(), Json::String(now.clone())),
    );
    let listed = list_records(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 200,
        },
        Some(&filter),
    )
    .await?;
    let mut count = 0_u64;
    for job in listed.items {
        let updated = sqlx::query(
            "UPDATE \"docs_jobs\" SET status = 'queued', lease_owner = NULL, lease_until = NULL, last_error = ?, updated_at = ? WHERE id = ? AND status = 'running' AND lease_until < ?",
        )
        .bind("lease expired; requeued")
        .bind(chrono::Utc::now())
        .bind(job.id.as_str())
        .bind(&now)
        .execute(pool)
        .await?;
        count += updated.rows_affected();
    }
    Ok(count)
}

pub async fn find_deployment(pool: &SqlitePool, id: &str) -> Result<Option<Record>> {
    let schema = collection_schema(pool, "docs_deployments").await?;
    find_record(pool, &schema, id).await
}

pub async fn list_deployments(pool: &SqlitePool) -> Result<Vec<Record>> {
    let schema = collection_schema(pool, "docs_deployments").await?;
    Ok(list_records_ordered(
        pool,
        &schema,
        crate::records::ListPage {
            page: 1,
            per_page: 200,
        },
        None,
        "created_at",
        true,
    )
    .await?
    .items)
}

/// Rollback is a new deployment record pointing at an old immutable object;
/// the historical row is never mutated into the active deployment.
pub async fn create_rollback(
    pool: &SqlitePool,
    target_id: &str,
    requested_by: Option<&str>,
) -> Result<Record> {
    let target = find_deployment(pool, target_id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;
    let status = target
        .fields
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if !matches!(status, "ready" | "superseded") {
        return Err(DbError::Invariant(
            "only a complete deployment can be rolled back to".into(),
        ));
    }
    let snapshot_key = target
        .fields
        .get("snapshot_key")
        .and_then(Json::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| DbError::Invariant("target deployment has no snapshot".into()))?;
    let checksum = target
        .fields
        .get("checksum")
        .and_then(Json::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| DbError::Invariant("target deployment has no checksum".into()))?;
    let (_, manifest) = find_version_manifest(pool).await?;
    let schema = collection_schema(pool, "docs_deployments").await?;
    create_record(
        pool,
        &schema,
        BTreeMap::from([
            ("version".into(), json!(manifest.manifest_revision as f64)),
            ("status".into(), Json::String("building".into())),
            (
                "snapshot_key".into(),
                Json::String(snapshot_key.to_string()),
            ),
            ("checksum".into(), Json::String(checksum.to_string())),
            (
                "snapshot_size".into(),
                target
                    .fields
                    .get("snapshot_size")
                    .cloned()
                    .unwrap_or(Json::Null),
            ),
            (
                "source_revision".into(),
                Json::String(format!("rollback:{target_id}")),
            ),
            (
                "renderer_version".into(),
                target
                    .fields
                    .get("renderer_version")
                    .cloned()
                    .unwrap_or(Json::String(manifest.renderer_version)),
            ),
            (
                "pages_count".into(),
                target
                    .fields
                    .get("pages_count")
                    .cloned()
                    .unwrap_or(json!(0.0)),
            ),
            (
                "created_by".into(),
                requested_by.map_or(Json::Null, |v| Json::String(v.to_string())),
            ),
            ("error".into(), Json::Null),
            ("error_details".into(), json!({"rollback_of": target_id})),
            ("completed_at".into(), Json::Null),
        ]),
    )
    .await
}

/// Create a rollback deployment and switch the active pointer in one App
/// transaction. Storage verification happens before this method; the
/// database-side state transition remains atomic and leaves no intermediate
/// building row when the pointer update conflicts with a newer manifest.
pub async fn create_rollback_ready(
    pool: &SqlitePool,
    target_id: &str,
    requested_by: Option<&str>,
    snapshot_size: u64,
) -> Result<Record> {
    let target = find_deployment(pool, target_id)
        .await?
        .ok_or_else(|| DbError::Sqlx(sqlx::Error::RowNotFound))?;
    let status = target
        .fields
        .get("status")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if !matches!(status, "ready" | "superseded") {
        return Err(DbError::Invariant(
            "only a complete deployment can be rolled back to".into(),
        ));
    }
    let snapshot_key = target
        .fields
        .get("snapshot_key")
        .and_then(Json::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DbError::Invariant("target deployment has no snapshot".into()))?;
    let checksum = target
        .fields
        .get("checksum")
        .and_then(Json::as_str)
        .filter(|value| is_sha256_hex(value))
        .ok_or_else(|| DbError::Invariant("target deployment has no valid checksum".into()))?;
    let (manifest_record, manifest) = find_version_manifest(pool).await?;
    let deployment_schema = collection_schema(pool, "docs_deployments").await?;
    let deployment_id = Uuid::now_v7().to_string();
    let now = chrono::Utc::now();
    let previous_id = manifest_record
        .fields
        .get("active_deployment_id")
        .and_then(Json::as_str)
        .map(str::to_string);
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO \"docs_deployments\" \
         (id, created_at, updated_at, version, status, snapshot_key, checksum, snapshot_size, source_revision, renderer_version, pages_count, created_by, error, error_details, completed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&deployment_id)
    .bind(now)
    .bind(now)
    .bind(manifest.manifest_revision as f64)
    .bind("ready")
    .bind(snapshot_key)
    .bind(checksum)
    .bind(snapshot_size as f64)
    .bind(format!("rollback:{target_id}"))
    .bind(
        target
            .fields
            .get("renderer_version")
            .and_then(Json::as_str)
            .unwrap_or(&manifest.renderer_version),
    )
    .bind(
        target
            .fields
            .get("pages_count")
            .and_then(Json::as_f64)
            .unwrap_or(0.0),
    )
    .bind(requested_by)
    .bind(Option::<String>::None)
    .bind(serde_json::json!({"rollback_of": target_id}).to_string())
    .bind(now.to_rfc3339())
    .execute(&mut *tx)
    .await?;
    let pointer = sqlx::query(
        "UPDATE \"docs_version_meta\" SET active_deployment_id = ?, updated_at = ? WHERE id = ? AND manifest_revision = ?",
    )
    .bind(&deployment_id)
    .bind(now)
    .bind(manifest_record.id.as_str())
    .bind(manifest.manifest_revision as f64)
    .execute(&mut *tx)
    .await?;
    if pointer.rows_affected() != 1 {
        return Err(DbError::Conflict(
            "version manifest changed while committing rollback".into(),
        ));
    }
    if let Some(previous_id) = previous_id.as_deref().filter(|id| *id != deployment_id) {
        sqlx::query("UPDATE \"docs_deployments\" SET status = ?, updated_at = ? WHERE id = ?")
            .bind("superseded")
            .bind(now)
            .bind(previous_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    find_record(pool, &deployment_schema, &deployment_id)
        .await?
        .ok_or_else(|| DbError::Invariant("rollback deployment disappeared after commit".into()))
}

pub async fn reader_session_subject(pool: &SqlitePool, token: Option<&str>) -> Result<String> {
    let token = token.ok_or_else(|| DbError::Core(rustbase_core::CoreError::Unauthorized))?;
    let hash = sha256_hex(token.as_bytes());
    let sessions = collection_schema(pool, "docs_reader_sessions").await?;
    let filter = FilterNode::and(
        FilterNode::Eq("token_hash".into(), Json::String(hash)),
        FilterNode::Eq("revoked".into(), Json::Bool(false)),
    );
    let session = list_records(
        pool,
        &sessions,
        crate::records::ListPage {
            page: 1,
            per_page: 1,
        },
        Some(&filter),
    )
    .await?;
    let session = session
        .items
        .into_iter()
        .next()
        .ok_or_else(|| DbError::Core(rustbase_core::CoreError::Unauthorized))?;
    let subject = session
        .fields
        .get("subject")
        .and_then(Json::as_str)
        .ok_or_else(|| DbError::Invariant("reader session subject missing".into()))?;
    let expires = session
        .fields
        .get("expires_at")
        .and_then(Json::as_str)
        .ok_or_else(|| DbError::Core(rustbase_core::CoreError::Unauthorized))?;
    let expires = chrono::DateTime::parse_from_rfc3339(expires)
        .map_err(|_| DbError::Core(rustbase_core::CoreError::Unauthorized))?;
    if expires.with_timezone(&chrono::Utc) <= chrono::Utc::now() {
        return Err(DbError::Core(rustbase_core::CoreError::Unauthorized));
    }
    Ok(subject.to_string())
}

pub async fn authorize_reader(
    pool: &SqlitePool,
    snapshot: &VersionSnapshot,
    path: &str,
    token: Option<&str>,
) -> Result<()> {
    if snapshot.manifest.access_mode == "public" {
        return Ok(());
    }
    let subject = reader_session_subject(pool, token).await?;
    let now = chrono::Utc::now();
    let allowed = snapshot.reader_grants.iter().any(|grant| {
        let prefix_matches = grant.path_prefix == "/"
            || path == grant.path_prefix
            || path.starts_with(&format!("{}/", grant.path_prefix.trim_end_matches('/')));
        grant.subject == subject
            && grant.effect == "allow"
            && prefix_matches
            && grant
                .expires_at
                .as_deref()
                .map(|expiry| {
                    chrono::DateTime::parse_from_rfc3339(expiry)
                        .map(|value| value.with_timezone(&chrono::Utc) > now)
                        .unwrap_or(false)
                })
                .unwrap_or(true)
    });
    if allowed {
        Ok(())
    } else {
        Err(DbError::Core(rustbase_core::CoreError::Forbidden))
    }
}

pub async fn create_reader_session(
    pool: &SqlitePool,
    subject: &str,
    token: &str,
    expires_at: &str,
) -> Result<Record> {
    if subject.trim().is_empty() || token.len() < 16 {
        return Err(DbError::InvalidIdentifier(
            "reader subject and token are required".into(),
        ));
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(expires_at)
        .map_err(|_| DbError::InvalidIdentifier("reader session expiry must be RFC3339".into()))?;
    let normalized_expires_at = parsed.with_timezone(&chrono::Utc);
    if normalized_expires_at <= chrono::Utc::now() {
        return Err(DbError::InvalidIdentifier(
            "reader session expiry must be in the future".into(),
        ));
    }
    let schema = collection_schema(pool, "docs_reader_sessions").await?;
    create_record(
        pool,
        &schema,
        BTreeMap::from([
            (
                "token_hash".into(),
                Json::String(sha256_hex(token.as_bytes())),
            ),
            ("subject".into(), Json::String(subject.to_string())),
            (
                "expires_at".into(),
                Json::String(normalized_expires_at.to_rfc3339()),
            ),
            ("revoked".into(), Json::Bool(false)),
        ]),
    )
    .await
}

pub async fn create_reader_grant(
    pool: &SqlitePool,
    grant: ReaderGrant,
    created_by: Option<&str>,
) -> Result<Record> {
    if grant.subject.trim().is_empty() || grant.effect != "allow" {
        return Err(DbError::InvalidIdentifier(
            "reader grant subject/path/effect is invalid".into(),
        ));
    }
    let path_prefix = if grant.path_prefix == "/" {
        "/".to_string()
    } else {
        grant.path_prefix.trim_end_matches('/').to_string()
    };
    validate_page_path(&path_prefix)?;
    let expires_at = grant
        .expires_at
        .as_deref()
        .map(|expiry| {
            chrono::DateTime::parse_from_rfc3339(expiry)
                .map(|value| value.with_timezone(&chrono::Utc).to_rfc3339())
                .map_err(|_| {
                    DbError::InvalidIdentifier("reader grant expiry must be RFC3339".into())
                })
        })
        .transpose()?;
    let schema = collection_schema(pool, "docs_reader_grants").await?;
    create_record(
        pool,
        &schema,
        BTreeMap::from([
            ("subject".into(), Json::String(grant.subject)),
            (
                "audience".into(),
                grant.audience.map_or(Json::Null, Json::String),
            ),
            ("path_prefix".into(), Json::String(path_prefix)),
            ("effect".into(), Json::String(grant.effect)),
            (
                "expires_at".into(),
                expires_at.map_or(Json::Null, Json::String),
            ),
            ("enabled".into(), Json::Bool(true)),
            (
                "created_by".into(),
                created_by.map_or(Json::Null, |v| Json::String(v.to_string())),
            ),
        ]),
    )
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyDeploymentRef {
    pub app_id: String,
    pub deployment_id: String,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteReleaseInput {
    pub deployments: BTreeMap<String, ReadyDeploymentRef>,
    pub default_app_id: String,
    #[serde(default = "default_language_code")]
    pub default_language_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteReleaseManifest {
    pub version: u64,
    pub deployments: BTreeMap<String, ReadyDeploymentRef>,
    pub default_app_id: String,
    pub default_language_code: String,
}

impl SiteReleaseManifest {
    fn validate(&self) -> Result<()> {
        if self.version == 0
            || self.deployments.is_empty()
            || !self.deployments.contains_key(&self.default_app_id)
        {
            return Err(DbError::Invariant(
                "site release manifest has no valid default deployment".into(),
            ));
        }
        validate_language_code(&self.default_language_code)?;
        for (app_id, deployment) in &self.deployments {
            if app_id.trim().is_empty()
                || deployment.app_id != *app_id
                || deployment.deployment_id.trim().is_empty()
                || !is_sha256_hex(&deployment.checksum)
            {
                return Err(DbError::Invariant(
                    "site release manifest contains an invalid deployment reference".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Atomically from the Workspace's point of view, create a small release
/// manifest and switch the site pointer. The caller must have already
/// verified each reference against its App-local READY deployment.
/// No page/content transaction crosses App databases.
pub async fn create_site_release(
    pool: &SqlitePool,
    input: SiteReleaseInput,
    created_by: Option<&str>,
) -> Result<Record> {
    if input.deployments.is_empty() || !input.deployments.contains_key(&input.default_app_id) {
        return Err(DbError::InvalidIdentifier(
            "site release needs a default selected App".into(),
        ));
    }
    validate_language_code(&input.default_language_code)?;
    for (app_id, deployment) in &input.deployments {
        if app_id.trim().is_empty()
            || app_id != &deployment.app_id
            || deployment.deployment_id.trim().is_empty()
            || !is_sha256_hex(&deployment.checksum)
        {
            return Err(DbError::Invariant(
                "site release deployment reference is inconsistent".into(),
            ));
        }
    }
    let site = find_site_catalog(pool)
        .await?
        .ok_or_else(|| DbError::Invariant("site catalog is not initialized".into()))?;
    let releases_schema = collection_schema(pool, "docs_site_releases").await?;
    let SiteReleaseInput {
        deployments,
        default_app_id,
        default_language_code,
    } = input;
    // Serialize writers before allocating the next release version. A plain
    // max(existing) + 1 read followed by a deferred transaction lets two
    // concurrent release requests select the same version.
    let mut connection = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *connection)
        .await?;
    let next_version =
        sqlx::query_scalar::<_, Option<f64>>("SELECT MAX(version) FROM \"docs_site_releases\"")
            .fetch_one(&mut *connection)
            .await?
            .unwrap_or(0.0) as u64
            + 1;
    let manifest = SiteReleaseManifest {
        version: next_version,
        deployments,
        default_app_id,
        default_language_code,
    };
    manifest.validate()?;
    let deployment_map = serde_json::to_string(&manifest)
        .map_err(|e| DbError::Invariant(format!("release manifest serialization: {e}")))?;
    let checksum = sha256_hex(
        &serde_json::to_vec(&manifest)
            .map_err(|e| DbError::Invariant(format!("release checksum: {e}")))?,
    );
    let release_id = Uuid::now_v7().to_string();
    let now = chrono::Utc::now();
    let created_by = created_by.map(str::to_string);
    sqlx::query(
        "INSERT INTO \"docs_site_releases\" \
         (id, created_at, updated_at, version, status, deployment_map, checksum, created_by, error, completed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&release_id)
    .bind(now)
    .bind(now)
    .bind(next_version as f64)
    .bind("ready")
    .bind(&deployment_map)
    .bind(&checksum)
    .bind(created_by)
    .bind(Option::<String>::None)
    .bind(now.to_rfc3339())
    .execute(&mut *connection)
    .await?;
    let updated =
        sqlx::query("UPDATE \"docs_site\" SET active_release_id = ?, updated_at = ? WHERE id = ?")
            .bind(&release_id)
            .bind(now)
            .bind(site.id.as_str())
            .execute(&mut *connection)
            .await?;
    if updated.rows_affected() != 1 {
        sqlx::query("ROLLBACK").execute(&mut *connection).await?;
        return Err(DbError::Sqlx(sqlx::Error::RowNotFound));
    }
    sqlx::query("COMMIT").execute(&mut *connection).await?;
    find_record(pool, &releases_schema, &release_id)
        .await?
        .ok_or_else(|| DbError::Invariant("site release disappeared after commit".into()))
}

pub async fn find_active_site_release(
    pool: &SqlitePool,
) -> Result<Option<(Record, SiteReleaseManifest)>> {
    let Some(site) = find_site_catalog(pool).await? else {
        return Ok(None);
    };
    let Some(release_id) = site.fields.get("active_release_id").and_then(Json::as_str) else {
        return Ok(None);
    };
    let releases_schema = collection_schema(pool, "docs_site_releases").await?;
    let Some(release) = find_record(pool, &releases_schema, release_id).await? else {
        return Err(DbError::Invariant(
            "site active release points to a missing record".into(),
        ));
    };
    if release.fields.get("status").and_then(Json::as_str) != Some("ready") {
        return Err(DbError::Invariant(
            "site active release is not READY".into(),
        ));
    }
    let value = release
        .fields
        .get("deployment_map")
        .cloned()
        .ok_or_else(|| DbError::Invariant("site release manifest is missing".into()))?;
    let manifest: SiteReleaseManifest = serde_json::from_value(value)
        .map_err(|e| DbError::Invariant(format!("invalid site release manifest: {e}")))?;
    manifest.validate()?;
    if release.fields.get("version").and_then(Json::as_f64) != Some(manifest.version as f64) {
        return Err(DbError::Invariant(
            "site release version does not match its manifest".into(),
        ));
    }
    let expected_checksum = sha256_hex(
        &serde_json::to_vec(&manifest)
            .map_err(|e| DbError::Invariant(format!("release checksum: {e}")))?,
    );
    if release.fields.get("checksum").and_then(Json::as_str) != Some(expected_checksum.as_str()) {
        return Err(DbError::Invariant(
            "site release checksum does not match its manifest".into(),
        ));
    }
    Ok(Some((release, manifest)))
}

pub async fn app_has_site_release_reference(pool: &SqlitePool, app_id: &str) -> Result<bool> {
    let schema = collection_schema(pool, "docs_site_releases").await?;
    let releases = list_all_records(pool, &schema, "created_at").await?;
    for release in releases {
        let Some(value) = release.fields.get("deployment_map") else {
            continue;
        };
        let manifest: SiteReleaseManifest = serde_json::from_value(value.clone())
            .map_err(|e| DbError::Invariant(format!("invalid site release manifest: {e}")))?;
        manifest.validate()?;
        if manifest.deployments.contains_key(app_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn record_to_reader_grant(record: &Record) -> Result<ReaderGrant> {
    Ok(ReaderGrant {
        subject: required_text(record, "subject")?,
        audience: optional_text(record, "audience"),
        path_prefix: required_text(record, "path_prefix")?,
        effect: required_text(record, "effect")?,
        expires_at: optional_text(record, "expires_at"),
    })
}

fn required_text(record: &Record, field: &str) -> Result<String> {
    record
        .fields
        .get(field)
        .and_then(Json::as_str)
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| DbError::Invariant(format!("docs record field '{field}' is missing")))
}

fn optional_text(record: &Record, field: &str) -> Option<String> {
    record
        .fields
        .get(field)
        .and_then(Json::as_str)
        .map(str::to_string)
}

async fn validate_publish_tree(pool: &SqlitePool, schema: &Schema, pages: &[Record]) -> Result<()> {
    for page in pages {
        let language = required_text(page, "language_code")?;
        let path = required_text(page, "path")?;
        let parent = optional_string_field(page, "parent_id");
        validate_parent(pool, schema, parent.as_deref(), &language, Some(&path)).await?;
        if let Some(parent_id) = parent.as_deref() {
            ensure_no_cycle(pool, schema, page.id.as_str(), parent_id).await?;
        }
    }
    Ok(())
}

fn validate_publish_page(page: &Record) -> Result<()> {
    let path = required_text(page, "path")?;
    let language = required_text(page, "language_code")?;
    validate_language_code(&language)?;
    validate_page_path(&path)?;
    let kind = required_text(page, "kind")?;
    if !matches!(kind.as_str(), "page" | "group") {
        return Err(DbError::Invariant(format!(
            "page '{}' has an unsupported kind",
            page.id
        )));
    }
    let content = page
        .fields
        .get("content")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if kind == "group" && !content.trim().is_empty() {
        return Err(DbError::Invariant(format!(
            "group page '{}' cannot contain content",
            page.id
        )));
    }
    if content.len() > 2 * 1024 * 1024 {
        return Err(DbError::InvalidIdentifier(
            "page content exceeds 2 MiB publish limit".into(),
        ));
    }
    let lowered = content.to_ascii_lowercase();
    if lowered.contains("<script")
        || lowered.contains("javascript:")
        || lowered.contains("dangerouslysetinnerhtml")
        || lowered.contains("<iframe")
        || lowered.contains(" onload=")
        || lowered.contains(" onclick=")
        || content.lines().any(|line| {
            line.trim_start().starts_with("import ") || line.trim_start().starts_with("export ")
        })
    {
        return Err(DbError::Invariant(format!(
            "page '{}' contains unsupported executable content",
            page.id
        )));
    }
    Ok(())
}

async fn list_all_records(
    pool: &SqlitePool,
    schema: &Schema,
    order_by: &str,
) -> Result<Vec<Record>> {
    let mut page = 1;
    let mut records = Vec::new();
    loop {
        let listed = list_records_ordered(
            pool,
            schema,
            crate::records::ListPage {
                page,
                per_page: 200,
            },
            None,
            order_by,
            false,
        )
        .await?;
        let count = listed.items.len();
        records.extend(listed.items);
        if count < listed.per_page as usize {
            break;
        }
        page += 1;
    }
    Ok(records)
}

fn validate_page_path(path: &str) -> Result<()> {
    if path != "/" && (!path.starts_with('/') || path.ends_with('/')) {
        return Err(DbError::InvalidIdentifier(
            "page path must start with '/' and not end with '/'".into(),
        ));
    }
    if path.contains("//") || path.contains("..") || path.contains('?') || path.contains('#') {
        return Err(DbError::InvalidIdentifier(
            "page path contains a reserved sequence".into(),
        ));
    }
    let first = path.split('/').nth(1).unwrap_or_default();
    if matches!(first, "api" | "_" | "docs") {
        return Err(DbError::InvalidIdentifier(
            "page path collides with a reserved route".into(),
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::{APP_MIGRATIONS, WORKSPACE_MIGRATIONS, apply_migrations};
    use crate::pool::{open_memory_pool, open_pool};
    use tempfile::tempdir;

    #[tokio::test]
    async fn workspace_bootstrap_is_idempotent() {
        let pool = open_memory_pool().await.unwrap();
        apply_migrations(pool.clone(), WORKSPACE_MIGRATIONS)
            .await
            .unwrap();

        bootstrap_workspace_collections(&pool).await.unwrap();
        bootstrap_workspace_collections(&pool).await.unwrap();

        let site = find_collection(&pool, "docs_site").await.unwrap().unwrap();
        assert_eq!(site.schema.id.as_str(), "docs_site");
        assert!(
            find_collection(&pool, "docs_versions")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn app_bootstrap_creates_reserved_collections_and_indexes() {
        let pool = open_memory_pool().await.unwrap();
        apply_migrations(pool.clone(), APP_MIGRATIONS)
            .await
            .unwrap();

        bootstrap_app_collections(&pool).await.unwrap();
        let pages = find_collection(&pool, "docs_pages").await.unwrap().unwrap();
        assert!(
            pages
                .schema
                .fields
                .iter()
                .any(|field| field.name == "parent_id")
        );

        let index_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'docs_pages_language_path'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(index_exists, 1);
    }

    #[tokio::test]
    async fn concurrent_job_claim_allows_only_one_worker() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("app.db")).await.unwrap();
        apply_migrations(pool.clone(), APP_MIGRATIONS)
            .await
            .unwrap();
        bootstrap_app_collections(&pool).await.unwrap();

        let jobs = collection_schema(&pool, "docs_jobs").await.unwrap();
        create_record(
            &pool,
            &jobs,
            BTreeMap::from([
                ("kind".into(), json!("publish")),
                ("status".into(), json!("queued")),
                ("payload".into(), json!({"deployment_id": "deployment-1"})),
                ("attempts".into(), json!(0.0)),
                ("run_after".into(), json!(chrono::Utc::now().to_rfc3339())),
            ]),
        )
        .await
        .unwrap();

        let first_pool = pool.clone();
        let second_pool = pool.clone();
        let (first, second) = tokio::join!(
            claim_next_job(&first_pool, "worker-a", 60),
            claim_next_job(&second_pool, "worker-b", 60),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.is_some() as u8 + second.is_some() as u8, 1);
        let claimed = first.or(second).expect("one worker must claim the job");
        assert_eq!(
            claimed.fields.get("attempts"),
            Some(&json!(1.0)),
            "a job must be claimed exactly once"
        );
    }

    #[tokio::test]
    async fn concurrent_publish_requests_reuse_one_idempotent_job() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("app.db")).await.unwrap();
        apply_migrations(pool.clone(), APP_MIGRATIONS)
            .await
            .unwrap();
        bootstrap_app_collections(&pool).await.unwrap();
        ensure_version_manifest(
            &pool,
            "acme",
            "Acme Docs",
            "web",
            "Web",
            &SiteCatalogConfig {
                description: None,
                default_language_code: "en".into(),
                access_mode: "public".into(),
                canonical_url: None,
                theme: json!({}),
            },
        )
        .await
        .unwrap();

        let first_pool = pool.clone();
        let second_pool = pool.clone();
        let (first, second) = tokio::join!(
            prepare_publish(&first_pool, None, "same-publish-request"),
            prepare_publish(&second_pool, None, "same-publish-request"),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.job.id, second.job.id);
        assert_eq!(first.deployment.id, second.deployment.id);
        assert!(first.in_progress || second.in_progress);
    }

    #[tokio::test]
    async fn concurrent_site_releases_allocate_distinct_versions() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("workspace.db")).await.unwrap();
        apply_migrations(pool.clone(), WORKSPACE_MIGRATIONS)
            .await
            .unwrap();
        bootstrap_workspace_collections(&pool).await.unwrap();
        upsert_site_catalog(
            &pool,
            SiteCatalogInput {
                name: "Acme Docs".into(),
                slug: "acme".into(),
                description: None,
                default_app_id: None,
                default_language_code: "en".into(),
                access_mode: "public".into(),
                canonical_url: None,
                theme: None,
            },
        )
        .await
        .unwrap();

        let input = || SiteReleaseInput {
            deployments: BTreeMap::from([(
                "web".into(),
                ReadyDeploymentRef {
                    app_id: "web".into(),
                    deployment_id: "deployment-1".into(),
                    checksum: "a".repeat(64),
                },
            )]),
            default_app_id: "web".into(),
            default_language_code: "en".into(),
        };
        let first_pool = pool.clone();
        let second_pool = pool.clone();
        let (first, second) = tokio::join!(
            create_site_release(&first_pool, input(), None),
            create_site_release(&second_pool, input(), None),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let versions = [
            first.fields.get("version").and_then(Json::as_f64),
            second.fields.get("version").and_then(Json::as_f64),
        ];
        assert!(versions.contains(&Some(1.0)));
        assert!(versions.contains(&Some(2.0)));
        assert!(find_active_site_release(&pool).await.unwrap().is_some());
    }

    fn page(language: &str, path: &str, parent_id: Option<String>) -> PageDraftInput {
        PageDraftInput {
            language_code: language.into(),
            parent_id,
            kind: "page".into(),
            title: path.trim_matches('/').into(),
            slug: path.trim_matches('/').replace('/', "-"),
            path: path.into(),
            position: 0.0,
            description: None,
            icon: None,
            hidden: false,
            content: "# Content".into(),
            content_format: "markdown".into(),
            config: None,
            translation_key: None,
            source_metadata: None,
        }
    }

    async fn docs_app_pool() -> SqlitePool {
        let pool = open_memory_pool().await.unwrap();
        apply_migrations(pool.clone(), APP_MIGRATIONS)
            .await
            .unwrap();
        bootstrap_app_collections(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn page_tree_enforces_parent_language_cycle_and_delete_invariants() {
        let pool = docs_app_pool().await;
        let root = create_page(&pool, page("en", "/guide", None))
            .await
            .unwrap();
        let child = create_page(&pool, page("en", "/guide/start", Some(root.id.to_string())))
            .await
            .unwrap();

        let cross_language = create_page(&pool, page("fr", "/guide-fr", Some(root.id.to_string())))
            .await
            .unwrap_err();
        assert!(matches!(cross_language, DbError::Invariant(_)));

        let cycle = update_page(
            &pool,
            root.id.as_str(),
            PagePatch {
                parent_id: Some(Some(child.id.to_string())),
                ..PagePatch::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(cycle, DbError::Invariant(_)));

        let delete_parent = delete_page(&pool, root.id.as_str()).await.unwrap_err();
        assert!(matches!(delete_parent, DbError::Invariant(_)));
    }

    #[tokio::test]
    async fn moving_a_page_rewrites_descendant_paths_atomically() {
        let pool = docs_app_pool().await;
        let root = create_page(&pool, page("en", "/guide", None))
            .await
            .unwrap();
        let child = create_page(&pool, page("en", "/guide/start", Some(root.id.to_string())))
            .await
            .unwrap();

        update_page(
            &pool,
            root.id.as_str(),
            PagePatch {
                path: Some("/manual".into()),
                ..PagePatch::default()
            },
        )
        .await
        .unwrap();
        let schema = docs_pages_schema(&pool).await.unwrap();
        let moved_child = find_record(&pool, &schema, child.id.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            moved_child.fields.get("path"),
            Some(&json!("/manual/start"))
        );
    }

    #[tokio::test]
    async fn moving_a_root_onto_a_descendant_frees_paths_before_root_update() {
        let pool = docs_app_pool().await;
        let root = create_page(&pool, page("en", "/guide", None))
            .await
            .unwrap();
        let child = create_page(&pool, page("en", "/guide/start", Some(root.id.to_string())))
            .await
            .unwrap();

        update_page(
            &pool,
            root.id.as_str(),
            PagePatch {
                path: Some("/guide/start".into()),
                ..PagePatch::default()
            },
        )
        .await
        .unwrap();
        let schema = docs_pages_schema(&pool).await.unwrap();
        let moved_child = find_record(&pool, &schema, child.id.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            moved_child.fields.get("path"),
            Some(&json!("/guide/start/start"))
        );
    }

    #[tokio::test]
    async fn page_paths_are_unique_per_language() {
        let pool = docs_app_pool().await;
        create_page(&pool, page("en", "/same", None)).await.unwrap();
        let duplicate = create_page(&pool, page("en", "/same", None))
            .await
            .unwrap_err();
        assert!(matches!(duplicate, DbError::Sqlx(sqlx::Error::Database(_))));

        // The same path is valid in a different language.
        create_page(&pool, page("fr", "/same", None)).await.unwrap();
    }

    #[tokio::test]
    async fn workspace_collections_use_the_same_record_engine() {
        use crate::records::create_record;
        use serde_json::json;
        use std::collections::BTreeMap;

        let pool = open_memory_pool().await.unwrap();
        apply_migrations(pool.clone(), WORKSPACE_MIGRATIONS)
            .await
            .unwrap();
        bootstrap_workspace_collections(&pool).await.unwrap();
        let site = find_collection(&pool, "docs_site").await.unwrap().unwrap();

        let fields = BTreeMap::from([
            ("name".to_string(), json!("Acme Docs")),
            ("slug".to_string(), json!("acme-docs")),
        ]);
        let record = create_record(&pool, &site.schema, fields).await.unwrap();
        assert_eq!(record.collection.as_str(), "docs_site");
    }

    #[test]
    fn reserved_collection_names_are_detected() {
        assert!(is_reserved_docs_collection("docs_pages"));
        assert!(is_reserved_docs_collection("docs_site"));
        assert!(!is_reserved_docs_collection("custom_metadata"));
    }
}
