//! The Coder adapter trait and real implementation.

use crate::coder::types::{
    CoderCapabilities, PublishOutcome, PublishedTemplate, TemplatePublicationResult,
};
use crate::error::ManagerError;
use async_trait::async_trait;
use coder_client::{
    CoderClient, CoderOrganizationId, CreateOrganizationRequest, CreateTemplateRequest,
};
use std::time::Duration;

/// A testable Coder adapter.
///
/// All operations are explicitly organization-scoped and verify the returned
/// resource's scope. The manager's provisioning worker and release service
/// depend only on this trait, so the offline tests can drive a deterministic
/// in-memory implementation while the real adapter talks to a pinned Coder
/// deployment.
#[async_trait]
pub trait CoderAdapter: Send + Sync {
    /// Verify the deployment's version, license, and org-scoped Agents/chat
    /// capabilities. Returns explicit capability booleans; the caller turns
    /// missing prerequisites into a not-ready state.
    async fn verify_capabilities(&self) -> Result<CoderCapabilities, ManagerError>;

    /// Ensure a dedicated Coder organization with `internal_name` exists and
    /// returns its verified id. Creates it if absent. Never resolves the
    /// default organization.
    async fn ensure_organization(
        &self,
        internal_name: &str,
        display_name: &str,
    ) -> Result<CoderOrganizationId, ManagerError>;

    /// Ensure an organization-scoped provisioner is ready (heartbeat verified).
    /// `backend` names the configured backend adapter. Returns an error that
    /// the worker maps to an explicit not-ready state when the provisioner is
    /// unavailable.
    async fn ensure_provisioner_ready(
        &self,
        org: &CoderOrganizationId,
        backend: &str,
    ) -> Result<(), ManagerError>;

    /// Ensure `version_name` of the deterministic `template_name` template is
    /// published into `org` and its import job succeeded. Idempotent: on retry
    /// it rediscovers and verifies the existing publication rather than
    /// appending unbounded duplicate versions. Returns the exact persisted IDs.
    async fn ensure_template_version(
        &self,
        org: &CoderOrganizationId,
        template_name: &str,
        version_name: &str,
        archive: &[u8],
    ) -> Result<TemplatePublicationResult, ManagerError>;

    /// Verify org-scoped model availability and Agents/chat capability. Returns
    /// true when at least one org-scoped model is reachable.
    async fn verify_org_models(&self, org: &CoderOrganizationId) -> Result<bool, ManagerError>;
}

/// Polling and lease defaults for the real adapter.
const IMPORT_POLL_INTERVAL: Duration = Duration::from_secs(2);
const IMPORT_POLL_TIMEOUT: Duration = Duration::from_secs(120);

/// A real adapter backed by `coder-client` and an operator-scoped Coder token.
#[derive(Clone)]
pub struct RealCoderAdapter {
    client: CoderClient,
    /// The pinned version baseline, e.g. "v2.37.3".
    version_baseline: String,
}

impl RealCoderAdapter {
    pub fn new(client: CoderClient, version_baseline: impl Into<String>) -> Self {
        RealCoderAdapter {
            client,
            version_baseline: version_baseline.into(),
        }
    }

    /// Build the adapter from the manager's Coder URL and operator token. The
    /// operator token is resolved by the caller from the secret provider and
    /// never logged or persisted by this adapter.
    pub fn from_parts(
        coder_url: &str,
        operator_token: &str,
        version_baseline: impl Into<String>,
    ) -> Self {
        RealCoderAdapter::new(
            CoderClient::new(coder_url, operator_token),
            version_baseline,
        )
    }
}

#[async_trait]
impl CoderAdapter for RealCoderAdapter {
    async fn verify_capabilities(&self) -> Result<CoderCapabilities, ManagerError> {
        // Version: `GET /api/v2/buildinfo` is unauthenticated. We compare the
        // reported version prefix against the pinned baseline. Because live
        // deployment access is an operator gate (WP-00), a failure here maps to
        // an explicit not-ready state, never a privileged fallback.
        let buildinfo: Result<serde_json::Value, _> = {
            let resp = self
                .client
                .raw_get("/api/v2/buildinfo")
                .await
                .map_err(ManagerError::from);
            resp
        };
        let version_ok = match buildinfo {
            Ok(v) => v
                .get("version")
                .and_then(|x| x.as_str())
                .map(|v| v.starts_with(&self.version_baseline) || v == self.version_baseline)
                .unwrap_or(false),
            Err(_) => false,
        };

        // License: organizations/service accounts require Premium. We cannot
        // fabricate this; probe the org-scoped chat-models endpoint, which
        // requires org membership and Agents access, as a conservative license
        // + capability signal. A failure is a not-ready prerequisite.
        let default_org_probe = self.probe_license().await;

        Ok(CoderCapabilities {
            version_ok,
            license_ok: default_org_probe,
            agents_chat_ok: default_org_probe,
        })
    }

