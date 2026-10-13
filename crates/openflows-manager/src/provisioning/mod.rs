//! Durable organization provisioning and readiness reconciliation.
//!
//! [`OrgProvisionWorker`] claims `org.provision` operations and executes the
//! readiness steps (capabilities, Coder organization, provisioner, template
//! publication, model/chat capability) before marking an organization ready.
//! It is durable and recoverable: progress is persisted, stale lease holders
//! are fenced, and a suspended/deleted organization is never resurrected to
//! ready. The operator-configured release artifacts come from
//! [`ReleaseProvider`], never from customer-supplied URLs.

pub mod release_provider;
pub mod worker;

pub use release_provider::{ReleaseProvider, ReleaseProviderError};
pub use worker::{err, OrgProvisionOutcome, OrgProvisionWorker};
