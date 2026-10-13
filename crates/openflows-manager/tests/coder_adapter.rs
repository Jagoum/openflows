//! HTTP response regressions for the real Coder adapter, without live credentials.
use axum::{routing::get, Json, Router};
use coder_client::CoderOrganizationId;
use openflows_manager::coder::{CoderAdapter, RealCoderAdapter};
use serde_json::json;
use std::time::Duration;

async fn fixture(router: Router) -> (RealCoderAdapter, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (
        RealCoderAdapter::from_parts(&base, "fixture-token", "v2.37.3"),
        server,
    )
}

#[tokio::test]
async fn idle_and_busy_daemons_with_the_expected_key_are_online() {
    for status in ["idle", "busy"] {
        let router = Router::new().route(
            "/api/v2/organizations/org/provisionerdaemons",
            get(move || async move {
                Json(json!([
                    {"id":"unkeyed", "status":null, "key_name":null, "last_seen_at":null},
                    {"id":"daemon", "status":status, "key_name":"openflows-provisioner-org"}
                ]))
            }),
        );
        let (adapter, server) = fixture(router).await;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            adapter.ensure_provisioner_ready(&CoderOrganizationId::new("org"), "docker"),
        )
        .await;
        server.abort();
        result
            .expect("online daemon must pass without polling")
            .unwrap();
    }
}

#[tokio::test]
async fn wrong_key_and_offline_daemons_do_not_pass_readiness() {
    for (status, key) in [
        ("idle", "another-key"),
        ("offline", "openflows-provisioner-org"),
        ("ready", "openflows-provisioner-org"),
    ] {
        let router =
            Router::new().route(
                "/api/v2/organizations/org/provisionerdaemons",
                get(move || async move {
                    Json(json!([{"id":"daemon", "status":status, "key_name":key}]))
                }),
            );
        let (adapter, server) = fixture(router).await;
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            adapter.ensure_provisioner_ready(&CoderOrganizationId::new("org"), "docker"),
        )
        .await;
        server.abort();
        assert!(result.is_err(), "unqualified daemon must remain pending");
    }
}

#[tokio::test]
async fn existing_versions_require_import_job_evidence() {
    for job in [
        serde_json::Value::Null,
        json!({"id":"", "status":"succeeded"}),
    ] {
        let router = Router::new()
            .route("/api/v2/organizations/org/templates", get(|| async {
                Json(json!([{"id":"template", "name":"openflows-nexus", "organization_id":"org"}]))
            }))
            .route("/api/v2/organizations/org/templates/template/versions", get(move || {
                let job = job.clone();
                async move { Json(json!([{"id":"version", "name":"release", "job":job}])) }
            }));
        let (adapter, server) = fixture(router).await;
        let result = adapter
            .ensure_template_version(
                &CoderOrganizationId::new("org"),
                "openflows-nexus",
                "release",
                &[],
            )
            .await;
        server.abort();
        let error = result.unwrap_err();
        assert!(
            matches!(error, openflows_manager::error::ManagerError::Api(ref e) if e.code == "IMPORT_JOB_UNKNOWN")
        );
    }
}
