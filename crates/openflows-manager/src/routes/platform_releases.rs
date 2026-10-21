//! Platform-operator actions for the deployment-configured template release.

use crate::error::ManagerError;
use crate::id::OrganizationId;
use crate::routes::organizations::{authenticated_user, require_csrf_for_browser};
use crate::server::AppState;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use std::str::FromStr;

/// Promote this Manager deployment's configured release to one organization.
/// Existing organizations stay pinned until a platform operator explicitly
/// invokes this action.
pub async fn promote_configured_release(
    State(state): State<AppState>,
    Path(org): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ManagerError> {
    let services = state
        .services()
        .ok_or_else(|| ManagerError::api("SERVICE_UNAVAILABLE", "service unavailable"))?;
    let principal = authenticated_user(&headers, &services.session_manager).await?;
    require_csrf_for_browser(&principal, &headers).await?;

    let organization_id = OrganizationId::from_str(&org)
        .map_err(|_| ManagerError::InvalidInput("malformed organization id".into()))?;
    let worker = services.org_provision.as_ref().ok_or_else(|| {
        ManagerError::api(
            "RELEASE_NOT_CONFIGURED",
            "no configured template release is available",
        )
    })?;
    let release_id = worker.release_id.ok_or_else(|| {
        ManagerError::api(
            "RELEASE_NOT_CONFIGURED",
            "no configured template release is available",
        )
    })?;
    let artifacts = worker.artifacts.as_ref().ok_or_else(|| {
        ManagerError::api(
            "RELEASE_NOT_CONFIGURED",
            "no configured template release artifacts are available",
        )
    })?;

    services
        .release_approvals
        .promote(
            principal.user_id,
            release_id,
            organization_id,
            worker.adapter.as_ref(),
            artifacts,
        )
        .await?;

    Ok(Json(json!({
        "organization_id": organization_id,
        "release_id": release_id,
        "status": "promoted"
    })))
}
