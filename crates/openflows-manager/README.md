# Manager foundations and human authorization

The manager owns the Openflows product database, separately from Coder. It adds
GitHub human sign-in, browser and CLI sessions, device approval, organization
membership, and invitations. GitHub installation connections, Coder
provisioning workers, and the complete hosted CLI remain later packages.

It also adds runtime authentication and the GitHub credential broker: a
workspace runtime principal (distinct from human sessions) that obtains
short-lived GitHub installation tokens scoped to its authorized repository and
permission profile. See [runtime credentials and the GitHub credential
broker](../../docs/architecture/runtime-credentials-broker.md).

## Validation

Migration `0012_current_organization_release.sql` adds the organization release
selection. Publication history is retained in `organization_release_bindings`;
the single current selection lives in `organization_current_releases`. Existing
bindings are backfilled by most recent creation time, with release ID breaking
ties. Successful organization readiness, release selection, and operation
completion are now committed together. Startup preserves retired releases.

The current real Coder adapter expects an operator-managed provisioner daemon
using the key name `openflows-provisioner-<coder-organization-id>`. Only `idle`
or `busy` daemons with that key qualify as online. Readiness does not create or
discard provisioner credentials. Automatic backend deployment and the real
template upload API remain unfinished work; passing fixture tests does
not establish live Coder compatibility.

Ordinary tests require no PostgreSQL server:

```sh
cargo test -p openflows-manager
cargo clippy -p openflows-manager --all-targets --all-features -- -D warnings
```

Run the PostgreSQL integration suite with:

```sh
bash crates/openflows-manager/scripts/run-integration-tests.sh
```

Without `OPENFLOWS_TEST_DATABASE_URL`, the runner starts the Compose
`openflows-db` service, waits at most 30 seconds for readiness, and uses the
local development credentials. Its published port binds to loopback by default;
`OPENFLOWS_PG_BIND_ADDRESS` explicitly changes that binding. If using a custom
password, supply a correctly encoded `OPENFLOWS_TEST_DATABASE_URL`.

With that variable set, the runner uses the supplied test server without
starting or stopping Compose services and never prints the URL. The role must
have CREATEDB permission. Each test creates a unique `of_test_*` database and
synchronously drops only that database on teardown, including during unwinding.
Use a dedicated test server. Stop the bundled service with
`docker compose --profile manager stop openflows-db`.

CI has a dedicated PostgreSQL 16 service/job that explicitly runs the ignored
database tests as well as the health tests.

## Transaction and lease contracts

`IdempotencyService::execute` owns a transaction and passes its connection to
the mutation callback. The callback returns a nonempty result reference. Use
that connection for all tenant, operation, audit, and outbox writes; do not use
a repository's standalone pool method from inside the callback. External side
effects must be enqueued, rather than performed before the transaction commits.
The key, mutation, and response commit together. Concurrent retries replay the
committed result; failure or cancellation rolls the entire transaction back.
Request hashing returns serialization errors to the caller. Records expire
after seven days; expired-key replacement is serialized by PostgreSQL.

`TenantsRepository::create_in_tx`, `operations::create_in_tx`, `audit::insert`,
and `outbox::insert_in_tx` accept the transaction's connection. The standalone
tenant `create` method uses the same implementation inside its own transaction.

Every operation/outbox claim has a fresh token. Owner, token, and unexpired
lease are required on acknowledgments and guarded writes. Reusing a worker
name does not revive an old guard. Each successful operation claim increments
its attempt count. These checks fence database updates; downstream side effects
still require idempotent adapters and recovery in later work packages.

Migration 0005 upgrades databases initialized with 0001–0004 without editing
their checksums. It enforces organization relationships and reserves tenant
names/repositories and workspace role/slots until both desired and observed
states are deleted. NULL workspace slots represent a singleton slot.

## Template release source

Template releases come from an operator-configured source, never
customer-supplied URLs (see
[docs/implementation/centralized-deployment/06-hosted-template-releases.md](../../../docs/implementation/centralized-deployment/06-hosted-template-releases.md)):

- **Local** (default): packages the checked-in `crates/coder-client/templates`
  directory. For development and tests; requires no network.
- **GitHub** (`OPENFLOWS_RELEASE_SOURCE=github`): resolves a pinned immutable
  GitHub release, verifies the manifest and every role archive digest, rejects
  unsafe archive contents, bounds download sizes, applies timeouts, and caches
  verified artifacts on disk.

All settings are centralized and typed in `config.rs`. The release is pinned to
an immutable tag plus the manifest digest; never a mutable branch or `latest`.

### Running the hosted source locally

Publish a release with a `manifest.json` asset plus one archive per role (see
the deployment doc for the layout), then run:

```sh
export OPENFLOWS_RELEASE_SOURCE=github
export OPENFLOWS_GITHUB_TEMPLATE_REPOSITORY=The-AgenticFlow/openflows-templates
export OPENFLOWS_GITHUB_TEMPLATE_TAG=2026.10.0
export OPENFLOWS_TEMPLATE_MANIFEST_SHA256=<64-char manifest digest>
export OPENFLOWS_RELEASE_CACHE_DIR=/tmp/openflows-release-cache
cargo run -p openflows-manager
```

### Testing the release source

```sh
cargo test -p openflows-manager provisioning::source
```