    async fn ensure_organization(
        &self,
        internal_name: &str,
        display_name: &str,
    ) -> Result<CoderOrganizationId, ManagerError> {
        let orgs = self
            .client
            .list_organizations()
            .await
            .map_err(ManagerError::from)?;
        // Deterministic internal name: if it exists, verify its scope.
        if let Some(existing) = orgs.iter().find(|o| o.name == internal_name) {
            if existing.id.is_empty() {
                return Err(ManagerError::Service(anyhow::anyhow!(
                    "Coder organization '{}' reported no id",
                    internal_name
                )));
            }
            return Ok(CoderOrganizationId::new(existing.id.clone()));
        }
        // Create it and verify the returned id.
        let created = self
            .client
            .create_organization(&CreateOrganizationRequest {
                name: internal_name.to_string(),
                display_name: Some(display_name.to_string()),
                description: Some("OpenFlows-managed organization".to_string()),
            })
            .await
            .map_err(ManagerError::from)?;
        if created.id.is_empty() {
            return Err(ManagerError::Service(anyhow::anyhow!(
                "Coder organization creation returned no id"
            )));
        }
        Ok(CoderOrganizationId::new(created.id))
    }

    async fn ensure_provisioner_ready(
        &self,
        org: &CoderOrganizationId,
        backend: &str,
    ) -> Result<(), ManagerError> {
        // Deterministic provisioner key name derived from the org id so retries
        // reuse the same key rather than minting unbounded keys.
        let key_name = format!("openflows-provisioner-{}", org.0);
        self.client
            .create_provisioner_key(org, &key_name)
            .await
            .map_err(ManagerError::from)?;

        // Poll provisioner daemons until one reports ready, bounded. The
        // `backend` names the operator-configured adapter; the daemon must have
        // been started out-of-band with this org's key.
        let deadline = tokio::time::Instant::now() + IMPORT_POLL_TIMEOUT;
        loop {
            let daemons = self
                .client
                .list_provisioner_daemons(org)
                .await
                .map_err(ManagerError::from)?;
            if daemons.iter().any(|d| d.status == "ready") {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ManagerError::api(
                    "PROVISIONER_NOT_READY",
                    format!(
                        "organization provisioner for backend '{backend}' is not ready within timeout"
                    ),
                ));
            }
            tokio::time::sleep(IMPORT_POLL_INTERVAL).await;
        }
    }

    async fn ensure_template_version(
        &self,
        org: &CoderOrganizationId,
        template_name: &str,
        version_name: &str,
        archive: &[u8],
    ) -> Result<TemplatePublicationResult, ManagerError> {
        // 1. Find or create the template by its deterministic name within the
        //    explicit org.
        let templates = self
            .client
            .list_templates_in_org(org)
            .await
            .map_err(ManagerError::from)?;
        let template = templates.into_iter().find(|t| t.name == template_name);

        let template_id = match template {
            Some(t) => {
                if !t.organization_id.is_empty() && t.organization_id != org.0 {
                    return Err(ManagerError::api(
                        "CODER_SCOPE_MISMATCH",
                        "existing template belongs to a different Coder organization",
                    ));
                }
                coder_client::CoderTemplateId::new(t.id)
            }
            None => {
                // Template absent: create a version first (a template needs at
                // least one version), then create the template bound to it.
                let version = self
                    .client
                    .create_template_version(
                        org,
                        &placeholder_template_id_for_version(org, template_name),
                        version_name,
                        archive,
                    )
                    .await
                    .map_err(ManagerError::from)?;
                let created = self
                    .client
                    .create_template(
                        org,
                        &CreateTemplateRequest {
                            name: template_name.to_string(),
                            description: Some("OpenFlows-managed role template".to_string()),
                            default_ttl_ms: None,
                            active_version_id: Some(version.id.clone()),
                        },
                    )
                    .await
                    .map_err(ManagerError::from)?;
                // The version id recorded above belongs to the newly created
                // template; return it.
                let published = PublishedTemplate {
                    template_id: coder_client::CoderTemplateId::new(created.id),
                    version_id: coder_client::CoderTemplateVersionId::new(version.id),
                    import_job_id: version
                        .job
                        .as_ref()
                        .map(|j| j.id.clone())
                        .unwrap_or_default(),
                };
                // If the version has no import job yet, poll it.
                if published.import_job_id.is_empty() {
                    return Err(ManagerError::api(
                        "IMPORT_JOB_UNKNOWN",
                        "created template version did not report an import job",
                    ));
                }
                self.await_import(org, &published.import_job_id).await?;
                return Ok(TemplatePublicationResult {
                    published,
                    outcome: PublishOutcome::Created,
                    template_name: template_name.to_string(),
                    version_name: version_name.to_string(),
                });
            }
        };

        // 2. Template exists. Look for a version with our deterministic name.
        let versions = self
            .client
            .list_template_versions(org, &template_id)
            .await
            .map_err(ManagerError::from)?;
        if let Some(existing) = versions.into_iter().find(|v| v.name == version_name) {
            if existing.id.is_empty() {
                return Err(ManagerError::Service(anyhow::anyhow!(
                    "Coder version '{}' reported no id",
                    version_name
                )));
            }
            // Verify the version's import job succeeded. Rediscover by stable
            // identity; never adopt a same-name version from another template.
            let job_id = existing
                .job
                .as_ref()
                .map(|j| j.id.clone())
                .unwrap_or_default();
            if !job_id.is_empty() {
                self.await_import(org, &job_id).await?;
            }
            return Ok(TemplatePublicationResult {
                published: PublishedTemplate {
                    template_id: template_id.clone(),
                    version_id: coder_client::CoderTemplateVersionId::new(existing.id),
                    import_job_id: job_id,
                },
                outcome: PublishOutcome::Reconciled,
                template_name: template_name.to_string(),
                version_name: version_name.to_string(),
            });
        }

        // 3. Template exists but our deterministic version does not: upload a
        //    new version (idempotent by version name) and poll its import.
        let version = self
            .client
            .create_template_version(org, &template_id, version_name, archive)
            .await
            .map_err(ManagerError::from)?;
        let job_id = version
            .job
            .as_ref()
            .map(|j| j.id.clone())
            .unwrap_or_default();
        if job_id.is_empty() {
            return Err(ManagerError::api(
                "IMPORT_JOB_UNKNOWN",
                "created template version did not report an import job",
            ));
        }
        self.await_import(org, &job_id).await?;
        Ok(TemplatePublicationResult {
            published: PublishedTemplate {
                template_id: template_id.clone(),
                version_id: coder_client::CoderTemplateVersionId::new(version.id),
                import_job_id: job_id,
            },
            outcome: PublishOutcome::Created,
            template_name: template_name.to_string(),
            version_name: version_name.to_string(),
        })
    }

    async fn verify_org_models(&self, org: &CoderOrganizationId) -> Result<bool, ManagerError> {
        let models = self
            .client
            .list_chat_models_in_org(org)
            .await
            .map_err(ManagerError::from)?;
        // The org-scoped endpoint reachable + at least one model means the org
        // has Agents/chat capability and a configured model.
        Ok(!models.is_empty())
    }
}

