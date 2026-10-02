# Nibleaf → RustBase server migration plan

**Status:** architecture plan; no application source has been modified.

**Analysis date:** 2026-09-30 (Asia/Ho_Chi_Minh)

**Repositories analyzed:**

- RustBase: `/home/user/rustbase`, `https://github.com/nexio-lab/rustbase.git`
- Nibleaf: `/home/user/nibleaf`, `https://github.com/lord007tn/nibleaf`

This plan uses the requested domain mapping:

> **System = single-organization server/root. Workspace = one documentation site. App = one versioned documentation line/branch inside that site.**

One System can contain many Workspaces/sites, and each Workspace can contain many version Apps. A site release can publish all selected version App deployments as one documentation product. No additional Organization layer is required for the current scope.

Nibleaf's page tree, branch, translation, snapshot, reader, and publishing invariants still need a docs domain module in the RustBase server. The target must make both a complete site package and a complete version database independently deployable.

---

## 1. Executive recommendation

Build a `rustbase-docs` server module using RustBase's existing workspace/app isolation, authentication primitives, SQLite pools, storage abstraction, audit trail, hooks, and realtime events. Add reserved documentation collections and domain services, routes, a durable job runner, immutable snapshot publishing, public host resolution, search, and a migration CLI.

Use a strangler migration rather than a rewrite of every Nibleaf surface at once:

1. **RustBase becomes the source of truth and API/public server.**
2. Keep the existing Nibleaf React/TanStack UI as a client during the first migration slice by adapting its API client to the RustBase compatibility API. This is still a server migration; it avoids making UI framework replacement a prerequisite.
3. Port the editor/public reader UI only after the RustBase documentation API and snapshot format are stable. The current Nibleaf application is a TanStack/Node application and cannot be made a Rust binary merely by copying its static assets; it currently owns server-side routes and API assumptions.
4. Use reserved dynamic collections as the primary docs persistence substrate, but keep page tree, deployments, jobs, readers, and access-control invariants behind domain services rather than unconstrained generic CRUD.

The first production milestone should be a complete, secure path for **Markdown pages → tree/navigation → multilingual draft → publish → immutable public snapshot → rollback**. Git, PDF/Chromium exports, vector search, advanced integrations, and some UI parity should follow that milestone rather than block it.

The deployment boundary is important: a **site package** contains the workspace catalog plus its version apps, while a **version package** contains one app database, its effective version/site manifest, its snapshot history, and its storage objects. Serving one version must not require a live join to another version database. A workspace can coordinate many versions, but the app database remains a viable future standalone server/data package.

### Architecture decision revision: dynamic-collection-first

The initial plan was intentionally conservative and proposed typed documentation tables for the page model. Given the desired RustBase direction, that is not required as the first implementation. Use RustBase's existing **hierarchy → dynamic collections → records** as the storage substrate:

- `System` is the single-organization root for this deployment scope.
- `Workspace` remains the site boundary; many Workspaces/sites are supported.
- `App` remains the version/branch data boundary.
- Reserved documentation collections inside each App store pages, languages, assets, deployments, jobs, and other version data.
- The workspace catalog uses reserved workspace tables/collections for site-wide metadata where available; otherwise the effective values are materialized into the App manifest.
- A domain service, not the generic CRUD endpoint, enforces page-tree, publish, snapshot, reader, and job invariants.

This reduces initial schema work while preserving a path to promote only measured hot/invariant-heavy collections to physical typed tables later. “Dynamic collection” describes the persistence contract; it does not mean that every documentation operation should be an unconstrained generic record PATCH.

---

## 2. What was found

### 2.1 Repository and build state

- RustBase is a Rust workspace with separate core, database, API, auth, storage, realtime, runtime, server, and UI/dashboard crates.
- RustBase's current release metadata identifies version `0.2.2`; the analyzed checkout was at commit `58a2c47`.
- Nibleaf is a TypeScript monorepo with React/TanStack UI, Hono server routes, Prisma/PostgreSQL data access, BullMQ/Redis workers, S3-compatible storage, search providers, exports, and several integrations. The analyzed checkout was at commit `5ac76b4`.
- Nibleaf's Prisma schema contains **59 models and 12 enums**. Its core documentation models include `Project`, `Branch`, `Language`, `Page`, `Deployment`, `Domain`, `Asset`, `Reader`, `Audience`, `ReaderSession`, `Comment`, `AnalyticsEvent`, `Export*`, `Git*`, `OpenApiDocument`, and search/index records.
- The source repositories were cloned and inspected. No source implementation or migration plan artifact existed before this report.
- Tests/builds were **not executed**: this environment has Node `v20.20.2`, but does not have `cargo`, `pnpm`, or `bun`. Nibleaf's package requirements call for a newer Node runtime than the available version. The analysis is therefore source-level/static, not a passing-build claim.

### 2.2 RustBase's existing platform

RustBase already provides the following useful foundation:

- Target hierarchy: `System → Workspace → App`. The System is the single-organization root for the current product scope; do not add an Organization layer unless multi-organization tenancy becomes a real requirement.
- Separate SQLite databases and storage directories for workspace and app scopes.
- Dynamic app collections with text, number, boolean, email, URL, date, JSON, relation, and file reference field types.
- Collection schema migrations, record CRUD, filtering, access rules, hooks, audit records, and realtime SSE/WebSocket events.
- Workspace/app admin roles, master-admin setup, password authentication, email OTP, password reset, TOTP, OAuth links, JWT/JWKS support, token revocation, and rate/lockout controls.
- Local and S3-compatible storage abstractions, SMTP/log mailers, custom routes, QuickJS hooks, and cron tasks.
- An embedded Svelte dashboard.

