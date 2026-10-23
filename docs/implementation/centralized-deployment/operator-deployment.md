# Operator deployment

The control plane now has one configuration file and three commands:

```sh
openflows-manager --config /path/to/.env.openflows setup
openflows-manager --config /path/to/.env.openflows doctor
openflows-manager --config /path/to/.env.openflows serve
```

From a source checkout, replace `openflows-manager` with
`cargo run -p openflows-manager --`. These commands belong to the Manager;
the existing `openflows doctor` diagnoses the tenant controller.

## Local single-host installation

1. Copy `.env.openflows.example` to `.env.openflows`, restrict it to mode 0600,
   and fill in your domains, database passwords, GitHub application metadata,
   and workspace image digest. Use the same file for Compose and the Manager.
   The example explicitly selects local mode. Database passwords in URLs must
   be URL-encoded if they contain reserved characters.
2. Create the persistent `OPENFLOWS_SECRET_DIR` with mode 0700. Put the following
   credentials in individual mode-0600 files inside it:

   | File | Origin |
   | --- | --- |
   | `github-client-secret` | OpenFlows GitHub OAuth/App client secret |
   | `github-app-private-key` | OpenFlows GitHub App PEM private key |
   | `github-webhook-secret` | The secret configured for the GitHub App webhook |
   | `coder-github-client-secret` | Separate GitHub OAuth application's client secret for Coder sign-in |
   | `coder-hook-secret` | Exactly 32 random bytes shared by Coder and the Manager for lifecycle-hook signing (`openssl rand 32 > .openflows/secrets/coder-hook-secret`; setup reuses it) |

   GitHub application registration and authorization remain external steps.
   Retaining GitHub login in both products requires two client registrations.
   The Coder login application's homepage and callback use `CODER_ACCESS_URL`.
   Keep OpenFlows' existing GitHub callbacks unchanged.
3. Start the supporting services:

   ```sh
   docker compose --env-file .env.openflows -f deployment.compose.yml up -d
   ```

   This Compose file enables Coder's OAuth provider and reads its GitHub secret
   from a mounted file. The optional legacy Git external-auth provider is not
   configured: Manager runtime templates obtain Git credentials through the
   OpenFlows broker. Ports bind to localhost. Supply reverse proxies/tunnels
   for the configured HTTPS origins; Coder's origin must also be reachable
   from workspace containers. `localhost` inside a container refers to that
   container. This file does not install a reverse proxy or issue certificates.

   The Compose project and volume names match the existing local stack. When
   adopting an existing stack, preserve its database passwords: changing an
   environment value does not change the password in an initialized database.

   The pinned Coder v2.37.3 uses the `oauth2` experiment and `callback_url` API
   field; the supplied Compose/setup code handles those differences. Its OAuth
   tokens do not enforce the requested scope, even though scopes are advertised.
   The link handler verifies the account and stores its renewable OAuth refresh
   credential in the configured secret provider. Protect and rotate this secret
   as a user credential; disconnecting an account must revoke it. Do not assume
   the requested `user:read_personal` scope limits tokens on this Coder version.
   Production deployment must validate OAuth scope enforcement in its selected
   Coder release.
4. Authorize setup once against an existing Coder deployment:

   ```sh
   coder login https://your-coder-host
   cargo run -p openflows-manager -- --config .env.openflows setup
   ```

   Use an administrator account able to register OAuth applications and manage
   organizations, users, templates, and workspaces. For noninteractive setup,
   provide `OPENFLOWS_CODER_BOOTSTRAP_TOKEN_FILE` pointing to a protected API
   token file. The generated Manager token has the bootstrap account's access;
   setup does not invent a restricted role or provision a separate service
   account. If you already have a suitable service-account token, store it as
   `coder-operator-token` before setup.

   On an **empty, operator-managed Coder deployment**, setup can create the
   first administrator without a manual Coder login:

   ```sh
   cargo run -p openflows-manager -- --config .env.openflows setup \
     --initialize-coder --admin-email admin@example.com
   ```

   Its random password is saved as `coder-bootstrap-password` before the
   initial API request. Reruns can authenticate with that saved password. This
   option does not reset an existing administrator or change existing accounts.
5. Run `doctor`, address failures, then run `serve` using the commands above.
   Doctor does not apply database migrations, provision workspaces, or create
   credentials. Serve applies migrations through the existing startup path.
6. Sign into OpenFlows with GitHub, connect your personal Coder account once,
   and create a new tenant. The recorded creator owns the tenant's workspaces.

No Coder OAuth application ID, OAuth secret reference, master-key reference,
or operator-token reference needs to be copied into the deployment file.
Setup owns these defaults and stores the generated OAuth ID in
`OPENFLOWS_SECRET_DIR/deployment.json`.

## Persistence and recovery

Back up the entire secret directory together with the control-plane database.
Losing or replacing the master key makes existing encrypted records unreadable.
Setup preserves an existing master key and refuses an invalid-length key.
Do not move an existing installation to a new empty secret directory. Preserve
its keys and explicit references when adopting the new configuration file.
Legacy `.env.prod` remains supported when no new file is selected.

Setup takes an exclusive local file lock. It saves the deployment identity
before external writes, reconciles its OAuth application by that identity,
and reuses saved keys and credentials on subsequent runs. It refuses changes
to the saved Coder/public origins: callback or deployment migration must be
explicit. An invalid existing API token is an error, not a reason to silently
grant a different account access. Renew it through your credential-management
process before retrying setup.

Coder reveals newly generated tokens/secrets only once. If the process dies
after Coder creates a credential but before it is saved, that credential can
remain unused in Coder. Setup can create a replacement OAuth secret; an orphan
API token with the deterministic token name may need operator cleanup before
retrying. The local lock does not coordinate setup processes on different hosts.

Explicit `--config` files must exist; malformed or missing files fail closed.
Exported variables take precedence over file values. Relative secret directories
resolve beside the selected file. Compose resolves paths relative to its file,
so use an absolute secret-directory path when deploying outside the checkout.

## What doctor verifies

Doctor reports configuration validity, required secret files and RSA key format,
database connectivity, Redis connectivity, Coder API authentication, the registered
OAuth callback, template-source configuration, and Docker availability where
applicable. Any failure produces a nonzero exit code. It does not prove that
GitHub callbacks work, that Redis grants the required ACL permissions, that a
template artifact can be downloaded/imported, or that a running workspace can
reach Coder. Complete the browser and workspace smoke test after deployment.

## Production boundary

This change consolidates deployment tooling; it does not certify the current
local stack for production. Hosted mode already rejects the shared Docker
backend because its tenant network and secret-mount isolation is insufficient.
That guard remains intact. Do not bypass it with an arbitrary backend name.

A production release still needs an implemented and validated isolated
provisioner backend, a supported deployment manifest for that backend, pinned
hosted template artifacts and workspace images, and an operational credential
rotation/backup process. The new commands are designed to reuse the Manager's
typed configuration and secret provider, rather than introducing a second
provisioning implementation.

Existing machine-owned tenants still require explicit ownership migration;
setup neither transfers nor deletes their workspaces.

References: [Coder GitHub sign-in](https://coder.com/docs/admin/users/github-auth),
[Coder OAuth application registration](https://coder.com/docs/admin/integrations/oauth2-provider),
[v2.37.3 application schema](https://github.com/coder/coder/blob/v2.37.3/codersdk/oauth2.go),
[v2.37.3 token scope implementation](https://github.com/coder/coder/blob/v2.37.3/coderd/oauth2provider/tokens.go).
