# Phân tích khả thi: Nibleaf → RustBase theo DB hierarchy và dynamic collections

**Phạm vi:** đánh giá kiến trúc và tính khả thi sau khi chốt mapping:

```text
RustBase System
└── Workspace = một Nibleaf documentation site
    └── App = một version/branch của documentation
        └── Dynamic collections
            └── Records
```

**Kết luận ngắn:** hướng này khả thi và phù hợp với RustBase hơn phương án port nguyên xi 59 model Prisma của Nibleaf. Tuy nhiên, cần phân biệt rõ:

- **Dynamic collection là persistence substrate**: nơi lưu dữ liệu document và schema linh hoạt.
- **Docs service là domain boundary**: nơi thực thi page tree, publish, snapshot, rollback, reader access, job lease và các invariant.
- **Generic CRUD không được trở thành public contract duy nhất** của documentation system.

Nếu chỉ dùng generic collections và CRUD hiện có, không thể đạt parity đáng tin cậy với Nibleaf. Nếu dùng **dynamic collections + domain services + một số DB extensions nhỏ**, kiến trúc có tính khả thi cao.

---

## 1. Đánh giá điều hành

### 1.1 Quyết định đề xuất

Nên chọn phương án:

> **Dynamic-collection-first, domain-service-enforced hybrid.**

Không cần bắt đầu bằng việc tạo hàng chục typed table riêng cho Nibleaf. Hãy bootstrap các collection reserved cho documentation trong từng App, sau đó dùng Rust service để thao tác qua các use case có kiểm soát.

Ví dụ:

- `docs_pages` là dynamic collection.
- `DocsVersionService::move_page()` mới là API hợp lệ để di chuyển page.
- `DocsPublishService::publish()` mới là API hợp lệ để tạo deployment.
- Generic `PATCH /records/docs_pages/:id` chỉ được sử dụng cho extension/admin nội bộ hoặc bị khóa với reserved collections.

### 1.2 Điểm khả thi theo phương án

| Phương án | Khả thi kỹ thuật | Đánh giá |
|---|---:|---|
| Port nguyên 59 Prisma models sang RustBase | Trung bình/thấp | Tốn công, tạo nhiều bảng ít giá trị, không tận dụng đúng hierarchy của RustBase. |
| Chỉ dùng dynamic collections + generic CRUD | Thấp cho full parity | Lưu được dữ liệu nhưng không đảm bảo page tree, publish atomic, snapshot, job lease và private reader. |
| Dynamic collections + docs domain services + DB extensions nhỏ | **Cao** | Cân bằng giữa tốc độ, tính linh hoạt và độ an toàn. Đây là phương án đề xuất. |
| Typed table cho toàn bộ docs domain ngay từ đầu | Cao về integrity, thấp về tốc độ | Mạnh nhưng over-engineering ở giai đoạn đầu; chỉ nên promote các phần có bằng chứng về bottleneck/invariant. |

Đánh giá tương đối:

- Core content/editor: **8/10**.
- Version/App isolation: **8.5/10**.
- Publish/snapshot/rollback sau khi bổ sung job và storage: **7.5/10**.
- Auth/RBAC/private reader: **6.5/10**.
- Public domain/reader/search: **6.5/10**.
- Git/PDF/vector search/integrations: **4–6/10**, vì RustBase hiện chưa có các subsystem tương đương.
- Full Nibleaf parity bằng một lần port: **không nên cam kết**. Khả thi theo từng phase, không khả thi nếu chỉ dựa vào generic CRUD hiện tại.

Các điểm số này là đánh giá kiến trúc dựa trên source đã phân tích, không phải kết quả benchmark; RustBase/Nibleaf chưa build/test trong môi trường hiện tại vì thiếu `cargo`, `pnpm`, `bun` và Node đang thấp hơn yêu cầu của Nibleaf.

---

## 2. Vì sao hierarchy này phù hợp

### 2.1 `Workspace = site`

Nibleaf Project/site là tenant boundary tự nhiên. RustBase Workspace cũng đã là boundary cho:

- workspace administrators;
- workspace database;
- apps;
- audit/policies;
- workspace-level identity/configuration;
- storage namespace.

Vì vậy không nên map Nibleaf Project thành một collection hoặc một App. Nó nên là `Workspace`.

### 2.2 `App = version`

Nibleaf Branch/version cần isolation tốt hơn một record `branch_id` trong cùng một bảng. Map thành App có các lợi ích:

- mỗi version có SQLite database riêng;
- có thể backup/restore/fork độc lập;
- schema/content lỗi ở một version không làm hỏng version khác;
- dễ đóng gói thành standalone version server;
- page không cần thêm `branch_id` trong mọi record;
- quyền và lifecycle của version có thể gắn với App.

Đây là mapping có lợi về deployment, dù không giống cách Nibleaf đang lưu Branch trong PostgreSQL.

### 2.3 `Collection = domain aggregate hoặc document type`

Không nên biến mọi Prisma model thành một collection. Nên gom theo bounded context:

- page content và navigation;
- publishing/deployment;
- asset metadata;
- reader/access;
- jobs;
- comments/review;
- integration state.

Các bảng Nibleaf chỉ phục vụ usage billing, provider checkpoint, audit phụ, hoặc integration-specific state có thể được trì hoãn hoặc gộp vào JSON/config collection.

---

## 3. Target data layout

### 3.1 Workspace scope

RustBase hiện có workspace database nhưng chưa có đầy đủ user-defined collections ở workspace scope như App scope. Có ba cách:

#### Phương án A — thêm workspace dynamic collections, khuyến nghị

Mở rộng collection subsystem để hỗ trợ cả:

```text
CollectionScope::Workspace(workspace_id)
CollectionScope::App(workspace_id, app_id)
```

Dùng chung schema/record engine, chỉ khác pool và namespace. Khi đó workspace có thể có:

- `docs_site`
- `docs_versions` hoặc metadata mirror của RustBase `apps`
- `docs_members`
- `docs_domains`
- `docs_site_releases`
- `docs_identities`
- site-level reader catalog/audit

Đây là cách nhất quán nhất với yêu cầu “follow DB hierarchy và dynamic fields collections”.

#### Phương án B — workspace native migrations

Các bảng site-level được thêm bằng migration RustBase riêng, không expose như dynamic collection. Cách này nhanh và integrity tốt hơn, nhưng không thuần dynamic.

#### Phương án C — không có workspace collections, duplicate site data vào App

Mỗi App lưu site config/effective manifest riêng. Workspace chỉ giữ apps/admins. Đây là cách nhanh nhất để standalone version hoạt động, nhưng có vấn đề:

- thay đổi branding/domain/policy phải fan-out nhiều App;
- dễ tạo manifest lệch nhau;
- site-level membership/domain/release khó quản lý;
- khó audit canonical source.

**Khuyến nghị:** dùng A nếu mục tiêu là platform lâu dài; dùng B cho P0; luôn materialize effective manifest vào App để standalone serving không phụ thuộc Workspace DB.

### 3.2 App scope: reserved dynamic collections

Mỗi documentation App được bootstrap với các collection reserved sau:

| Collection | Mục đích | Ghi chú |
|---|---|---|
| `docs_meta` | Một record chứa version manifest, site ID/slug, version ID/slug, renderer version, active deployment reference | Có schema lock; không cho generic delete. |
| `docs_languages` | Ngôn ngữ hiệu lực của version, direction, default, position | Không dùng foreign key sang workspace; dùng stable language code. |
| `docs_pages` | Page/group tree và raw Markdown/MDX | Collection quan trọng nhất; mọi mutation đi qua page service. |
| `docs_assets` | Metadata của asset trong version | Blob ở Storage; record giữ key, checksum, MIME, size, reference state. |
| `docs_redirects` | Redirect source/target/status | Publish validation chống cycle và invalid target. |
| `docs_openapi` | OpenAPI document hiệu lực của version | JSON có validation riêng. |
| `docs_deployments` | Pending/building/ready/failed history, snapshot key/checksum | Snapshot lớn không lưu trực tiếp trong record. |
| `docs_jobs` | Publish/search/export/email/Git job state | Dùng dynamic record làm storage ban đầu; claim/lease phải có DB primitive riêng. |
| `docs_comments` | Page comment, anchor, resolved state | Có thể trì hoãn ở MVP. |
| `docs_readers` | Reader identity/status cho version standalone | Có thể trỏ logical site identity khi chạy trong Workspace mode. |
| `docs_audiences` | Nhóm reader và grant | Cần policy service. |
| `docs_grants` | Page/version access grants | Không để generic public record filtering quyết định private docs. |
| `docs_search_documents` | Search document metadata | FTS5 virtual table/index là DB extension, không chỉ là dynamic collection. |
| `docs_analytics` | Raw events mức vừa/nhỏ | Traffic lớn nên chuyển sink riêng. |

### 3.3 Schema đề xuất cho `docs_pages`

```text
id                  text
created_at          date
updated_at          date
language_code       text
parent_id           text nullable
kind                text       -- page | group
title               text
slug                text
path                text
description         text nullable
icon                text nullable
position            number
hidden              bool
content             text       -- Markdown/MDX source
content_format      text       -- markdown | mdx
config              json       -- SEO/page options
translation_key     text nullable
source_metadata     json nullable
```

Vì App đã đại diện cho version nên không cần `branch_id` trong `docs_pages`.

Các invariant không nên chỉ dựa vào schema dynamic:

- `parent_id` phải cùng App và cùng language context;
- không được có cycle;
- path unique theo language;
- `kind=group` và `kind=page` có semantics khác nhau;
- position phải ổn định dưới cùng parent;
- slug/path phải normalized và không đụng reserved route;
- language phải enabled trong `docs_languages`;
- page move không được làm mất subtree;
- page delete phải có policy cho children, redirects và comments.

### 3.4 Cần mở rộng dynamic schema một cách có chọn lọc

RustBase hiện đã có required/unique/type mapping, nhưng docs domain cần thêm một số khả năng nhỏ:

```text
indexes:
  - fields: [language_code, path]
    unique: true
  - fields: [language_code, parent_id, position]

reserved: true
schema_version: 1
schema_hash: ...
public_read: false
```

Không nhất thiết phải xây một ORM mới. Chỉ cần:

- composite indexes/unique indexes cho reserved collections;
- schema lock để generic admin không phá schema docs;
- transaction-aware repository methods;
- migration hook cho virtual FTS5 tables;
- optional check/index definitions trong collection schema.

---

## 4. Phần nào dynamic collections làm tốt, phần nào không

### 4.1 Dynamic collections làm tốt

- lưu document fields thay đổi theo thời gian;
- `config` JSON cho theme, SEO, page options;
- custom metadata do người dùng định nghĩa;
- CRUD cơ bản và filtering;
- record audit/realtime nếu dùng đúng hook;
- per-App data isolation;
- export/import record data;
- version-specific extension collections;
- schema evolution ở các trường không critical.