The current platform does **not** natively provide:

- A page tree with parent/order/path invariants.
- Markdown/MDX parsing, component policy, content sanitization, or documentation navigation.
- A workspace-wide immutable deployment/snapshot pointer.
- A public reader route or custom-domain resolver.
- Nibleaf's private-reader/audience/session/JWT access model.
- Full-text or vector search over published documentation.
- HTML/PDF/static export rendering.
- Git repository import, pull/push, conflict, or preview workflows.
- A durable, retryable, cross-process background job system.
- Product analytics for documentation readers.
- Cross-app transactions. A site-wide publish operation would span a workspace database and multiple app databases unless the snapshot is assembled and committed through an explicit orchestration protocol.

### 2.3 Nibleaf's important behavior

Nibleaf creates a project/site with its own organization, makes the creator the owner, seeds `en` and a default `main` branch, and stores page content as Markdown/MDX associated with a language and branch. Pages form a tree and carry title, slug/path, kind, parent, position, visibility, description, SEO/configuration, and translation metadata.

Branches are copied/forked with remapped page IDs. The published representation is not the draft tables: publishing reads the relevant branch/page/language/site/OpenAPI data, validates it, builds an immutable site snapshot, and creates a READY deployment. The previous READY deployment remains live if the new build fails. Rollback creates a new current deployment from an old snapshot rather than mutating historical deployment data.

Nibleaf then performs indexing, export, notification, analytics, Git, and other work asynchronously. Its public reader resolves a site/version/language/page, applies access policy, renders content, generates SEO output, and can search the published site.

---

## 3. Target RustBase ownership model

### 3.1 Canonical mapping

| Nibleaf concept | RustBase target | Notes |
|---|---|---|
| Server/root/single organization | `System` | Deployment/runtime and single-organization boundary for the current scope. |
| Documentation site | `Workspace` | One site per workspace; many Workspaces/sites can coexist. |
| Nibleaf project/site | `Workspace` | Nibleaf's project/site maps directly to a Workspace. |
| Branch/version | `App` | One app/data database per documentation version/branch. `main` is an app marked as the default version. |
| Page draft | Reserved dynamic `docs_pages` collection in the version App, accessed through a docs service | A page is stored as a dynamic record, but tree, path, order, language, and publish invariants are enforced by the docs service rather than raw generic CRUD. |
| Project configuration/theme | Workspace site catalog plus copied effective `docs_version_manifest` in each App | The workspace is canonical for multi-version administration; a version package carries everything needed to render independently. JSON is acceptable for extensible theme settings, but access mode and security-sensitive values need typed validation. |
| Language | Workspace defaults/catalog plus effective language rows in each App | Store stable BCP-47-like codes, not cross-database foreign keys. The app copy is authoritative for standalone version serving. |
| Deployment | App-scoped deployment registry + immutable storage snapshot; optional workspace release catalog | Each version can publish and roll back independently. A site release only selects already-READY version deployments; it does not require one cross-app content transaction. |
| Asset | App-owned RustBase object storage + typed asset metadata | Keep version assets self-contained; optional site-level deduplication must not become a runtime dependency. Add MIME, size, checksum, visibility, public key, references, and snapshot-manifest semantics. |
| Site member | Docs site member/RBAC table | RustBase's existing workspace/app admin roles are not sufficient for owner/admin/editor/viewer semantics. |
| Nibleaf author user | RustBase docs identity/session, reusing auth primitives | Do not make every editor a full master admin or generic app admin. |
| Reader/audience | New docs reader, audience, grant, and session tables | Separate readers from author/admin identities. |
| Domain | Workspace catalog plus optional version-package host aliases + host resolver | DNS/TLS provisioning belongs at the ingress/deployment edge, not in the current router alone. A standalone version may have its own verified host. |
| Search index | App/version-scoped SQLite FTS5 first; optional external vector index later | Index only READY app deployments; a site search can fan out over selected app indexes or use a site release manifest. |
| Export snapshot/artifact | Deployment snapshot plus storage-backed export job/artifact | Export only immutable snapshots. |
| Git connection/sync | New docs Git subsystem or later adapter service | Do not pretend generic hooks are a Git workflow. |
| Analytics event | Append-only docs event table, with optional ClickHouse sink | Keep raw event collection privacy-safe and bounded. |
| OpenAPI document | Workspace default/catalog plus effective App-owned document/blob | Validate/normalize JSON and include the effective document in the version manifest/snapshot so a standalone App can render it. |
| Comment | Typed App/page comment table | It needs page/anchor/user/resolution semantics and should not be a generic unvalidated record. Site-level review notifications can remain in the workspace catalog. |
| Notification/email | RustBase mailer and durable jobs | Reuse SMTP/log mailer after job durability exists; standalone packages may use an external notification sink. |

### 3.2 Why the target is not a 1:1 Nibleaf model

The earlier wording about a “deliberately asymmetric” mapping should not be read as a reason to preserve Nibleaf's database topology. Nibleaf's `Project` and `Branch` model is a logical product model backed by one PostgreSQL system and shared worker/search/storage services. The requested RustBase mapping is a deployment model:

- **Workspace = site control plane/catalog.** It owns the site identity, version catalog, author membership, and optional multi-version release selection.
- **App = version data plane.** It owns one complete documentation version and its effective configuration, languages, pages, assets, jobs, deployments, and search index.
- **App publication is the normal unit.** A version can be built, deployed, rolled back, backed up, and served without reading another App database.
- **Site publication is composition, not a cross-app build.** A site-level release may select one READY deployment from each App and publish a small release manifest. It must not assemble live page content across apps or require a cross-database transaction.
- **Shared metadata is copied into a version manifest where serving needs it.** Workspace data remains canonical for administration, but a standalone version package contains its effective site name/branding, language settings, renderer policy, access mode, canonical URL policy, and other read-time configuration.

