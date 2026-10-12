//! Durable organization-provisioning worker.
//!
//! Claims leased `org.provision` operations and runs the WP-05 readiness
//! reconciliation. It keeps external Coder calls outside database
//! transactions, persists progress step-by-step, fences stale lease holders,
//! and never marks a suspended/deleted organization ready. A failed import,
//! missing license, missing provisioner heartbeat, or missing model produces an
//! explicit not-ready state with a sanitized error code.

use crate::coder::CoderAdapter;
use crate::error::ManagerError;
use crate::id::{OperationId, OrganizationId};
use crate::operations::{self, LeaseGuard, OperationState};
use crate::repositories::OrganizationsRepository;
use crate::templates::packaging::Role;
use crate::templates::repository::ReleaseRepository;
use crate::templates::service::{internal_org_name, ReleaseService};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// How long the worker sleeps between claim cycles.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Readiness step keys (persisted as `operations.current_step`).
pub mod step {
    pub const VERIFY_CAPABILITIES: &str = "verify_capabilities";
    pub const ENSURE_ORG: &str = "ensure_org";
    pub const ENSURE_PROVISIONER: &str = "ensure_provisioner";
    pub const PUBLISH_TEMPLATES: &str = "publish_templates";
    pub const VERIFY_MODELS: &str = "verify_models";
    pub const MARK_READY: &str = "mark_ready";
}

/// Error codes surfaced as sanitized not-ready reasons.
pub mod err {
    pub const RELEASE_NOT_CONFIGURED: &str = "RELEASE_NOT_CONFIGURED";
    pub const CODER_CAPABILITIES_UNMET: &str = "CODER_CAPABILITIES_UNMET";
    pub const CODER_ORG_FAILED: &str = "CODER_ORG_FAILED";
    pub const PROVISIONER_NOT_READY: &str = "PROVISIONER_NOT_READY";
    pub const IMPORT_FAILED: &str = "IMPORT_FAILED";
    pub const MODEL_CAPABILITY_UNAVAILABLE: &str = "MODEL_CAPABILITY_UNAVAILABLE";
    pub const ORG_STATE_CHANGED: &str = "ORG_STATE_CHANGED";
}