### 4.2 Dynamic collections không đủ nếu dùng một mình

#### Page tree

Một record có `parent_id` không tự bảo đảm tree. Cần transaction + cycle detection + path recomputation. SQLite không có row-level `FOR UPDATE` như PostgreSQL; nên dùng:

- `BEGIN IMMEDIATE` cho mutation tree;
- per-App async mutex để giảm conflict trong cùng process;
- unique index cho `(language_code, path)`;
- kiểm tra cycle trong service;
- optimistic revision nếu nhiều server/worker cùng ghi.

#### Publish

Một generic record `deployment.status=ready` không tự làm snapshot atomic. Cần:

1. đọc đúng revision của tất cả reserved collections trong App;
2. build snapshot độc lập;
3. ghi object immutable vào Storage;
4. verify checksum;
5. transaction update active deployment pointer;
6. public reader chỉ đọc pointer READY.

#### Job lease

`docs_jobs` có thể là dynamic collection để lưu payload, nhưng claim job cần atomic conditional update:

```sql
UPDATE docs_jobs
SET status = 'running', lease_owner = ?, lease_until = ?
WHERE id = ?
  AND status IN ('queued', 'retry')
  AND (lease_until IS NULL OR lease_until < ?)
```

Generic PATCH không đủ an toàn cho việc này. Cần DB method riêng.

#### Search

Dynamic record table không thay thế được FTS5. Dùng:

- `docs_search_documents` cho metadata;
- FTS5 virtual table riêng;
- rebuild từ READY snapshot;
- search revision gắn với deployment ID.

#### Reader access

Access rule generic ở collection-level không biết:

- active deployment;
- reader session;
- audience grant;
- page-level entitlement;
- revoked invitation;
- JWT replay.

Cần `DocsAccessService` ở trên generic access rules.

---

## 5. Các service bắt buộc phải có

### `DocsWorkspaceService`

- tạo site Workspace;
- tạo/fork/delete version App;
- sync workspace defaults vào `docs_version_manifest`;
- quản lý member/domain/site release;
- tạo site package.

### `DocsVersionService`

- page CRUD;
- move/reorder/delete subtree;
- language/translation operations;
- import Markdown/MDX;
- fork App;
- asset reference accounting.

### `DocsPublishService`

- capture source revision;
- validate tree/path/link/asset/MDX;
- build snapshot;
- write immutable storage object;
- update App active deployment pointer;
- rollback;
- optional site release composition.

### `DocsPublicService`

- resolve host/site/version/language;
- load manifest and active snapshot;
- serve page/navigation/SEO/sitemap;
- enforce private reader grant;
- deliver immutable assets;
- emit analytics asynchronously.

### `DocsJobService`

- enqueue;
- idempotency;
- atomic claim;
- heartbeat;
- retry/backoff;
- lease recovery;
- cancellation;
- dead-letter/error state.

### `DocsMigrationService`

- import Nibleaf export manifest;
- map Project → Workspace;
- map Branch → App;
- bootstrap reserved collections;
- import records/assets/snapshots;
- verify counts/checksums/routes;
- resume after failure.

Các service này nên gọi `rustbase-db` repository primitives. Không nên bypass trực tiếp bằng HTTP generic records trong production paths.

---

## 6. Đánh giá từng Nibleaf capability

| Capability | Storage bằng dynamic collections | Phần phải bổ sung | Khả thi |
|---|---|---|---|
| Markdown/MDX pages | `docs_pages.content` + JSON config | Renderer, sanitizer, allowlist component, size/time limits | Cao cho Markdown; trung bình cho full MDX |
| Navigation/page tree | `parent_id`, `position`, `path` | Tree service, transaction, composite indexes | Cao |
| Branch/version | Một App cho mỗi branch/version | App fork/clone và manifest sync | Cao |
| Multilingual/RTL | `docs_languages`, page `language_code` | Translation matching, direction propagation | Cao |
| Publishing | `docs_deployments` + Storage | Snapshot builder, immutable pointer, durable job | Trung bình-cao |
| Rollback | Historical deployment records | New forward pointer revision | Cao sau khi publish ổn định |
| Public reader | Snapshot/manifest trong App | Public routes, host resolver, cache headers | Trung bình |
| Custom domain | Workspace `docs_domains` | Verification/TLS/edge contract | Trung bình |
| Assets | `docs_assets` + Storage | MIME/size/checksum/presigned/public delivery | Trung bình |
| Auth/RBAC | Existing RustBase auth + membership collection | Docs roles, site membership, reader identity | Trung bình-cao |
| Private readers | Reader/audience/grants collections | Session/JWT/JWKS/replay/access service | Trung bình |
| FTS search | Metadata collection + FTS5 table | Index worker, deployment filter | Trung bình-cao |
| Vector search | Metadata + external ID | Qdrant/embedding worker | Trung bình-thấp ở MVP |
| Static export | Deployment records + Storage artifact | Renderer/export worker/retention | Trung bình |
| PDF export | Export job record | Isolated Chromium or renderer service | Trung bình-thấp |
| Git import | App records/assets | Git provider/auth/conflict/job adapter | Trung bình-thấp |
| Git bidirectional sync | App records + sync metadata | Conflict model, webhooks, previews | Thấp-trung bình |
| Comments/review | `docs_comments` | Anchors, notifications, audit | Cao |
| Analytics | `docs_analytics` | Rate limit, retention, aggregation/sink | Cao ở mức vừa |
| OpenAPI | `docs_openapi.document` JSON | Validation, Scalar/renderer, snapshot inclusion | Cao |
| Usage/billing | Existing/extended platform metadata | Metering and external billing integration | Không nên block migration |