This makes both of these deployment shapes possible:

1. **Site server:** mount one workspace catalog, all or some version app databases, and the site storage prefix.
2. **Version server:** mount one app database, its `docs_version_manifest`, its immutable snapshot/assets, and the required secrets/edge configuration. The server can run in single-version mode without the workspace catalog being available.

The target therefore intentionally adopts the desired `Workspace = site` / `App = version` mapping even though it does not copy Nibleaf's one-project/many-branches storage arrangement one-for-one.

### 3.3 Proposed physical layout

```text
system.db
  RustBase instance/platform records
  optional global docs identity aliases

workspaces/<site>/workspace.db
  RustBase apps (version catalog: one row per docs version)
  RustBase workspace/admin records
  docs_site_catalog
  docs_version_catalog
  docs_members / invitations
  docs_domains
  optional docs_site_releases (version -> READY app deployment map)
  optional shared author/reader identity catalog

workspaces/<site>/apps/<version>/data.db
  docs_version_manifest (self-contained effective serving config)
  docs_pages
  docs_languages (effective enabled/default language copy)
  docs_deployments (canonical version history and active pointer)
  docs_jobs (or an equivalent app-local durable queue)
  docs_assets metadata
  version-local comments/grants/settings
  optional extension collections

storage/docs/<site>/<version>/
  immutable version deployment snapshots
  version asset objects
  export artifacts
  optional search/index manifests

standalone-version/<version-package>/
  app data.db
  version manifest
  snapshot/assets storage prefix
  optional read-only reader/auth configuration
```

No docs serving path should require a SQL join across `workspace.db` and an App `data.db`. The workspace catalog can point to an App, but the App's effective manifest and READY deployment are sufficient for a version server to answer public requests.

When the product release includes all site versions, `docs_site_releases` in the Workspace is the logical publication boundary: it selects READY deployments from the version Apps and switches one active release pointer. It does not require copying draft pages into one cross-App transaction.

The current RustBase boot sequence assumes `system → workspace → app`. Add an explicit docs deployment adapter/single-App mode rather than weakening every generic RustBase route to tolerate a missing workspace catalog. In version mode, mount the App DB plus its manifest/storage prefix, enable the docs public/admin routes that are supported by the package, and keep unrelated generic workspace administration disabled or explicitly proxied to a site server.

## 4. Proposed documentation collection model

### 4.1 Site catalog and standalone version manifest

The workspace owns the canonical site catalog and administrative relationships. Each App also stores a materialized effective manifest so that public/version serving does not depend on a workspace database. Synchronization from workspace defaults into app manifests is explicit, audited, and revisioned; it is not an implicit cross-database read.

#### `docs_site_catalog` (workspace scope)

- `workspace_id` (primary key)
- site name, slug, description, icon/logo reference
- canonical site config/theme defaults
- default version/app ID and default language code
- access-mode defaults and site release ID
- configuration/manifest revision and timestamps

This is the administrative source of truth for a multi-version site. A standalone App receives the effective values it needs in `docs_version_manifest`.

#### `docs_languages` (workspace defaults)

- code, label, direction (`ltr`/`rtl`), enabled/default flags, position
- translation/project configuration JSON
- unique `(workspace_id, code)`

Each version App has an effective language table/manifest copy. A version may enable a subset or override display/navigation settings. Store stable BCP-47-like codes rather than cross-database foreign keys, and never infer RTL from a display name.

#### `docs_members` and invitations

- docs identity ID, workspace ID, role, status, timestamps
- roles: owner, admin, editor, reviewer, viewer (exact policy can be reduced for MVP)
- invitation token digest, expiry, inviter, accepted/revoked state

This is site administration data. A standalone public version can be read-only without it; standalone authoring requires an explicitly packaged identity/auth mode rather than silently assuming that a workspace DB exists.

#### `docs_versions` (workspace catalog)

The RustBase `apps` table remains the source of app identity. Add a typed docs metadata row or equivalent fields:

- app ID
- version/branch slug and display name
- default flag, enabled/published flag, position
- source branch/fork metadata
- version-specific public path/label
- created-by and timestamps

This catalog points to version Apps but does not own their page content or deployment state. Forking creates an App and copies source page rows in a transaction or resumable job, with a mapping table for old-to-new page IDs.

#### `docs_site_releases` (optional site composition)

For a multi-version site, keep a small workspace-level release manifest containing:

- release ID/revision and status
- selected `(app_id, deployment_id)` pairs
- default version and language routing
- site-level redirect/domain/canonical settings
- checksum and timestamps

It is a catalog of immutable App deployments, not a second copy of page content. A standalone version server can ignore it and serve its own App deployment.

#### `docs_domains`

- normalized hostname, workspace ID, primary flag
- verification method/token/status
- DNS/TLS provider identifiers and status
- last check/error
- optional version/app alias for a standalone version host

Host resolution must reject ambiguous/unverified domains and must not let a user-controlled `Host` header select an arbitrary workspace or App.

#### Readers and private access

Authoritative site-level reader identities/audiences may live in the workspace catalog, but any grant/policy needed by a standalone version must be materialized into its deployment snapshot or version manifest. Use separate tables for reader identity/status/contact/external subject, audience definitions, page/version grants, invitations, hashed reader sessions, external JWT provider configuration, replay protection, and privacy-safe reader audit events.