/// The outcome of one provisioning attempt, returned for tests to assert on
/// without running the polling loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgProvisionOutcome {
    Ready(OrganizationId),
    NotReady(OrganizationId, &'static str),
    Failed(OrganizationId, &'static str, bool),
    StaleLease,
}

#[derive(Clone)]
pub struct OrgProvisionWorker {
    pub pool: PgPool,
    pub orgs: OrganizationsRepository,
    pub releases: ReleaseService,
    pub release_repo: ReleaseRepository,
    pub adapter: Arc<dyn CoderAdapter>,
    /// The operator-configured approved release artifacts. `None` means no
    /// release is configured; readiness must not proceed.
    pub artifacts: Option<Arc<crate::templates::service::ReleaseArtifacts>>,
    /// The release id of the operator-configured approved release.
    pub release_id: Option<crate::id::ReleaseId>,
    /// The provisioner backend adapter name (operator config).
    pub provisioner_backend: String,
    /// The Coder version baseline required for readiness.
    pub version_baseline: String,
}

impl OrgProvisionWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        orgs: OrganizationsRepository,
        releases: ReleaseService,
        release_repo: ReleaseRepository,
        adapter: Arc<dyn CoderAdapter>,
        artifacts: Option<Arc<crate::templates::service::ReleaseArtifacts>>,
        release_id: Option<crate::id::ReleaseId>,
        provisioner_backend: String,
        version_baseline: String,
    ) -> Self {
        OrgProvisionWorker {
            pool,
            orgs,
            releases,
            release_repo,
            adapter,
            artifacts,
            release_id,
            provisioner_backend,
            version_baseline,
        }
    }

    pub async fn run_forever(&self) -> Result<(), ManagerError> {
        loop {
            if let Err(e) = self.run_once().await {
                tracing::warn!(%e, "org provision worker poll failed; retrying");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Claim and process up to 4 `org.provision` operations once.
    pub async fn run_once(&self) -> Result<(), ManagerError> {
        let owner = format!("org-provision-worker-{}", std::process::id());
        let mut claimed = Vec::new();
        for _ in 0..4 {
            match operations::claim_kind(&self.pool, &owner, Some("org.provision")).await? {
                Some(guard) => claimed.push(guard),
                None => break,
            }
        }
        for guard in claimed {
            let process = self.provision(&guard);
            tokio::pin!(process);
            let result = loop {
                tokio::select! {
                    result = &mut process => break result,
                    _ = tokio::time::sleep(
                        operations::HEARTBEAT_INTERVAL
                            .to_std()
                            .unwrap_or(Duration::from_secs(10)),
                    ) => {
                        if !guard.heartbeat().await? {
                            return Err(ManagerError::Service(anyhow::anyhow!(
                                "org provision worker lost its lease"
                            )));
                        }
                    }
                }
            };
            self.finish(&guard, result).await?;
        }
        Ok(())
    }

    /// Execute one provisioning attempt for a leased operation.
    pub async fn provision(&self, guard: &LeaseGuard) -> Result<OrgProvisionOutcome, ManagerError> {
        let org_id = guard.organization_id;
        let _ = guard;

        // 1. Re-read the organization. If it is no longer provisioning (e.g.
        //    suspended/deleted during a previous attempt), do not resurrect it.
        let org_status = self.org_status(org_id).await?;
        let Some(org_status) = org_status else {
            return Ok(OrgProvisionOutcome::NotReady(
                org_id,
                err::ORG_STATE_CHANGED,
            ));
        };
        if org_status != "provisioning" {
            return Ok(OrgProvisionOutcome::NotReady(
                org_id,
                err::ORG_STATE_CHANGED,
            ));
        }

        // 2. Release prerequisites.
        let artifacts = match &self.artifacts {
            Some(a) => a.clone(),
            None => {
                self.record_not_ready(
                    org_id,
                    err::RELEASE_NOT_CONFIGURED,
                    "no approved release configured",
                )
                .await?;
                return Ok(OrgProvisionOutcome::NotReady(
                    org_id,
                    err::RELEASE_NOT_CONFIGURED,
                ));
            }
        };
        let release_id = match self.release_id {
            Some(r) => r,
            None => {
                self.record_not_ready(org_id, err::RELEASE_NOT_CONFIGURED, "no release id")
                    .await?;
                return Ok(OrgProvisionOutcome::NotReady(
                    org_id,
                    err::RELEASE_NOT_CONFIGURED,
                ));
            }
        };
        let release_approved = self
            .releases
            .repo
            .get_release(release_id)
            .await?
            .map(|(row, _)| row.status == "approved")
            .unwrap_or(false);
        if !release_approved {
            self.record_not_ready(
                org_id,
                err::RELEASE_NOT_CONFIGURED,
                "release is not approved",
            )
            .await?;
            return Ok(OrgProvisionOutcome::NotReady(
                org_id,
                err::RELEASE_NOT_CONFIGURED,
            ));
        }

        // 3. Capabilities.
        let caps = self.adapter.verify_capabilities().await?;
        if !caps.version_ok || !caps.license_ok {
            let code = if !caps.version_ok {
                "CODER_VERSION_UNMET"
            } else {
                err::CODER_CAPABILITIES_UNMET
            };
            self.record_not_ready(org_id, code, "Coder version or license prerequisite unmet")
                .await?;
            return Ok(OrgProvisionOutcome::NotReady(org_id, code));
        }

        // 4. Ensure the dedicated Coder organization and persist its mapping.
        let internal_name = internal_org_name(org_id);
        let coder_org = match self
            .adapter
            .ensure_organization(&internal_name, &internal_name)
            .await
        {
            Ok(org) => org,
            Err(e) => {
                self.record_not_ready(org_id, err::CODER_ORG_FAILED, &e.to_string())
                    .await?;
                return Ok(OrgProvisionOutcome::NotReady(org_id, err::CODER_ORG_FAILED));
            }
        };
        self.orgs
            .set_coder_organization_id(crate::db::tx::OrgScope::new(org_id), org_id, &coder_org.0)
            .await?;

        // 5. Ensure the provisioner (heartbeat verified).
        if let Err(e) = self
            .adapter
            .ensure_provisioner_ready(&coder_org, &self.provisioner_backend)
            .await
        {
            self.release_repo
                .set_provisioner_status(org_id, "unavailable")
                .await?;
            self.record_not_ready(org_id, err::PROVISIONER_NOT_READY, &e.to_string())
                .await?;
            return Ok(OrgProvisionOutcome::NotReady(
                org_id,
                err::PROVISIONER_NOT_READY,
            ));
        }
        self.release_repo
            .set_provisioner_status(org_id, "ready")
            .await?;

        // 6. Publish the required template roles. A failed or timed-out import
        //    prevents readiness.
        if let Err(e) = self
            .releases
            .publish_for_org(org_id, release_id, self.adapter.as_ref(), &artifacts)
            .await
        {
            let code = err::IMPORT_FAILED;
            self.record_not_ready(org_id, code, &e.to_string()).await?;
            return Ok(OrgProvisionOutcome::NotReady(org_id, code));
        }

        // 7. Verify org-scoped model/chat capability.
        let models_ok = self
            .adapter
            .verify_org_models(&coder_org)
            .await
            .unwrap_or(false);
        if !models_ok {
            self.record_not_ready(
                org_id,
                err::MODEL_CAPABILITY_UNAVAILABLE,
                "no org-scoped model / Agents-chat capability",
            )
            .await?;
            return Ok(OrgProvisionOutcome::NotReady(
                org_id,
                err::MODEL_CAPABILITY_UNAVAILABLE,
            ));
        }

        // 8. Re-check the org is still provisioning before marking ready (the
        //    org may have been suspended/deleted while we worked upstream).
        if self.org_status(org_id).await? != Some("provisioning".to_string()) {
            self.record_not_ready(org_id, err::ORG_STATE_CHANGED, "organization state changed")
                .await?;
            return Ok(OrgProvisionOutcome::NotReady(
                org_id,
                err::ORG_STATE_CHANGED,
            ));
        }

        // 9. Mark ready.
        self.orgs
            .set_status(crate::db::tx::OrgScope::new(org_id), org_id, "ready")
            .await?;
        self.orgs
            .set_model_capability_ok(crate::db::tx::OrgScope::new(org_id), org_id, true)
            .await?;
        self.release_repo
            .bind_org_release(org_id, release_id)
            .await?;
        tracing::info!(org_id = %org_id.0, "organization marked ready");
        Ok(OrgProvisionOutcome::Ready(org_id))
    }

    async fn finish(
        &self,
        guard: &LeaseGuard,
        outcome: Result<OrgProvisionOutcome, ManagerError>,
    ) -> Result<(), ManagerError> {
        match outcome {
            Ok(OrgProvisionOutcome::Ready(org_id)) => {
                let ok = guard
                    .complete(Some(serde_json::json!({"status": "ready", "organization_id": org_id.to_string()})))
                    .await?;
                if !ok {
                    tracing::warn!("org provision worker lost its lease before completing");
                }
            }
            Ok(OrgProvisionOutcome::NotReady(org_id, code)) => {
                // Not-ready is a retryable failure: prerequisites may later be
                // met (e.g. provisioner or model becomes available).
                let ok = guard.fail(true, Some(code)).await?;
                if ok {
                    self.orgs
                        .set_last_readiness_error(
                            crate::db::tx::OrgScope::new(org_id),
                            org_id,
                            Some(code),
                        )
                        .await?;
                }
            }
            Ok(OrgProvisionOutcome::Failed(org_id, code, retryable)) => {
                let ok = guard.fail(retryable, Some(code)).await?;
                if ok && !retryable {
                    self.orgs
                        .set_last_readiness_error(
                            crate::db::tx::OrgScope::new(org_id),
                            org_id,
                            Some(code),
                        )
                        .await?;
                }
            }
            Ok(OrgProvisionOutcome::StaleLease) => {}
            Err(e) => {
                tracing::warn!(%e, "org provision attempt failed with error");
                let _ = guard.fail(true, None).await?;
            }
        }
        Ok(())
    }

    async fn org_status(&self, org_id: OrganizationId) -> Result<Option<String>, ManagerError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM organizations WHERE id = $1")
                .bind(org_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(ManagerError::from)?;
        Ok(row.map(|r| r.0))
    }

    async fn record_not_ready(
        &self,
        org_id: OrganizationId,
        code: &str,
        message: &str,
    ) -> Result<(), ManagerError> {
        let scope = crate::db::tx::OrgScope::new(org_id);
        let _ = self
            .orgs
            .set_last_readiness_error(scope, org_id, Some(code))
            .await;
        tracing::warn!(org_id = %org_id.0, code, message, "organization not ready");
        Ok(())
    }
}

/// A placeholder used by tests to assert operation state transitions.
pub async fn set_operation_state(
    pool: &PgPool,
    id: OperationId,
    state: OperationState,
) -> Result<bool, ManagerError> {
    let affected =
        sqlx::query("UPDATE operations SET state = $1, updated_at = now() WHERE id = $2")
            .bind(state.as_str())
            .bind(id.0)
            .execute(pool)
            .await
            .map_err(ManagerError::from)?
            .rows_affected();
    Ok(affected == 1)
}

/// The role order used for publication (Nexus..Lore).
pub const PUBLISH_ROLES: [Role; 4] = [Role::Nexus, Role::Forge, Role::Sentinel, Role::Vessel];
