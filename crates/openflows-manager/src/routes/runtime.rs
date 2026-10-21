//! Runtime credential routes.
//!
//! These endpoints are authenticated by a workspace runtime bearer credential
//! — a principal distinct from human browser/CLI sessions and human API
//! credentials. They deliberately do not consult the human session manager, so
//! a browser cookie or human access token can never authorize a runtime
//! endpoint. Newly issued secrets are returned only to the authenticated
//! runtime, with `Cache-Control: no-store`.

use crate::error::ManagerError;
use crate::routes::auth::bearer_token;
use crate::runtime::permissions::RuntimePurpose;
use crate::server::AppState;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub bootstrap: String,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub workspace_id: String,
    pub access_token: String,
    pub access_expires_at: chrono::DateTime<chrono::Utc>,
    pub refresh_token: String,
    pub refresh_expires_at: chrono::DateTime<chrono::Utc>,
    pub generation: i64,
}

/// Exchange the single-use secret mounted into a workspace for its first
/// short-lived access/refresh session. Bootstrap replay is rejected.
pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Result<Response, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let issued = services
        .runtime_credentials
        .register(&body.bootstrap)
        .await?;
    let mut response = Json(RegisterResponse {
        workspace_id: issued.workspace_id.to_string(),
        access_token: issued.access_secret,
        access_expires_at: issued.access_expires_at,
        refresh_token: issued.refresh_secret,
        refresh_expires_at: issued.refresh_expires_at,
        generation: issued.generation,
    })
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}

/// The body of `POST /runtime/github-credentials`. Only the coarse `purpose` is
/// caller-supplied; all authorization scope (organization, installation,
/// repository, permission profile) is derived server-side.
#[derive(Deserialize)]
pub struct BrokerRequest {
    pub purpose: String,
}

/// The response returned to the authorized runtime. The token must never be
/// logged or persisted by the client beyond its short-lived use.
#[derive(Serialize)]
pub struct BrokerResponse {
    pub token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub repository_id: i64,
    pub username: String,
    pub permissions: std::collections::BTreeMap<String, String>,
}

/// POST /api/v1/runtime/github-credentials
///
/// Authenticated exclusively by a workspace runtime credential. Returns a
/// short-lived GitHub installation token scoped to the workspace's authorized
/// repository and permission profile. Human credentials are rejected.
pub async fn github_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BrokerRequest>,
) -> Result<Response, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;

    let bearer = bearer_token(&headers)
        .ok_or_else(|| ManagerError::api("UNAUTHORIZED", "runtime authentication required"))?;

    // Authenticate as a workspace runtime principal only. This never touches
    // the human session manager, so browser cookies / human tokens are rejected.
    let scope = services.runtime_credentials.authenticate(&bearer).await?;
    if !crate::rate_limit::RateLimiter::new(services.db.pool().clone())
        .allow(
            &scope.workspace_id.to_string(),
            crate::rate_limit::LimitScope::RuntimeBroker,
            30,
        )
        .await?
    {
        return Err(ManagerError::api(
            "RATE_LIMITED",
            "runtime credential request limit exceeded",
        ));
    }

    let purpose = RuntimePurpose::parse(&body.purpose)?;
    let request_id = crate::routes::organizations::request_id(&headers);

    let credential = services
        .broker
        .exchange(&scope, purpose, &request_id)
        .await?;

    let body = BrokerResponse {
        token: credential.token,
        expires_at: credential.expires_at,
        repository_id: credential.repository_id,
        username: credential.username,
        permissions: credential.permissions,
    };
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store"
            .parse()
            .map_err(|_| ManagerError::Service(anyhow::anyhow!("invalid cache-control header")))?,
    );
    Ok(response)
}

/// Authenticate a request as a workspace runtime principal and resolve its full
/// scope. Rejects human credentials (browser cookies / CLI tokens).
async fn authenticate_runtime(
    services: &crate::server::ManagerServices,
    headers: &HeaderMap,
) -> Result<crate::runtime::RuntimeScope, ManagerError> {
    let bearer = bearer_token(headers)
        .ok_or_else(|| ManagerError::api("UNAUTHORIZED", "runtime authentication required"))?;
    services.runtime_credentials.authenticate(&bearer).await
}