### 4.2 Version-app dynamic collections

The version App database is the authoritative data plane for one documentation version. It must be exportable and bootable as a standalone version package.

Dynamic collections are appropriate for the document-shaped content and evolving configuration in this domain. They are not sufficient by themselves for composite uniqueness, tree acyclicity, state transitions, full-text indexes, lease claims, or cross-collection transactions. Those behaviors belong in repository/service methods and, where necessary, small RustBase DB extensions.

The following are reserved dynamic collections: their field schemas are bootstrapped with the App and their schema hash is checked on startup. Generic collection APIs remain available for extension collections, but reserved docs collections are changed only through versioned docs migrations.

#### `docs_version_manifest`

Store the effective, revisioned values required for version serving, including:

- logical site ID/slug and version ID/slug/label;
- site name/branding/theme values used by the reader;
- effective language list/default/direction;
- renderer/MDX policy and snapshot schema version;
- canonical URL, public path, access-mode, reader-provider, and SEO defaults;
- asset/search namespace and source workspace revision;
- optional standalone host aliases and a manifest checksum.

The workspace catalog can regenerate this manifest. A public version server reads the manifest locally and does not need a workspace join.

#### `docs_deployments` and `docs_jobs`

Keep the canonical deployment history and active deployment pointer in the version App database:

- deployment ID/revision, status (`pending`, `building`, `ready`, `failed`, `superseded`, `cancelled`);
- source app/manifest revision and requested-by identity;
- immutable snapshot key, checksum, size, renderer/schema version;
- page/language counts and safe error details;
- created/started/completed timestamps;
- durable publish/search/export job records or an app-local job queue reference.

A deployment row is immutable history after completion. The App's active pointer is the public boundary for that version. A workspace site release may reference this pointer but must not mutate it.

Implement a reserved dynamic `docs_pages` collection in each version App DB. Use the generic collection/record repository for storage, but expose page-specific service methods for create, move, reorder, fork, import, and publish validation. Promote this collection to a physical typed table only if profiling or integrity requirements justify it.

Suggested fields:

- `id`, `created_at`, `updated_at`
- `language_code`
- `parent_id` (nullable self-reference)
- `kind`: page or group
- `title`, `slug`, normalized `path`
- `position`
- `description`, icon, hidden/unlisted flags
- `content` (raw Markdown/MDX source)
- `content_format` and renderer/schema version
- `config_json` for page/SEO settings
- `translation_key`
- optional source-file/import metadata
- author/reviewer metadata

Required indexes/constraints:

- unique page path within `(app/version, language_code)`
- stable ordering under `(language_code, parent_id, position)`
- parent belongs to the same app and language
- no cycles
- group/page kind rules
- slug/path normalization and reserved-route checks
- page language must be enabled for the workspace

All tree mutations—create, move, reorder, delete, branch fork, and import replacement—must go through one repository/service that validates the complete affected tree. Do not expose raw SQL or generic record PATCH for these operations.

### 4.3 App-owned assets

Assets should be owned by the version App (with optional content-addressed deduplication underneath), so a version package can be moved or deployed without a live workspace dependency. Extend `rustbase-storage` and docs metadata rather than treating the current file endpoint as complete. Add:

- content type, content length, checksum, upload state, creator
- object key and public delivery key
- upload/download authorization and short-lived signed URLs where applicable
- image dimensions and safe metadata if needed
- snapshot references and garbage-collection state
- size/type limits configured per site/plan

RustBase's current file API is admin-oriented and has a 25 MiB upload cap. Nibleaf assets need author/editor uploads, public reader delivery, content metadata, reference cleanup, and immutable snapshot references. Preserve the current API for generic apps and add docs-specific endpoints.

---

## 5. Markdown, MDX, navigation, and rendering

### 5.1 Source of truth

Keep raw source in the version app draft table. A publish job produces a normalized snapshot representation containing:

- version metadata
- language/direction
- page tree/navigation
- normalized route and SEO metadata
- rendered/sanitized content or a safe renderable AST
- referenced asset manifest
- OpenAPI references
- redirects and access policy
- snapshot schema/renderer version

The public reader must never execute arbitrary draft MDX or load an untrusted component by name.

### 5.2 MDX compatibility decision

Full JavaScript MDX execution is a major security and runtime boundary and is not supplied by RustBase. Decide explicitly between:

1. **Safe Rust renderer (recommended for first RustBase-native milestone):** support Markdown plus an allowlisted component syntax; reject or preserve unsupported MDX constructs with a publish error.
2. **Client-side controlled renderer:** publish sanitized Markdown/AST and let a trusted reader bundle render only known components.
3. **Isolated Node/MDX rendering service:** retain Nibleaf's compatible MDX runtime behind a restricted internal API while the rest migrates.

Do not run arbitrary tenant-authored JavaScript inside the RustBase process. Any temporary compatibility renderer must have a network/filesystem deny policy, resource limits, and a versioned renderer contract.

### 5.3 Public reader

Add a public reader route set that resolves either a verified host or an explicit site path:

```text
GET  /docs/:site/                         site shell/default version
GET  /docs/:site/:version/*path           page/navigation/SEO response
GET  /docs/:site/:version/search?q=...    published search
GET  /docs/:site/:version/sitemap.xml
GET  /docs/:site/:version/robots.txt
GET  /docs/:site/:version/assets/*key    immutable version asset delivery
POST /docs/:site/reader/*                 private-reader invite/session flows
```

The exact prefix can be changed to preserve Nibleaf URLs. Host-based resolution should be additive and should become canonical after DNS/TLS verification. Public handlers must:

