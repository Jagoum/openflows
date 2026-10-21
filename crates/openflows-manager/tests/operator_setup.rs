//! Exercise the real CLI against a bounded fake Coder API, without operator secrets.
use axum::{
    extract::State,
    http::{Method, StatusCode},
    routing::any,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Fake {
    app: Option<Value>,
    app_creates: usize,
    secret_creates: usize,
    writes: usize,
    initialized: bool,
    bootstrap_creates: usize,
    token_creates: usize,
}

async fn api(
    State(state): State<Arc<Mutex<Fake>>>,
    method: Method,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    let mut state = state.lock().unwrap();
    let path = uri.path();
    if method != Method::GET {
        state.writes += 1;
    }
    match (method, path) {
        (Method::GET, "/api/v2/users/first") => (
            if state.initialized {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
            Json(json!({})),
        ),
        (Method::POST, "/api/v2/users/first") => {
            state.initialized = true;
            state.bootstrap_creates += 1;
            (
                StatusCode::CREATED,
                Json(json!({"user_id":"00000000-0000-0000-0000-000000000001"})),
            )
        }
        (Method::POST, "/api/v2/users/login") => (
            StatusCode::OK,
            Json(json!({"session_token":"bootstrap-never-log"})),
        ),
        (Method::POST, "/api/v2/users/00000000-0000-0000-0000-000000000001/keys/tokens") => {
            state.token_creates += 1;
            (
                StatusCode::CREATED,
                Json(json!({"key":"operator-never-log"})),
            )
        }
        (Method::GET, "/api/v2/users/me") => (
            StatusCode::OK,
            Json(json!({"id":"00000000-0000-0000-0000-000000000001"})),
        ),
        (Method::GET, "/api/v2/oauth2-provider/apps") => (
            StatusCode::OK,
            Json(json!(state.app.iter().collect::<Vec<_>>())),
        ),
        (Method::POST, "/api/v2/oauth2-provider/apps") => {
            state.app_creates += 1;
            let mut app = body.unwrap().0;
            let name = app["name"].as_str().unwrap();
            assert!(name.len() <= 32);
            assert!(name.starts_with("of-"));
            assert!(name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'));
            assert_eq!(
                app["callback_url"],
                "https://flows.test/auth/coder/callback"
            );
            app["id"] = json!("00000000-0000-0000-0000-000000000002");
            state.app = Some(app.clone());
            (StatusCode::CREATED, Json(app))
        }
        (Method::GET, "/api/v2/oauth2-provider/apps/00000000-0000-0000-0000-000000000002") => {
            (StatusCode::OK, Json(state.app.clone().unwrap()))
        }
        (
            Method::POST,
            "/api/v2/oauth2-provider/apps/00000000-0000-0000-0000-000000000002/secrets",
        ) => {
            state.secret_creates += 1;
            (
                StatusCode::CREATED,
                Json(json!({"client_secret_full":"test-secret-never-log"})),
            )
        }
        _ => (StatusCode::NOT_FOUND, Json(json!({}))),
    }
}

fn command(dir: &std::path::Path, operation: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_openflows-manager"));
    command
        .env_clear()
        .current_dir(dir)
        .arg("--config")
        .arg("deployment.env")
        .arg(operation);
    command
}

#[tokio::test]
async fn repeated_setup_reuses_keys_app_and_secret_and_refuses_deployment_switch() {
    let state = Arc::new(Mutex::new(Fake::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().fallback(any(api)).with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join(".openflows/secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::write(
        secrets.join("coder-operator-token"),
        "test-operator-never-log",
    )
    .unwrap();
    let config = format!("OPENFLOWS_MODE=local\nCODER_URL=http://{address}\nOPENFLOWS_PUBLIC_URL=https://flows.test\n");
    std::fs::write(dir.path().join("deployment.env"), &config).unwrap();
    for _ in 0..2 {
        let output = command(dir.path(), "setup").output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("never-log"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("never-log"));
    }
    assert_eq!(state.lock().unwrap().app_creates, 1);
    assert_eq!(state.lock().unwrap().secret_creates, 1);
    // An interrupted setup with the app saved but no secret resumes at that step.
    std::fs::remove_file(secrets.join("coder-oauth-client-secret")).unwrap();
    let output = command(dir.path(), "setup").output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(state.lock().unwrap().app_creates, 1);
    assert_eq!(state.lock().unwrap().secret_creates, 2);
    let writes = state.lock().unwrap().writes;
    let mut diagnostic = command(dir.path(), "doctor");
    diagnostic.env("OPENFLOWS_PROVISIONER_BACKEND", "test");
    let output = diagnostic.output().await.unwrap();
    // This fixture has no DB, Redis or GitHub secrets, so doctor must fail while
    // still checking Coder, without invoking any of the setup mutations.
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("PASS Coder identity"));
    assert_eq!(state.lock().unwrap().writes, writes);
    assert_eq!(std::fs::read(secrets.join("master-key")).unwrap().len(), 32);
    let saved = std::fs::read(secrets.join("deployment.json")).unwrap();
    std::fs::write(
        dir.path().join("deployment.env"),
        config.replace("flows.test", "another.test"),
    )
    .unwrap();
    let output = command(dir.path(), "setup").output().await.unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read(secrets.join("deployment.json")).unwrap(),
        saved
    );
    server.abort();
}

#[tokio::test]
async fn explicit_missing_configuration_never_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env.prod"), "OPENFLOWS_MODE=local\n").unwrap();
    let output = command(dir.path(), "setup").output().await.unwrap();
    assert!(!output.status.success());
    assert!(!dir.path().join(".openflows").exists());
}

#[tokio::test]
async fn fresh_bootstrap_persists_credentials_and_does_not_repeat_admin_creation() {
    let state = Arc::new(Mutex::new(Fake::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().fallback(any(api)).with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("deployment.env"), format!(
        "OPENFLOWS_MODE=local\nCODER_URL=http://{address}\nOPENFLOWS_PUBLIC_URL=https://flows.test\n"
    )).unwrap();
    for _ in 0..2 {
        let output = command(dir.path(), "setup")
            .args(["--initialize-coder", "--admin-email", "admin@example.test"])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("never-log"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("never-log"));
    }
    let state = state.lock().unwrap();
    assert_eq!(state.bootstrap_creates, 1);
    assert_eq!(state.token_creates, 1);
    assert_eq!(state.app_creates, 1);
    let root = dir.path().join(".openflows/secrets");
    assert!(root.join("coder-bootstrap-password").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.join("coder-operator-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    server.abort();
}