### Nhận xét quan trọng

“Khả thi” ở đây nghĩa là có thể xây dựng trên RustBase, không có nghĩa là tính năng tự xuất hiện sau khi tạo collection. Các tính năng có mức trung bình/thấp đều đòi hỏi subsystem mới, không phải thêm vài field.

---

## 7. Standalone deployment

### 7.1 Site mode

```text
RustBase server
  system.db
  workspace.db = site catalog/members/domains/releases
  app A data.db = version A
  app B data.db = version B
  storage/site/*
```

Site server có thể quản lý nhiều version và tạo site release manifest.

### 7.2 Version mode

```text
RustBase docs-version server
  app data.db
  docs_meta / version manifest
  active snapshot
  version assets
  optional local reader config
```

Version mode không được yêu cầu:

- workspace catalog;
- App khác;
- live page data từ version khác;
- site-wide release database.

Nếu chạy public/read-only, App package là đủ. Nếu muốn standalone authoring đầy đủ, cần quyết định một trong hai:

1. package kèm local identity/membership/auth;
2. version server gọi về site control plane cho admin/auth;
3. standalone authoring chỉ dành cho local owner và không có cross-site membership.

Khuyến nghị rollout:

- P0/P1: standalone public/read-only;
- P2: standalone authoring nếu có yêu cầu thương mại rõ ràng.

### 7.3 Đồng bộ site → App manifest

Workspace là canonical cho site defaults. Mỗi App có `manifest_revision` và `source_workspace_revision`.

Khi site config đổi:

1. tạo manifest update event;
2. worker materialize manifest cho từng App;
3. verify checksum;
4. mark App manifest current;
5. nếu cần public atomicity, publish snapshot mới.

Một App standalone đang chạy vẫn có thể tiếp tục dùng manifest revision cũ cho tới khi được update. Đây là behavior tốt hơn việc public request bị phụ thuộc vào workspace database.

---

## 8. Publish design theo dynamic collections

### 8.1 Draft state

Nguồn draft nằm trong các reserved collections của App:

```text
docs_meta
 docs_pages
 docs_languages
 docs_assets
 docs_redirects
 docs_openapi
 docs_grants
```

Mỗi mutation có thể tăng `content_revision` hoặc dùng updated-at/revision token.

### 8.2 Publish request

Trong cùng App DB:

1. insert `docs_deployments(status='pending')`;
2. insert `docs_jobs(kind='publish', idempotency_key=...)`;
3. commit;
4. API trả deployment ID.

Không tạo deployment READY ngay trong HTTP request.

### 8.3 Worker

Worker đọc một App, validate toàn bộ reserved collections, build normalized snapshot:

```json
{
  "schema_version": 1,
  "site": {},
  "version": {},
  "languages": [],
  "navigation": {},
  "pages": [],
  "assets": [],
  "redirects": [],
  "openapi": {},
  "access_policy": {},
  "source_revision": "...",
  "renderer_version": "..."
}
```

Snapshot ghi vào storage key immutable:

```text
/docs/<site>/<version>/deployments/<deployment-id>/snapshot.json.zst
```

Sau đó update `docs_meta.active_deployment_id` hoặc deployment pointer trong một transaction App.

### 8.4 Site release

Site release chỉ là:

```json
{
  "site": "acme-docs",
  "versions": {
    "v1": {"app_id": "...", "deployment_id": "..."},
    "v2": {"app_id": "...", "deployment_id": "..."}
  }
}
```

Không build lại pages của nhiều App trong một transaction. Đây là yếu tố giúp mỗi version deploy standalone.

---

## 9. Các điểm cần sửa trong RustBase trước khi implementation

### P0 — bắt buộc

1. **Reserved collection bootstrap**
   - khi tạo docs App, tạo toàn bộ reserved collections;
   - verify schema hash khi server start;
   - ngăn generic admin sửa/xóa reserved schema.

2. **Workspace collection scope hoặc workspace native migrations**
   - site catalog/members/domains không nên nhét vào một App tùy ý;
   - không dùng System DB cho site data vì sẽ phá tenant boundary.

3. **Transaction-aware DB API**
   - page tree mutation;
   - deployment pointer;
   - job claim;
   - import batch.

4. **Composite indexes/constraints**
   - đặc biệt `(language_code, path)`;
   - `(parent_id, position, language_code)`;
   - job status/lease indexes.

5. **Fix collection creation cleanup**
   - code hiện tại tạo câu lệnh `DROP TABLE` trong error path nhưng không execute nó;
   - cần transaction hoặc cleanup có kiểm chứng.

6. **Storage metadata/public delivery**
   - content type, size, checksum;
   - presigned upload/download nếu dùng S3;
   - immutable public asset key;
   - reference/garbage collection.

7. **Single-version deployment mode**
   - current boot hierarchy assumes system/workspace/app;
   - thêm adapter explicit cho App package.

### P1

- SQLite FTS5 migration support;
- WAL/busy timeout/backup/restore policy;
- job lease/reaper;
- public host resolver;
- docs-specific auth principal and RBAC;
- JSON/config validation;
- Markdown sanitizer/MDX policy.

### P2

- Qdrant/vector search;
- Git adapters;
- Chromium export;
- ClickHouse analytics sink;
- advanced integrations/MCP.

---

## 10. Hiệu năng và giới hạn vận hành

### 10.1 Điểm mạnh