- resolve the active READY deployment before reading content;
- apply version/language/reader access policy from the snapshot;
- return stable cache headers keyed by deployment revision;
- prevent draft leakage through error messages, search, previews, or asset URLs;
- emit privacy-safe analytics asynchronously.

---

## 6. Publishing and rollback protocol

### 6.1 Required invariants

1. A failed publish never replaces the currently active READY deployment of that App/version.
2. A public request sees one complete version snapshot, not a mixture of old and new pages.
3. A READY deployment's content, assets, renderer version, and search revision are immutable.
4. Rollback is a new App deployment/pointer revision, not an update to an old record.
5. A retry with the same idempotency key cannot create conflicting active deployments.
6. All version inputs are validated before that App's active pointer changes.
7. A site release can select only complete READY App deployments and contains references, not live cross-app content.

### 6.2 Publish sequence

1. **Draft transaction:** page/tree/config changes commit in the relevant App database.
2. **Request transaction:** insert a `docs_deployment` row in the App DB and a durable job/outbox row with an idempotency key. A workspace scheduler may index the job, but the App package must retain enough state for standalone recovery.
3. **Claim:** the durable worker leases the job. It must be safe to retry after process death.
4. **Read/validate:** load one App's pages, effective language/manifest, assets, redirects, OpenAPI references, and access policy. Validate tree invariants, duplicate paths, links, asset references, and Markdown/MDX policy without reading another App database.
5. **Build:** create a self-contained normalized version snapshot. Render/sanitize content under a fixed renderer version. Record asset checksums and all source revisions.
6. **Write:** write the snapshot to an immutable storage key using a temporary key and checksum verification, then make it visible under its final immutable key.
7. **Commit READY:** in an App transaction, mark the deployment READY and update that App's active deployment pointer only if the source/manifest revision conditions still hold. Do not update the pointer on a stale or superseded request.
8. **Optional site release:** a workspace operation may atomically create/update a small release manifest pointing to READY App deployments. It does not rebuild or join their page data.
9. **Post-publish jobs:** enqueue FTS/search indexing, export, notification, analytics/cache invalidation, and optional webhooks. These failures must not corrupt the READY snapshot.
10. **Failure:** mark the new App deployment FAILED with a safe error summary and leave the old version pointer untouched. Clean temporary objects asynchronously.

### 6.3 Rollback

Rollback accepts a historical READY deployment ID for one App/version, verifies that its object and schema version are readable, and creates a new App deployment/pointer revision referencing that immutable snapshot. If a site release is in use, a separate site-level release update can select the rolled-back App deployment. This preserves an auditable sequence and makes cache invalidation predictable.

### 6.4 Cross-app consistency

There is intentionally no cross-app content transaction. That is a feature of the target deployment model, not a limitation to hide. Each App captures and publishes its own version atomically. A multi-version site uses a workspace release manifest containing stable `(app, deployment, checksum)` references. If one version is being rebuilt, the site continues serving its previous READY deployment for that version. A site-wide release operation can validate all referenced deployments before switching the small manifest pointer, but it never reads draft pages across Apps.

## 7. Durable jobs, search, exports, analytics, and Git

### 7.1 Job subsystem (P0)

BullMQ cannot simply be replaced with an in-process RustBase hook. Hooks and cron are suitable for short extension work, not long render/index/export/Git jobs.

Add a `rustbase-docs-jobs` subsystem with an App-local SQLite-backed queue and a worker loop in the server, or a separately runnable Rust worker using the same repository code. A workspace coordinator may discover/dispatch jobs across Apps, but each job payload must be recoverable from the version database so the App can run standalone. Each job needs:

- ID, workspace/app scope, kind, JSON payload, idempotency key
- status, attempt count, priority, `run_after`
- lease owner/expiry, heartbeat, started/completed timestamps
- retryable/non-retryable error code and redacted details
- cancellation and parent/child job relation

Claim jobs with an atomic transaction, use exponential backoff, reap expired leases, and make every processor idempotent. Queue kinds should include `publish`, `search_index`, `export`, `email`, `analytics_rollup`, `git_sync`, `git_preview`, and `asset_gc`.

If deployment scale later requires multiple workers or Redis, preserve this job contract and add a queue adapter rather than coupling docs code to BullMQ again.

### 7.2 Search (P0/P1)

Start with SQLite FTS5 inside each version App over published page text/title/description and an ordinary metadata table containing site/version, deployment revision, language, path, and visibility. Build the index from that App's READY snapshot, not mutable drafts. Version search is standalone; a site-wide search can fan out over selected App indexes or use a site release manifest. Search results must verify the requested active deployment/revision and access policy.

Later add a Qdrant/embedding adapter behind the same search interface for semantic/hybrid search. Embedding model, schema, and vector size belong in the index revision metadata. A failed external index must not make the public site serve draft data or make the last READY snapshot unavailable.

### 7.3 Exports (P1)

- Export from an immutable deployment snapshot only.
- Store generated ZIP/HTML/Markdown/PDF artifacts in RustBase storage with checksum, MIME, size, retention, and owner metadata.
- Implement static HTML/Markdown/ZIP first.
- PDF requires a renderer decision: isolated Chromium service, browser worker, or a Rust-compatible renderer. Do not run unbounded Chromium from an HTTP request.
- Scheduled exports use the same durable job runner and retain artifacts by count/time policy.

### 7.4 Analytics (P1)

Record append-only events such as pageview, search, reader login, publish, and export. Hash or omit IP/user-agent data by default; apply retention and rate limits. Keep ingestion separate from public page latency. Add an optional ClickHouse sink only after the local event contract and backpressure behavior are defined.