/// `POST /runtime/workspaces` — a tenant's Nexus controller requests a worker
/// workspace. All privileged parameters are resolved server-side from the
/// authenticated Nexus runtime scope; a runtime caller cannot select another
/// organization, tenant, repository, or an unauthorized role. Returns 202.
#[derive(Deserialize)]
pub struct WorkerRequest {
    pub role: String,
    #[serde(default)]
    pub slot: Option<i32>,
    #[serde(default)]
    pub ticket_id: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

pub async fn request_worker(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<WorkerRequest>,
) -> Result<Response, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let scope = authenticate_runtime(services, &headers).await?;
    let idempotency_key = crate::routes::organizations::idempotency_key(&headers)?;
    if body.branch.is_some() && !safe_branch(body.branch.as_deref().unwrap_or("")) {
        return Err(ManagerError::InvalidInput(
            "branch contains unsafe characters".into(),
        ));
    }
    let result = services
        .runtime_workspaces
        .request_worker(
            &scope,
            &body.role,
            body.slot,
            body.ticket_id.as_deref(),
            body.branch.as_deref(),
            &idempotency_key,
        )
        .await?;
    let mut response = (
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "workspace_id": result.workspace_id,
            "operation_id": result.operation_id,
            "status": result.status,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store"
            .parse()
            .map_err(|_| ManagerError::Service(anyhow::anyhow!("invalid cache-control header")))?,
    );
    Ok(response)
}

pub async fn get_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace): Path<String>,
) -> Result<Json<crate::runtime::workspaces::RuntimeWorkspaceStatus>, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let scope = authenticate_runtime(services, &headers).await?;
    let workspace_id = crate::runtime::workspaces::parse_workspace_id(&workspace)?;
    Ok(Json(
        services
            .runtime_workspaces
            .get_workspace(&scope, workspace_id)
            .await?,
    ))
}

pub async fn stop_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace): Path<String>,
) -> Result<Response, ManagerError> {
    mutate_workspace(state, headers, workspace, false).await
}

pub async fn delete_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace): Path<String>,
) -> Result<Response, ManagerError> {
    mutate_workspace(state, headers, workspace, true).await
}

async fn mutate_workspace(
    state: AppState,
    headers: HeaderMap,
    workspace: String,
    delete: bool,
) -> Result<Response, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let scope = authenticate_runtime(services, &headers).await?;
    let idempotency_key = crate::routes::organizations::idempotency_key(&headers)?;
    let workspace_id = crate::runtime::workspaces::parse_workspace_id(&workspace)?;
    let result = services
        .runtime_workspaces
        .mutate_worker(&scope, workspace_id, delete, &idempotency_key)
        .await?;
    Ok((axum::http::StatusCode::ACCEPTED, Json(result)).into_response())
}

/// `POST /runtime/heartbeat` — a workspace principal reports its observed
/// readiness. Cannot change ownership or role.
pub async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::runtime::HeartbeatResponse>, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let scope = authenticate_runtime(services, &headers).await?;
    let result = services.runtime_workspaces.heartbeat(&scope).await?;
    Ok(Json(result))
}

/// `POST /runtime/refresh` — rotate the workspace's runtime credential. The new
/// secret is returned exactly once; rotation does not rely on a workspace
/// rebuild.
#[derive(Serialize)]
pub struct RefreshResponse {
    pub workspace_id: String,
    pub access_token: String,
    pub access_expires_at: chrono::DateTime<chrono::Utc>,
    pub refresh_token: String,
    pub refresh_expires_at: chrono::DateTime<chrono::Utc>,
    pub generation: i64,
}

pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let bearer = bearer_token(&headers)
        .ok_or_else(|| ManagerError::api("UNAUTHORIZED", "refresh credential required"))?;
    let issued = services
        .runtime_credentials
        .refresh_session(&bearer)
        .await?;
    let body = RefreshResponse {
        workspace_id: issued.workspace_id.to_string(),
        access_token: issued.access_secret,
        access_expires_at: issued.access_expires_at,
        refresh_token: issued.refresh_secret,
        refresh_expires_at: issued.refresh_expires_at,
        generation: issued.generation,
    };
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store"
            .parse()
            .map_err(|_| ManagerError::Service(anyhow::anyhow!("invalid cache-control header")))?,
    );
    Ok(response)
}

/// `POST /runtime/revoke` — a workspace principal revokes its own runtime
/// issuance (advances the workspace generation so no previously issued
/// credential can authenticate). Used on shutdown and to support rotation
/// without a rebuild.
pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::http::StatusCode, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let scope = authenticate_runtime(services, &headers).await?;
    services
        .runtime_credentials
        .revoke(scope.workspace_id)
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// A conservative safe-branch check: letters, digits, underscores, hyphens,
/// slashes, dots; reject whitespace, control chars, `..`, and leading `-`.
fn safe_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'/' || b == b'.')
}