Một App = một version giúp giảm contention so với một database chứa toàn bộ site:

- authoring v1 không block write v2;
- backup/restore nhỏ hơn;
- snapshot build scope rõ;
- standalone package đơn giản;
- lỗi dữ liệu một version ít lan sang version khác.

Public traffic không nên query `docs_pages` draft. Public reader phải đọc snapshot immutable, có thể cache tại edge.

### 10.2 Điểm yếu

- SQLite không phù hợp cho write concurrency rất cao;
- site release fan-out nhiều App có thể làm search chậm;
- duplicate manifest/assets tăng storage;
- App-local jobs khiến việc điều phối toàn site phức tạp hơn;
- backup nhiều file DB cần manifest/consistency protocol;
- full-text FTS5 index phải rebuild/recover theo từng App.

### 10.3 Khi nào promote dynamic collection thành typed table

Không nên đoán trước. Promote khi có một trong các dấu hiệu:

- query tree/search thường xuyên vượt giới hạn;
- composite constraint không thể diễn tả ổn định bằng schema extension;
- job claim cần throughput/concurrency cao;
- record JSON làm snapshot build quá chậm;
- migration/schema validation trở thành bottleneck;
- cần foreign key/cascade nghiêm ngặt cho data safety.

Các candidate dễ promote nhất:

1. `docs_pages` nếu site lớn hoặc tree mutation nhiều;
2. `docs_jobs` nếu worker scale lớn;
3. `docs_search_documents`/FTS metadata;
4. `docs_deployments` nếu release/audit throughput tăng.

Promotion phải giữ API/service contract; không để UI biết storage là dynamic collection hay typed table.

---

## 11. Bảo mật và data isolation

Dynamic fields làm tăng flexibility nhưng cũng tăng attack surface:

- JSON config không được render trực tiếp thành HTML;
- raw Markdown/MDX phải sanitize/allowlist;
- không cho người dùng tự chọn collection/path để bypass docs authorization;
- reserved docs collection không được public generic read;
- Host header phải resolve qua domain đã verify;
- snapshot cache key phải chứa site/version/deployment/access context cần thiết;
- asset key không được suy ra trực tiếp từ user input;
- reader token chỉ lưu hash;
- Git secrets không lưu trong generic JSON plaintext;
- error response không được leak draft content hoặc private page existence.

Kiểm tra tối thiểu phải bao gồm hai Workspace có cùng page IDs/slugs và hai App có cùng language/path. Không được có cross-tenant/cross-version read.

---

## 12. Ước lượng triển khai

Đây là ước lượng engineering, không phải cam kết lịch:

### MVP có dynamic collections và public publish

- collection scope/bootstrap/schema lock: 2–4 tuần;
- page/tree/language/editor API: 4–6 tuần;
- snapshot/publish/rollback/storage: 5–8 tuần;
- durable jobs + FTS5 + public reader: 5–8 tuần;
- importer fixture và migration tooling: 3–5 tuần;
- auth/RBAC/assets/domain hardening: 4–7 tuần.

Với 2–3 senior engineers và một frontend/QA hỗ trợ: khoảng **5–8 tháng** để có hệ thống production-ready ở phạm vi core docs.

### Feature parity rộng với Nibleaf

Thêm:

- private reader/JWT/audience;
- exports/PDF;
- Git bidirectional sync;
- vector search;
- analytics sink;
- integrations/MCP;
- UI parity và cutover lớn.

Phạm vi này nên dự trù **10–16 tháng** với team 3–5 người có Rust/backend/frontend/DevOps. Một senior developer đơn lẻ có thể làm được nhưng rủi ro lịch và scope rất cao.

---

## 13. Go/no-go criteria

Không nên bắt đầu migration dữ liệu thật trước khi chứng minh được các test sau:

1. Tạo Workspace site → tạo App version → bootstrap reserved collections.
2. Export/import một App version và chạy được ở single-version mode.
3. Tạo/move/reorder page tree không tạo cycle hoặc duplicate path.
4. Publish thành công tạo immutable snapshot và active pointer.
5. Publish fail không làm mất deployment cũ.
6. Rollback tạo revision mới và reader thấy nội dung cũ.
7. Hai Workspace có dữ liệu trùng ID không đọc chéo được.
8. Generic collection endpoint không bypass được docs domain authorization.
9. Worker restart/lease expiry không tạo hai active deployment.
10. Nibleaf fixture import có count/hash/path report có thể kiểm tra tự động.

Nếu 10 điểm này pass, kiến trúc dynamic-collection-first có cơ sở tốt để đi tiếp. Nếu không pass, việc import thêm feature chỉ làm tăng nợ kỹ thuật.

---

## 14. Kết luận cuối cùng

Ý tưởng **follow RustBase DB hierarchy và dynamic fields collections** là đúng hướng và nên được đưa thành quyết định kiến trúc chính.

Nhưng cách triển khai đúng không phải là:

```text
Nibleaf API → generic RustBase CRUD → xong
```

Mà là:

```text
Workspace = site
App = version
Reserved dynamic collections = persistence
Docs domain services = invariants/use cases
Immutable snapshot = public runtime boundary
Durable jobs = asynchronous work
Manifest/package = standalone deployment boundary
```

Với mô hình này:

- không cần port 1:1 toàn bộ 59 Prisma models;
- tận dụng đúng RustBase hierarchy và SQLite isolation;
- giữ được dynamic fields cho page/config/extension;
- vẫn có khả năng đảm bảo tree, publish, rollback và security;
- có thể deploy một site hoặc tách từng version App thành standalone server;
- chỉ promote dynamic collections thành typed tables khi có bằng chứng thực tế.