### 7.5 Git (P2)

Nibleaf has repository connections, file state, import/export, webhook delivery, branches, previews, conflicts, and audit records. RustBase has no equivalent. Implement this as a separate adapter with:

- encrypted credentials/secrets and redacted logs;
- durable idempotent operations;
- import into a newly created version app first;
- conflict state that never silently overwrites drafts;
- preview snapshots isolated from the active deployment;
- webhook signature verification and replay protection.

A Markdown-folder import/export CLI can be an earlier useful subset. Full bidirectional sync should not be part of the first publish milestone.

---

## 8. Authentication, authorization, domains, and privacy

### 8.1 Authors/admins

Nibleaf has a global user plus per-organization membership. RustBase's current users/admins are scoped differently and its workspace/app admin permissions are broad. Recommended target:

- reuse RustBase's password hashing, OTP, reset, TOTP, OAuth, JWT, mailer, and revocation primitives;
- add a docs identity/session namespace or a system-level docs identity table if one user must belong to many documentation workspaces;
- add workspace docs membership roles and invitation flows;
- issue tokens with an explicit docs principal/capability and check workspace membership on every admin/editor route;
- keep master-admin endpoints separate from ordinary site owners;
- preserve audit actor, IP handling, and token revocation semantics.

A lower-risk first cut can migrate accounts as password-reset invitations rather than importing Better Auth password hashes. Direct hash migration must be confirmed against the actual Better Auth algorithm/parameters before use.

### 8.2 Private readers

Implement Nibleaf's reader access as a separate trust boundary:

- reader invite/session tokens stored as hashes;
- short expiry, revocation, last-used timestamp, and rate limits;
- audience/page grants evaluated against the snapshot;
- optional external JWT/JWKS provider with issuer/audience/subject/group mapping;
- replay protection for JWT IDs where configured;
- no reader token in a cache key that can cause cross-reader content leakage.

### 8.3 Custom domains

The RustBase HTTP router currently has no Nibleaf-style host resolver. Add a normalized-domain lookup and an ingress/TLS integration contract. Domain ownership verification, certificate issuance, wildcard handling, and proxy headers belong to the edge provider; RustBase must validate the resulting trusted host context. Begin with path-based public URLs for development and add custom domains once snapshot/public caching is stable.

### 8.4 Site-wide versus app-wide policy

- Site access mode, member roles, the version catalog, and the default version are workspace-scoped administrative data.
- Domains can be site-scoped, but a standalone version may carry its own verified host alias in its manifest.
- The effective language list, reader policy needed for serving, draft pages, page comments, version settings, active deployment, assets, and search index are App-scoped.
- Public authorization is evaluated using the App's manifest/snapshot plus version/page grants. Workspace identity data may be used when the App is mounted as part of a site, but is not required for a public standalone read-only package.
- Never use a generic app record access rule as the only check for public docs; it does not know active deployment or reader session semantics.

---

## 9. Compatibility API and UI strategy

### 9.1 API layering

Keep existing RustBase generic endpoints unchanged. Add a dedicated docs namespace, for example:

```text
/api/workspaces/:workspace/docs/site
/api/workspaces/:workspace/docs/members
/api/workspaces/:workspace/docs/languages
/api/workspaces/:workspace/docs/versions
/api/workspaces/:workspace/docs/versions/:app/pages
/api/workspaces/:workspace/docs/versions/:app/deployments
/api/workspaces/:workspace/docs/versions/:app/publish
/api/workspaces/:workspace/docs/releases       optional site release manifest
/api/workspaces/:workspace/docs/domains
/api/workspaces/:workspace/docs/readers
/api/workspaces/:workspace/docs/search
```

Add compatibility response shapes where the Nibleaf UI needs them, but keep Rust-native resource IDs and revision fields in the response. Version every public snapshot and docs API contract. Avoid copying Nibleaf's server actions into RustBase; translate behavior into Rust repositories/services with explicit transactions.

### 9.2 UI phases

1. Adapt Nibleaf's API client and keep its editor/dashboard as the initial authoring client.
2. Add a RustBase public reader or a static/edge reader that consumes the snapshot API.
3. Port/extend the RustBase dashboard only after the API stabilizes, or embed a compiled docs frontend under a distinct route.
4. Retire the Nibleaf Hono/Prisma/BullMQ server after migration verification.

The current embedded Svelte dashboard and Nibleaf's React/TanStack application are separate UI systems. Combining them should be a later product decision, not hidden inside the data migration.

---

## 10. Migration procedure

### 10.1 Do not copy Prisma tables directly

Nibleaf's 59-model PostgreSQL schema contains auth, usage, integration, and provider-specific records that have no one-to-one RustBase target. A direct database copy would also miss SQLite-specific constraints, RustBase IDs, storage keys, public snapshot semantics, and app isolation.

Build a versioned export/import format instead.

### 10.2 Export manifest

For each Nibleaf project/site, export:

```text
site.json
members.json / identity references
languages.json
versions.json
pages.ndjson
redirects.json
site-config.json
assets.json + asset blobs/checksums
openapi.json (if present)
reader/audience/grant manifests (if present)
deployments.json + snapshot objects per version
version-manifests.json
optional site-release manifests
comments.json
analytics export (optional/retention-dependent)
git metadata (not raw secrets)
```

Each manifest should include source project ID, source revision/time, export schema version, counts, checksums, and URL/path mappings. Do not export plaintext credentials, session tokens, webhook secrets, or Git access tokens.

### 10.3 Import order

