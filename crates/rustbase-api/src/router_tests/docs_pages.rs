use super::*;
use crate::router::*;
use axum::http::StatusCode;
use tower::ServiceExt;

fn page_body(language: &str, path: &str, parent_id: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "language_code": language,
        "parent_id": parent_id,
        "kind": "page",
        "title": path.trim_matches('/'),
        "slug": path.trim_matches('/').replace('/', "-"),
        "path": path,
        "position": 0.0,
        "description": null,
        "icon": null,
        "hidden": false,
        "content": "# Content",
        "content_format": "markdown",
        "config": null,
        "translation_key": null,
        "source_metadata": null
    })
}

#[tokio::test]
async fn docs_page_api_enforces_tree_invariants() {
    let (state, _dir, _, workspace_admin_id) = state_with_workspace_and_admin().await;
    let token = workspace_token(&state, "acme", &workspace_admin_id);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&page_body("en", "/guide", None)),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let root = json_body(response).await;
    let root_id = root["id"].as_str().unwrap().to_string();

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&page_body("en", "/guide/start", Some(&root_id))),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let child = json_body(response).await;
    let child_id = child["id"].as_str().unwrap().to_string();

    // A page cannot be moved below its own descendant.
    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "PATCH",
            &format!("/api/workspaces/acme/apps/mobile/docs/pages/{root_id}"),
            Some(&token),
            Some(&serde_json::json!({"parent_id": child_id})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // A parent cannot be deleted while it has children.
    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "DELETE",
            &format!("/api/workspaces/acme/apps/mobile/docs/pages/{root_id}"),
            Some(&token),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // The generic records surface cannot bypass docs page invariants.
    let app = build_router(state);
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/collections/docs_pages/records",
            Some(&token),
            Some(&serde_json::json!({})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn docs_publish_and_public_reader_use_immutable_snapshot() {
    let (state, _dir, _, workspace_admin_id) = state_with_workspace_and_admin().await;
    let token = ensure_mobile_app(&state, &workspace_admin_id).await;

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&page_body("en", "/guide", None)),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/publish",
            Some(&token),
            Some(&serde_json::json!({"idempotency_key":"publish-guide-v1"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let published = json_body(response).await;
    assert_eq!(published["reused"], false);
    assert!(published["checksum"].as_str().is_some());

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth("GET", "/docs/acme/mobile/guide", None, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("public")
    );
    let public_page = json_body(response).await;
    assert_eq!(public_page["page"]["fields"]["path"], "/guide");
    assert_eq!(public_page["deployment_id"], published["deployment"]["id"]);

    // Replaying the same idempotency key returns the same immutable deployment.
    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/publish",
            Some(&token),
            Some(&serde_json::json!({"idempotency_key":"publish-guide-v1"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let replay = json_body(response).await;
    assert_eq!(replay["reused"], true);
    assert_eq!(replay["deployment"]["id"], published["deployment"]["id"]);
}

#[tokio::test]
async fn docs_release_pins_a_historical_ready_snapshot() {
    let (state, _dir, _, workspace_admin_id) = state_with_workspace_and_admin().await;
    let token = ensure_mobile_app(&state, &workspace_admin_id).await;
    let mut home = page_body("en", "/", None);
    home["slug"] = serde_json::json!("home");
    home["title"] = serde_json::json!("Home");

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&home),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/publish",
            Some(&token),
            Some(&serde_json::json!({"idempotency_key":"release-source-v1"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let first_publish = json_body(response).await;
    let first_deployment_id = first_publish["deployment"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/docs/releases",
            Some(&token),
            Some(&serde_json::json!({
                "deployments":{"mobile":first_deployment_id},
                "default_app_id":"mobile",
                "default_language_code":"en"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // A newer App deployment supersedes the old row, but the release still
    // points at the immutable object selected while it was READY.
    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&page_body("en", "/new", None)),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/publish",
            Some(&token),
            Some(&serde_json::json!({"idempotency_key":"release-source-v2"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            &format!(
                "/api/workspaces/acme/apps/mobile/docs/deployments/{first_deployment_id}/rollback"
            ),
            Some(&token),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let rollback = json_body(response).await;
    assert_ne!(rollback["id"], first_deployment_id);

    let app = build_router(state);
    let response = app
        .oneshot(req_with_auth("GET", "/docs/acme", None, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page = json_body(response).await;
    assert_eq!(page["deployment_id"], first_deployment_id);
    assert_eq!(page["page"]["fields"]["path"], "/");
}

#[tokio::test]
async fn private_docs_require_a_reader_session_and_snapshot_grant() {
    let (state, _dir, _, workspace_admin_id) = state_with_workspace_and_admin().await;
    let token = ensure_mobile_app(&state, &workspace_admin_id).await;

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "PUT",
            "/api/workspaces/acme/docs/site",
            Some(&token),
            Some(&serde_json::json!({
                "name":"Acme",
                "slug":"acme",
                "default_app_id":"mobile",
                "default_language_code":"en",
                "access_mode":"private"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&page_body("en", "/guide", None)),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/readers/grants",
            Some(&token),
            Some(&serde_json::json!({
                "subject":"reader-1",
                "path_prefix":"/guide"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/readers/sessions",
            Some(&token),
            Some(&serde_json::json!({"subject":"reader-1","ttl_seconds":3600})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let session = json_body(response).await;
    let reader_token = session["token"].as_str().unwrap().to_string();

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/publish",
            Some(&token),
            Some(&serde_json::json!({"idempotency_key":"private-v1"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth("GET", "/docs/acme/mobile/guide", None, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let app = build_router(state);
    let response = app
        .oneshot(req_with_auth(
            "GET",
            "/docs/acme/mobile/guide",
            Some(&reader_token),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("private")
    );
}

#[tokio::test]
async fn docs_page_paths_are_unique_per_language() {
    let (state, _dir, _, workspace_admin_id) = state_with_workspace_and_admin().await;
    let token = workspace_token(&state, "acme", &workspace_admin_id);
    let body = page_body("en", "/same", None);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&body),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let app = build_router(state.clone());
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&body),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // The same path is valid in a different language.
    let app = build_router(state);
    let response = app
        .oneshot(req_with_auth(
            "POST",
            "/api/workspaces/acme/apps/mobile/docs/pages",
            Some(&token),
            Some(&page_body("fr", "/same", None)),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}