**Khuyến nghị phê duyệt:** triển khai theo phương án **dynamic-collection-first + domain-service-enforced hybrid**, bắt đầu bằng collection scope/bootstrap, `docs_pages`, page service, snapshot publish và single-version package.

---

## 15. Use-case revision: một site deploy toàn bộ versions như một product

Phần này là đánh giá riêng cho use-case mới được nêu:

> Một site có nhiều versions, nhưng khi release thì release toàn bộ site như một documentation product. Không cần deploy từng version độc lập ở runtime.

Với use-case này, **có thể bỏ mapping `App = version` ở domain level** và đơn giản hóa thành:

```text
Workspace = site
└── Một App được quản lý nội bộ = docs container của site
    └── docs_versions = records
        └── docs_pages.version_id = version record
```

### 15.1 Không nên xóa `App` khỏi RustBase storage hierarchy ngay

Có hai nghĩa khác nhau của “bỏ App-level”:

#### Bỏ App khỏi product/domain model — khuyến nghị

UI/API của documentation chỉ hiển thị:

```text
Site → Version → Page
```

Bên trong RustBase vẫn tạo một App hệ thống, ví dụ:

```text
Workspace: acme-docs
App:       docs
```

App này không đại diện cho một version. Nó là container kỹ thuật duy nhất của documentation site.

Đây là phương án tốt nhất vì:

- RustBase dynamic collections hiện đang gắn với App scope;
- không phải sửa toàn bộ `AppPoolManager`, routes, access guards và storage paths;
- vẫn tận dụng được App SQLite database;
- có thể khóa App khỏi generic dashboard và expose API docs riêng;
- sau này có thể tách version thành App nếu cần.

#### Xóa App khỏi RustBase storage hierarchy hoàn toàn

Khi đó phải chuyển dynamic collections sang Workspace DB:

```text
Workspace
└── Dynamic collections
    └── Records
```

Việc này yêu cầu sửa sâu hơn:

- collection registry phải hỗ trợ workspace scope;
- record API phải nhận `WorkspaceCtx` thay vì chỉ `AppCtx`;
- routes, access rules, realtime, hooks, files và migrations phải hiểu workspace collections;
- cần quyết định conflict giữa workspace system collections và user-defined collections;
- phải kiểm soát namespace/authorization khác với App collections.

Vì vậy đây không còn là “đơn giản hóa” trong implementation. Nó là một thay đổi nền tảng RustBase. Nếu mục tiêu là giảm complexity của Nibleaf migration, không nên chọn phương án này ở phase đầu.

### 15.2 Target model đơn giản hóa

#### Workspace database

- site name/slug/config;
- members/RBAC;
- domains;
- site-level readers/audiences nếu cần;
- site metadata và release catalog nhỏ.

#### Một managed docs App database

| Collection | Thay đổi khi dùng one-App-per-site |
|---|---|
| `docs_versions` | Mỗi version là một record; không còn một RustBase App cho mỗi version. |
| `docs_pages` | Bắt buộc thêm `version_id`. |
| `docs_languages` | Có thể lưu theo site hoặc `(version_id, language_code)`. |
| `docs_assets` | Có thể dùng chung giữa versions; thêm `version_id` nếu version-specific. |
| `docs_deployments` | Mỗi deployment là một site product release chứa toàn bộ versions. |
| `docs_jobs` | Một queue cấp site/App, dễ điều phối hơn. |
| `docs_search_documents` | Một FTS index cấp site, filter theo `version_id`. |
| `docs_comments` | Thêm `version_id` hoặc suy ra qua page. |
| `docs_readers`/`docs_grants` | Policy cấp site, page và version. |

`docs_pages` cần composite uniqueness:

```text
unique(version_id, language_code, path)
index(version_id, language_code, parent_id, position)
```

`parent_id` phải được validate cùng `version_id` và language context. Không được cho page của version A làm parent của version B.

### 15.3 Publish khi toàn bộ site là một product

Deployment trở nên đơn giản hơn:

1. tạo một `docs_deployments` record cho toàn site;
2. chụp source revision của toàn bộ `docs_versions` đang enabled;
3. validate page tree/path/language/redirect/link của toàn bộ versions;
4. build một snapshot chứa tất cả versions;
5. ghi snapshot immutable vào Storage;
6. build/rebuild một FTS index của release đó;
7. atomically update `docs_meta.active_deployment_id`;
8. nếu lỗi, giữ nguyên release cũ.

Snapshot có thể có dạng:

```json
{
  "site": {},
  "release": {},
  "versions": [
    {
      "id": "v1",
      "slug": "v1",
      "languages": [],
      "navigation": {},
      "pages": []
    },
    {
      "id": "v2",
      "slug": "v2",
      "languages": [],
      "navigation": {},
      "pages": []
    }
  ]
}
```

Không còn các phần phức tạp sau:

- cross-App publish consistency;
- workspace site release trỏ tới nhiều App deployments;
- đồng bộ manifest giữa nhiều App;
- search fan-out qua nhiều SQLite databases;
- job discovery ở nhiều App;
- version App bootstrap/fork/delete;
- app-level author permission.

Đây là giảm complexity thực sự, không chỉ giảm số bảng.

### 15.4 Fork version trong one-App model

Fork không còn là clone SQLite database. Nó là:

1. tạo record mới trong `docs_versions`;
2. tạo ID mapping cho page cũ → page mới;
3. copy `docs_pages` với `version_id` mới;
4. copy version-specific assets/config nếu cần;
5. reset deployment/release state;
6. ghi audit event.

Ưu điểm:

- không cần copy file database;
- có thể fork một phần tree;
- dễ giữ site-level assets dùng chung;
- dễ import Git branch vào một version record.

Nhược điểm:

- fork lớn giữ write transaction lâu hơn;
- tất cả versions dùng chung SQLite write lock;
- cần cẩn thận khi copy page references, comments, grants và assets.

Fork nên chạy qua job đối với version lớn, không chạy toàn bộ trong HTTP request.

### 15.5 Complexity giảm bao nhiêu

So với `App = version`, one-managed-App-per-site loại bỏ hoặc giảm đáng kể:

| Thành phần | Tác động |
|---|---|
| Cross-App publish | Loại bỏ hoàn toàn |
| Site release manifest | Đơn giản thành một active deployment pointer |
| Manifest synchronization | Giảm mạnh, chủ yếu là một `docs_meta` |
| Site-wide search | Một FTS/index thay vì fan-out |
| Job scheduling | Một queue cấp site |
| Version creation | Record insert thay vì App provisioning |
| Version fork | Record copy thay vì database clone |
| Auth/RBAC | Site-level đơn giản hơn |
| Standalone raw version DB | Khó hơn |
| SQLite write contention | Tăng lên |
| Blast radius khi DB hỏng | Tăng lên: ảnh hưởng toàn site |

Ước lượng tương đối cho core backend:

- **Giảm 25–40% complexity** so với mô hình mỗi version một App.
- **Giảm 15–30% thời gian triển khai MVP**, tùy mức độ custom domain/private reader/Git/export.
- Không làm mất các phần khó nhất: Markdown/MDX, public reader, storage, job durability, auth và migration.

Đây là ước lượng kiến trúc, cần benchmark bằng fixture thật trước khi dùng làm kế hoạch cam kết.

### 15.6 Tác động đến standalone version trong tương lai

One-App-per-site không làm standalone version bất khả thi, nhưng thay đổi ý nghĩa của “standalone”.

#### Nếu standalone nghĩa là deploy raw version database

Không còn thuận tiện. Database chứa nhiều versions nên phải filter/export một version trước khi deploy.

#### Nếu standalone nghĩa là deploy một version package

Vẫn khả thi và đủ thực tế. Tạo package:

```text
version-package/
  manifest.json
  version-record.json
  pages.ndjson
  languages.json
  assets/
  deployment-snapshot.json.zst
  optional reader-policy.json
```

Server standalone import package đó vào một managed docs App local. Đây là package-level isolation thay vì raw DB-level isolation.

Nên thiết kế `DocsVersionBundle` ngay từ đầu, dù chưa cần App-per-version:

- export version;
- import version;
- checksum;
- source site/version ID;
- renderer/schema version;
- asset manifest;
- URL/path mapping.

Nếu sau này nhu cầu deploy raw database từng version trở thành yêu cầu bắt buộc, có thể thêm migration:

```text
one-site App
→ export DocsVersionBundle
→ create standalone App
→ import bundle
```

### 15.7 Khi nào one-App-per-site là lựa chọn đúng

Nên chọn one managed App per site nếu:

- một product release luôn bao gồm tất cả versions;
- versions thường được publish cùng nhau;
- số versions/site ở mức vài chục hoặc thấp hơn;
- page count/site ở mức hàng trăm đến vài chục nghìn;
- authoring write concurrency thấp/vừa;
- site là deployment/backup/restore boundary;
- standalone version là yêu cầu tương lai, chưa phải hard requirement hiện tại.

Không nên chọn nếu:

- mỗi version có lifecycle/release độc lập;
- mỗi version có customer/owner/auth riêng;
- cần deploy/scale version độc lập thường xuyên;
- một site có hàng trăm versions hoặc rất nhiều page;
- cần raw database handoff cho từng version;
- nhiều team ghi đồng thời vào các versions khác nhau với write throughput cao.

### 15.8 Quyết định đề xuất cho use-case hiện tại

Nếu đúng là documentation được release như **một product duy nhất gồm toàn bộ versions**, tôi khuyến nghị:

```text
Product/domain model:
  Workspace = Site
  Version   = docs_versions record
  Page      = docs_pages record với version_id

RustBase storage implementation:
  Workspace
  └── one managed App named docs
      └── dynamic documentation collections
```

Đây là cách đơn giản hơn, ít invasive hơn và phù hợp hơn với use-case thực tế. Không nên xóa `App` khỏi RustBase core ở phase đầu; chỉ nên ẩn App khỏi docs product model và dùng nó làm technical container.

Phương án này đạt:

- site-level release atomic;
- one snapshot cho toàn bộ documentation product;
- site-level search/export/analytics;
- dễ migrate Nibleaf hơn;
- ít cross-database complexity;
- vẫn giữ đường lui sang standalone version package.

Chỉ nên giữ `App = version` nếu khả năng deploy raw version database hoặc release version độc lập là yêu cầu bắt buộc ngay từ đầu.

---

## 16. Final correction: multi-sites, không thêm tầng Organization

Đính chính quyết định trước: **không cần thêm tầng `Organization` vào RustBase** cho phạm vi hiện tại.

Yêu cầu thực tế là:

```text
Một RustBase installation / single-organization root
├── Workspace = Site A
│   ├── App = Version 1
│   ├── App = Version 2
│   └── App = Version 3
└── Workspace = Site B
    ├── App = Version 1
    └── App = Version 2
```