1. Create the RustBase workspace using the source project slug, with a collision-safe slug mapping.
2. Create the workspace docs site row and languages.
3. Create one RustBase app for each Nibleaf branch/version; record source branch ID → app ID.
4. Materialize a self-contained `docs_version_manifest` in each App from the site defaults plus that version's overrides.
5. Import pages per app/language in parent-before-child or two-pass ID-map order. Validate/recompute paths and ordering.
6. Upload version assets with checksums and write the source-to-storage mapping; optional deduplication must not be required to serve the App.
7. Import site catalog, version catalog, domains, redirects, OpenAPI, comments, readers, audiences, and grants. Copy any serving-critical policy into the relevant App manifest/snapshot.
8. Import or rebuild historical deployment snapshots into each App's deployment registry. Mark only verified snapshots READY; keep the source current deployment active until validation completes.
9. Optionally create a site-release manifest selecting one READY deployment per version.
10. Import authors/members as invited/reset-required identities, then send activation/reset messages.
11. Rebuild FTS/search inside each version App from its selected READY snapshot.
12. Reconcile counts/hashes, run link/route/snapshot/standalone-boot checks, and produce a machine-readable import report.

### 10.4 Cutover

- Run an initial import while Nibleaf remains authoritative.
- Perform a delta export or a short source freeze/read-only window.
- Re-run changed pages/assets/members and rebuild the target active snapshot.
- Serve RustBase on a temporary path/host and crawl it with the same route set.
- Compare status codes, canonical URLs, titles, headings, language direction, asset checksums, and search results.
- Switch DNS/edge routing only after the target deployment is READY.
- Keep Nibleaf read-only for the rollback window. Retain source-to-target ID and URL redirect maps.
- After the window, disable old writes, archive the old service, and keep the old data/export under the agreed retention policy.

### 10.5 Migration acceptance checks

For every site/version/language:

- source and target page counts match;
- tree parent/order relationships match;
- normalized paths and redirects match;
- raw content hashes match unless a documented renderer conversion changed it;
- rendered HTML/AST is safe and contains no unexpected component execution;
- asset count, MIME, size, checksum, and referenced URLs match;
- each App's active deployment returns a complete self-contained snapshot;
- a version package can serve its public reader without another App database;
- failed publish leaves the previous App deployment live;
- rollback returns the selected historical content;
- private pages are inaccessible without the correct reader/audience grant;
- custom host cannot resolve another workspace;
- search never returns draft or revoked-reader content;
- audit events identify the actor and source import revision;
- repeated import with the same idempotency key is safe.

---

## 11. Phased implementation plan

### Phase 0 — decisions and hardening (P0)

- Confirm RustBase licensing/distribution strategy and Nibleaf code/trademark permissions.
- Freeze the snapshot schema, ID/slug rules, renderer/MDX policy, branch→app semantics, and standalone package contract.
- Define whether standalone authoring is supported or whether version mode is initially public/read-only; do not imply that a raw App DB contains site-admin identity state.
- Add repository-level migration feature flags and observability.
- Fix RustBase collection creation cleanup: the current error path constructs a `DROP TABLE` statement but does not execute it; make schema/table creation transactional or execute verified cleanup.
- Audit app/workspace deletion and storage cleanup so docs objects cannot be orphaned.
- Add a test harness in an environment with the required Rust/Node package toolchains.

**Exit:** signed architecture/licensing decision, stable fixtures, and no destructive migration without an audit/backup path.

### Phase 1 — docs data foundation (P0)

- Add `rustbase-docs-core` types and validators.
- Define the site-package/version-package format and the explicit RustBase single-version deployment mode.
- Add reserved dynamic collection schemas and schema-lock/bootstrap logic for documentation Apps.
- Add workspace/app migrations and repositories for site, languages, versions, members, pages, domains, deployment metadata, and jobs.
- Add page CRUD/tree/fork/import APIs with tenant and role checks.
- Add object metadata and content-type/presigned/public delivery extensions to storage.
- Add audit events and realtime events for draft changes.

**Exit:** an editor can create/move/reorder/translate Markdown pages in one version App, invalid trees/paths are rejected transactionally, and the App can be exported as a self-contained version package.

### Phase 2 — snapshot publishing/public reader (P0)

- Implement Markdown/allowlisted-MDX parser and sanitizer policy.
- Implement snapshot builder, asset manifest, deployment state machine, active pointer, and rollback.
- Add public site/version/page/SEO/sitemap routes using only active READY snapshots.
- Add preview/diagnostic endpoints that are authenticated and cannot be cached as public.

**Exit:** each App publish is atomic from a reader's perspective, failed builds preserve the prior version, rollback is audited, and an optional site release only composes READY App references.

### Phase 3 — durable jobs and search (P0/P1)

- Add lease/retry/heartbeat/reaper worker.
- Move publish post-processing, indexing, email, and asset cleanup into jobs.
- Add SQLite FTS5 with deployment/version/language filters.
- Add cache invalidation and metrics for queue latency/failure/retry.

**Exit:** server restart during a publish/index/export job recovers without duplicate or corrupted results.

### Phase 4 — assets, auth, readers, domains, analytics (P1)

- Complete author membership/invitation flows and global identity decision.
- Add private reader/audience/session/JWT provider flows.
- Add verified custom domains and ingress contract.
- Add privacy-safe analytics and optional sink.
- Add comments and review/audit workflows.

**Exit:** public, private, and editor paths are isolated and tested across two workspaces and two versions.

### Phase 5 — exports and Git (P1/P2)

- Add static exports and storage-backed artifact retention.
- Add optional PDF worker boundary.
- Add Markdown-folder import/export, then provider Git connections, previews, conflicts, webhooks, and audit.
- Add optional Qdrant/hybrid search adapter.