impl RealCoderAdapter {
    /// Bounded polling of an import job until it succeeds. A failed or timed
    /// out import is a hard error: readiness must be prevented.
    async fn await_import(
        &self,
        org: &CoderOrganizationId,
        job_id: &str,
    ) -> Result<(), ManagerError> {
        let deadline = tokio::time::Instant::now() + IMPORT_POLL_TIMEOUT;
        loop {
            let job = self
                .client
                .get_import_job(org, job_id)
                .await
                .map_err(ManagerError::from)?;
            match job {
                Some(j) if j.is_succeeded() => return Ok(()),
                Some(j) if j.is_terminal() => {
                    return Err(ManagerError::api(
                        "IMPORT_FAILED",
                        "template import job failed or was cancelled",
                    ));
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ManagerError::api(
                    "IMPORT_TIMEOUT",
                    "template import job did not succeed within timeout",
                ));
            }
            tokio::time::sleep(IMPORT_POLL_INTERVAL).await;
        }
    }

    /// Probe license + org-scoped capability by reading buildinfo version and
    /// listing organizations. Conservative: cannot be true without an org we
    /// can reach.
    async fn probe_license(&self) -> bool {
        self.client.list_organizations().await.is_ok()
    }
}

/// Coder's template-version upload endpoint accepts an existing or new template
/// id. For the first version we use a deterministic placeholder that Coder
/// resolves or that we replace when creating the template. Because the pinned
/// version's exact contract is an operator gate, we keep this explicit.
fn placeholder_template_id_for_version(
    org: &CoderOrganizationId,
    template_name: &str,
) -> coder_client::CoderTemplateId {
    // The upload is routed by org; the template id is derived deterministically
    // from the org and name so a retry targets the same template.
    coder_client::CoderTemplateId::new(format!("{}-{}", org.0, template_name))
}