Hierarchy cuối cùng nên là:

```text
System = single-organization server/root
└── Workspace = documentation site
    └── App = documentation version/branch
        └── Dynamic collections / records
```

### 16.1 Vì sao không thêm Organization

Trong phạm vi hiện tại, `System` đã là root của một deployment và có thể được xem là single-organization boundary. Thêm `Organization` sẽ tạo thêm:

- một context authorization;
- organization membership/invitation;
- organization policies;
- organization ID trong token/route/storage;
- migration cho workspace registry;
- thêm một tầng mà use-case hiện tại chưa cần.

Đó là accidental complexity nếu chưa có yêu cầu multi-organization thực tế. Nếu sau này cần nhiều Organization trong cùng một System, có thể thêm migration khi đó. Không nên thiết kế thêm tầng chỉ vì khả năng giả định.

### 16.2 Mapping chính thức

| Domain | RustBase |
|---|---|
| Server/root/single organization | `System` |
| Documentation site | `Workspace` |
| Documentation version/branch | `App` |
| Page/content/config | Dynamic collections trong App DB |
| Site config/members/domains/releases | Workspace DB hoặc Workspace-scoped collections |
| Public product release | Workspace-level `SiteRelease` |

RustBase hierarchy hiện tại `System → Workspace → App` đã gần như đúng với yêu cầu. Chỉ cần làm rõ semantics:

- một Workspace là một site;
- nhiều Workspace tồn tại trong cùng System;
- một Workspace có nhiều App;
- một App đại diện cho một version;
- App không phải là organization.

### 16.3 App vẫn là Version khi release toàn bộ Site

`App = Version` là physical data/isolation boundary. `SiteRelease` là logical product/publication boundary.

Ví dụ:

```text
Workspace: site-a

App v1 → deployment-88
App v2 → deployment-91
App v3 → deployment-95

SiteRelease release-42:
  v1 → deployment-88
  v2 → deployment-91
  v3 → deployment-95
```

Khi release toàn bộ documentation product:

1. mỗi App tạo một READY deployment;
2. Workspace kiểm tra tất cả version bắt buộc đã READY;
3. tạo `site_release` chứa map `app_id → deployment_id`;
4. đổi `active_release_id` trong Workspace DB;
5. public reader phục vụ đúng các version trong release đó.

Không cần cross-database transaction. Các App deployments đã immutable; Workspace chỉ atomically đổi một release pointer.

### 16.4 Workspace scope và App scope

#### Workspace DB / Workspace collections

```text
site_config
site_members / site_roles
site_domains
site_releases
site_redirects
site_reader_catalog
```

#### App DB / App dynamic collections

```text
docs_version_meta
docs_languages
docs_pages
docs_assets
docs_openapi
docs_comments
docs_readers
docs_grants
docs_deployments
docs_jobs
docs_search_documents
```

RustBase hiện chưa có đầy đủ user-defined dynamic collections ở Workspace scope. Có hai cách đúng:

1. thêm `CollectionScope::Workspace(workspace_id)` và dùng chung dynamic collection engine;
2. dùng first-class Workspace migrations cho site config/members/domains/releases.

Không nên dùng một App tùy ý để chứa site-level data, vì nó làm sai semantics `App = version`.

### 16.5 Context và authorization cuối cùng

Không cần `OrganizationCtx`. Context giữ nguyên hierarchy hiện tại:

```text
SystemCtx
WorkspaceCtx
AppCtx
```

Có thể mở rộng claims bằng:

```text
workspace_id
app_id
role/capabilities
```

Authorization:

- System administrator: quản lý server/root;
- Workspace/site owner/admin: quản lý một site;
- Site editor/reviewer: chỉnh nội dung các version trong site;
- App/version editor: chỉ dùng nếu cần quyền version-specific;
- Public reader: không truy cập generic record API;
- Private reader: được đánh giá qua docs access service.

### 16.6 Storage layout

```text
storage/
  workspaces/<workspace-id>/
    apps/<app-id>/
      assets/
      deployments/
      exports/
```

Workspace-level release metadata nằm trong Workspace DB. App-level snapshot/assets nằm trong App storage. Không cần thêm `organization_id` vào storage key.

### 16.7 Tác động complexity

So với phương án vừa được đề xuất có thêm Organization:

- loại bỏ toàn bộ Organization context/membership/policy;
- không cần sửa token để có `organization_id`;
- không cần migrate Workspace → Organization → Site;
- không cần thêm một storage namespace;
- giữ nguyên RustBase hierarchy hiện tại về mặt nền tảng.

So với one-App-per-site:

- vẫn có thêm complexity ở site release và App orchestration;
- nhưng đổi lại giữ được version isolation, backup/restore và standalone version database;
- complexity này phù hợp với mục tiêu dài hạn.

Với 2–3 senior engineers, core implementation ở mức **5–8 tháng** là khả thi theo phase. Full Nibleaf parity vẫn cần thêm các phase riêng cho reader, exports, Git, vector search, analytics và integrations.

### 16.8 Quyết định cuối cùng

Không thêm `Organization` layer.

Chọn hierarchy:

```text
System
└── Workspace = Site
    └── App = Version
        └── Dynamic documentation collections
```

Dùng:

```text
SiteRelease = product-level publication of all selected Version deployments
```

Đây là thiết kế vừa đủ cho một single-organization RustBase installation có nhiều sites, mỗi site nhiều versions, đồng thời vẫn giữ khả năng deploy standalone từng version khi cần.