**Exit:** each integration is idempotent, secret-safe, cancellable, and cannot change the active deployment without an explicit publish.

### Phase 6 — UI consolidation and cutover (P1)

- Adapt or port the Nibleaf editor/public UI to the versioned RustBase API.
- Run fixture, dual-read, crawl, load, security, and migration acceptance suites.
- Perform staged site cutover with rollback to the old service during the retention window.
- Remove old server dependencies only after all required feature gates are green.

---

## 12. Test and operational plan

### Unit/property tests

- slug/path normalization, reserved route handling, Unicode and RTL behavior;
- page tree cycle/order/parent constraints;
- branch/app fork ID remapping;
- language/default-language and translation matching;
- redirect cycles and link validation;
- snapshot determinism and checksum generation;
- deployment state transitions and rollback;
- reader/audience grant evaluation;
- host normalization and workspace resolution;
- MD/MDX sanitization and resource limits;
- job idempotency/backoff/lease recovery.

### Database and integration tests

- migration from empty and partially upgraded workspace/app DBs;
- concurrent page edits and publish source-revision conflicts;
- storage failure before/after READY commit;
- worker process death and lease recovery;
- two workspaces with identical page IDs/slugs cannot cross-read;
- asset delete/reference cleanup;
- FTS rebuild from a historical snapshot;
- auth role matrix and token revocation.

### End-to-end tests

- create site → create version → add `en` and RTL language → edit tree → publish → reader route;
- export one version package, start a single-version server, and serve it without the workspace catalog or another App;
- fork version → edit fork → publish each version independently → optional site release → search;
- failed publish → old content remains live;
- rollback → new revision serves old content;
- reader invitation/JWT/audience grant/private page;
- custom domain verification and host isolation;
- export and Git preview do not mutate live content;
- Nibleaf fixture import produces expected counts, routes, assets, and snapshots.

### Production safeguards

- backups before each import/cutover and tested restore;
- snapshot/object checksums and retention policies;
- publish/search/job metrics and structured audit logs;
- rate limits for public search, reader login, asset upload, and publish;
- size/time limits for Markdown/MDX, pages, assets, exports, and jobs;
- explicit data deletion/export policy for readers and analytics;
- load testing within RustBase's SQLite/single-instance assumptions before accepting high-traffic sites.

---

## 13. Risks and decisions that must not be hidden

1. **License and trademark:** Nibleaf is AGPL-3.0-only, and its name/logo are separately restricted. Copying Nibleaf server code into a RustBase distribution requires an explicit license/ownership review. Options are a properly licensed combined AGPL distribution, a process/API boundary with separate work, or a clean-room Rust implementation from behavior/specifications. Do not ship a modified product under the Nibleaf name without permission.
2. **MDX runtime:** arbitrary JavaScript MDX is incompatible with an uncomplicated in-process Rust server. Choose allowlisted rendering, a client renderer, or an isolated compatibility service.
3. **Identity model:** Nibleaf's global users and per-site memberships do not exactly match RustBase's current workspace-scoped users/admins. A role-aware docs identity/membership layer is needed for faithful migration.
4. **Site release versus version publish:** the target avoids cross-app content transactions by making App/version publishing canonical. A multi-version site release must remain a small, validated reference manifest, not a hidden cross-database build.
5. **Durability:** QuickJS hooks and cron are not BullMQ. Long-running jobs need leases, retries, and recovery before publish/export/Git is reliable.
6. **Storage:** current RustBase file APIs do not provide all metadata, presigned, public delivery, and reference-accounting behavior required by Nibleaf.
7. **Search parity:** FTS5 is a sensible first target but does not equal Qdrant/embedding search. Make semantic search an explicit later capability.
8. **Exports:** PDF generation needs a renderer boundary and resource limits; it cannot be an unbounded HTTP handler.
9. **UI ownership:** porting Nibleaf's React/TanStack UI into the embedded Svelte dashboard is a separate product/UI project. Do not describe that work as complete when the server migration is complete.
10. **SQLite scale:** validate site size, publish time, concurrent authors, public read rate, and backup/restore behavior. Move only the hot/search/job paths to external services when measurements justify it.

---

## 14. Recommended first pull requests

1. **RFC/fixtures:** add this mapping and a small sanitized Nibleaf fixture with two versions, `en` plus an RTL language, a page tree, an asset, a redirect, and a failed/ready deployment history.
2. **Core/domain types:** add docs IDs, roles, reserved collection schemas, page/tree validators, self-contained version-manifest/snapshot structs, deployment state machine, and renderer contract without HTTP/database code.
3. **DB foundation:** add workspace catalog and App-local migrations, typed page repositories, effective language/manifest/deployment/job metadata, and transaction tests.
4. **API/editor slice:** add role-checked page/tree/language/version endpoints and adapt the Nibleaf API client; keep publish version-scoped.
5. **Publish slice:** add App-local snapshot build, immutable storage object, active pointer, rollback, standalone package export, and failure recovery.
6. **Public/search/worker slice:** add public reader, App-local FTS5, durable worker, standalone single-version mode, optional site release manifest, and cache/revision behavior.
7. **Migrator:** implement export-manifest importer, ID/path/asset mapping, dry-run report, resumability, and cutover verification.
8. **Parity work:** readers, domains, comments, exports, Git, analytics, vector search, integrations, and UI consolidation in separate feature-gated changes.

This ordering keeps the requested `Workspace = site` / `App = versioned docs` mapping visible in every layer while preventing the existing generic collection API from becoming an accidental, under-specified Nibleaf replacement.
