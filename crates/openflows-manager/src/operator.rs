//! Deployment commands. Setup owns generated state; doctor never starts workers.
use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use rand::RngCore;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use zeroize::Zeroizing;

const STATE: &str = "deployment.json";

#[derive(Serialize, Deserialize)]
struct Deployment {
    id: uuid::Uuid,
    coder_url: String,
    public_url: String,
    oauth_id: Option<String>,
}

fn setting(key: &str) -> Result<String> {
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("configure {key} in your deployment file"))
}

fn default(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        std::env::set_var(key, value);
    }
}

/// Explicit files fail closed. Legacy deployments retain their existing defaults.
pub fn load_configuration(explicit: Option<&Path>) -> Result<()> {
    let selected = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("OPENFLOWS_CONFIG_FILE").map(PathBuf::from))
        .or_else(|| {
            Path::new(".env.openflows")
                .exists()
                .then(|| PathBuf::from(".env.openflows"))
        });
    if let Some(path) = selected {
        dotenvy::from_path(&path)
            .map_err(|_| anyhow!("cannot parse deployment file {}", path.display()))?;
        let parent = path
            .canonicalize()?
            .parent()
            .context("configuration has no parent")?
            .to_path_buf();
        let root = std::env::var_os("OPENFLOWS_SECRET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| parent.join(".openflows/secrets"));
        let root = if root.is_absolute() {
            root
        } else {
            parent.join(root)
        };
        std::env::set_var("OPENFLOWS_SECRET_DIR", &root);
        default("OPENFLOWS_MODE", "hosted");
        default("OPENFLOWS_SECRET_PROVIDER", "filesystem");
        for (key, name) in [
            ("OPENFLOWS_CRYPTO_MASTER_KEY_REF", "master-key"),
            ("OPENFLOWS_CODER_HOOK_SECRET_REF", "coder-hook-secret"),
            ("OPENFLOWS_GITHUB_CLIENT_SECRET_REF", "github-client-secret"),
            ("OPENFLOWS_GITHUB_PRIVATE_KEY_REF", "github-app-private-key"),
            (
                "OPENFLOWS_GITHUB_WEBHOOK_SECRET_REF",
                "github-webhook-secret",
            ),
            ("OPENFLOWS_CODER_OPERATOR_TOKEN_REF", "coder-operator-token"),
            (
                "OPENFLOWS_CODER_OAUTH_CLIENT_SECRET_REF",
                "coder-oauth-client-secret",
            ),
        ] {
            default(key, name);
        }
        if let Some(state) = read_state(&root)? {
            state.validate()?;
            if let Some(id) = state.oauth_id {
                default("OPENFLOWS_CODER_OAUTH_CLIENT_ID", &id);
            }
        }
    } else if Path::new(".env.prod").exists() {
        dotenvy::from_path(".env.prod").map_err(|_| anyhow!("cannot parse legacy .env.prod"))?;
        eprintln!("Using legacy .env.prod; new deployments use --config .env.openflows.");
    }
    // The Docker provisioner runs Terraform on the Docker host. In local
    // development, make the Linux controller binary available to published
    // templates without requiring operators to copy an absolute path into
    // their environment. Production deployments use released workspace images
    // and never set this development-only variable.
    if std::env::var("OPENFLOWS_MODE").as_deref() == Ok("local")
        && std::env::var_os("TF_VAR_dev_binary_host_path").is_none()
    {
        if let Ok(path) = std::env::current_dir().map(|cwd| cwd.join(".dev-binaries")) {
            if path.is_dir() {
                if let Ok(path) = path.canonicalize() {
                    std::env::set_var("TF_VAR_dev_binary_host_path", path);
                }
            }
        }
    }
    Ok(())
}

impl Deployment {
    fn validate(&self) -> Result<()> {
        if self.coder_url != origin("CODER_URL")?
            || self.public_url != origin("OPENFLOWS_PUBLIC_URL")?
        {
            bail!("deployment URLs differ from saved setup state; restore the configured URLs or explicitly migrate this deployment");
        }
        if let (Some(saved), Ok(configured)) = (
            &self.oauth_id,
            std::env::var("OPENFLOWS_CODER_OAUTH_CLIENT_ID"),
        ) {
            if saved != &configured {
                bail!("configured Coder OAuth application differs from saved setup state; explicitly migrate the client and secret together");
            }
        }
        Ok(())
    }
}

fn origin(key: &str) -> Result<String> {
    let value = setting(key)?;
    let url = url::Url::parse(&value).map_err(|_| anyhow!("{key} must be an HTTP(S) origin"))?;
    let local = std::env::var("OPENFLOWS_MODE").as_deref() == Ok("local");
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || (local && loopback && url.scheme() == "http"))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("{key} must be an HTTPS origin (HTTP loopback is allowed in explicit local mode)");
    }
    Ok(value.trim_end_matches('/').into())
}

fn read_state(root: &Path) -> Result<Option<Deployment>> {
    match fs::read(root.join(STATE)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid deployment state")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("cannot read deployment state"),
    }
}

fn save(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(root).context("cannot stage deployment state")?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(root.join(name))
        .map_err(|_| anyhow!("cannot persist deployment state"))?;
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn secret_path(root: &Path, key: &str) -> Result<PathBuf> {
    let name = setting(key)?;
    let path = PathBuf::from(&name);
    if path.is_absolute() {
        if !path.starts_with(root)
            || path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            bail!("{key} must stay inside OPENFLOWS_SECRET_DIR");
        }
        return Ok(path);
    }
    if path.components().count() != 1 || name == "." || name == ".." {
        bail!("{key} must be a secret name or an absolute file path");
    }
    Ok(root.join(path))
}

fn read_secret(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let bytes = fs::read(path)
        .map_err(|_| anyhow!("cannot read required secret file {}", path.display()))?;
    if bytes.is_empty() {
        bail!("secret file {} is empty", path.display());
    }
    Ok(Zeroizing::new(bytes))
}

fn random_key() -> Zeroizing<Vec<u8>> {
    let mut bytes = Zeroizing::new(vec![0; 32]);
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

fn ensure_key(path: &Path) -> Result<()> {
    // Never replace a key: existing ciphertext depends on it.
    match fs::metadata(path) {
        Ok(_) => {
            if read_secret(path)?.len() != 32 {
                bail!("master key must contain exactly 32 bytes");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path)?;
            file.write_all(&random_key())?;
            file.sync_all()?;
        }
        Err(_) => bail!("cannot inspect master key"),
    }
    Ok(())
}

struct Coder {
    base: String,
    http: Client,
}
impl Coder {
    fn new() -> Result<Self> {
        Ok(Self {
            base: origin("CODER_URL")?,
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(20))
                .build()?,
        })
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if !token.is_empty() {
            request = request.header("Coder-Session-Token", token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request
            .send()
            .await
            .map_err(|_| anyhow!("Coder is unreachable at configured CODER_URL"))
    }
    async fn json(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let response = self.request(method, path, token, body).await?;
        if !response.status().is_success() {
            bail!("Coder {path} returned HTTP {}; check administrator permissions and OAuth provider availability", response.status());
        }
        response
            .json()
            .await
            .map_err(|_| anyhow!("Coder returned an invalid API response for {path}"))
    }
}

fn field(value: &Value, name: &str) -> Result<String> {
    value[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Coder response is missing {name}"))
}

async fn bootstrap(
    coder: &Coder,
    root: &Path,
    initialize: bool,
    email: Option<&str>,
) -> Result<Zeroizing<String>> {
    if initialize {
        let email = email.context("--initialize-coder requires --admin-email")?;
        let first = coder
            .request(Method::GET, "/api/v2/users/first", "", None)
            .await?;
        let password_path = root.join("coder-bootstrap-password");
        if first.status() == StatusCode::NOT_FOUND {
            if !password_path.exists() {
                save(
                    root,
                    "coder-bootstrap-password",
                    hex::encode(random_key()).as_bytes(),
                )?;
            }
            let password = read_secret(&password_path)?;
            let password = std::str::from_utf8(&password)?;
            coder
                .json(
                    Method::POST,
                    "/api/v2/users/first",
                    "",
                    Some(json!({
                        "email": email, "username":"openflows-admin", "password":password
                    })),
                )
                .await?;
        } else if !first.status().is_success() {
            bail!(
                "cannot inspect Coder initialization: HTTP {}",
                first.status()
            );
        }
        // On resume, only the saved bootstrap credential is used. No admin reset.
        let password = read_secret(&password_path)?;
        let login = coder
            .json(
                Method::POST,
                "/api/v2/users/login",
                "",
                Some(json!({
                    "email":email,"password":std::str::from_utf8(&password)?
                })),
            )
            .await?;
        return Ok(Zeroizing::new(field(&login, "session_token")?));
    }
    if let Ok(path) = std::env::var("OPENFLOWS_CODER_BOOTSTRAP_TOKEN_FILE") {
        return Ok(Zeroizing::new(
            String::from_utf8(read_secret(Path::new(&path))?.to_vec())?
                .trim()
                .into(),
        ));
    }
    let mut command = tokio::process::Command::new("coder");
    command
        .args(["--url", &coder.base, "login", "token"])
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(15), command.output()).await;
    if let Ok(Ok(output)) = result {
        if output.status.success() {
            let token = Zeroizing::new(String::from_utf8(output.stdout)?);
            if !token.trim().is_empty() {
                return Ok(Zeroizing::new(token.trim().into()));
            }
        }
    }
    bail!("authorize setup with 'coder login <CODER_URL>' or OPENFLOWS_CODER_BOOTSTRAP_TOKEN_FILE; for an empty managed deployment use --initialize-coder --admin-email <email>");
}

fn verify_app(app: &Value, callback: &str) -> Result<()> {
    let correct = app["redirect_uris"]
        .as_array()
        .is_some_and(|urls| urls.iter().any(|url| url == callback))
        || app["callback_url"] == callback;
    if !correct {
        bail!("existing Coder OAuth application's callback differs; explicitly update it before rerunning setup");
    }
    uuid::Uuid::parse_str(&field(app, "id")?).context("invalid Coder OAuth application ID")?;
    Ok(())
}

pub async fn setup(initialize: bool, email: Option<&str>) -> Result<()> {
    if setting("OPENFLOWS_SECRET_PROVIDER")? != "filesystem" {
        bail!("setup currently supports the durable filesystem secret provider");
    }
    let coder = Coder::new()?;
    let public_url = origin("OPENFLOWS_PUBLIC_URL")?;
    let root = PathBuf::from(setting("OPENFLOWS_SECRET_DIR")?);
    fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("setup.lock"))?;
    lock.try_lock_exclusive()
        .context("another setup process is running")?;
    let mut state = read_state(&root)?.unwrap_or(Deployment {
        id: uuid::Uuid::new_v4(),
        coder_url: coder.base.clone(),
        public_url,
        oauth_id: std::env::var("OPENFLOWS_CODER_OAUTH_CLIENT_ID").ok(),
    });
    state.validate()?;
    save(&root, STATE, &serde_json::to_vec(&state)?)?;
    ensure_key(&secret_path(&root, "OPENFLOWS_CRYPTO_MASTER_KEY_REF")?)?;
    ensure_key(&secret_path(&root, "OPENFLOWS_CODER_HOOK_SECRET_REF")?)?;

    let operator_path = secret_path(&root, "OPENFLOWS_CODER_OPERATOR_TOKEN_REF")?;
    let token = if operator_path.exists() {
        Zeroizing::new(
            String::from_utf8(read_secret(&operator_path)?.to_vec())?
                .trim()
                .into(),
        )
    } else {
        bootstrap(&coder, &root, initialize, email).await?
    };
    let me = coder
        .json(Method::GET, "/api/v2/users/me", &token, None)
        .await?;
    if !operator_path.exists() {
        let user_id = field(&me, "id")?;
        uuid::Uuid::parse_str(&user_id)?;
        let minted = coder
            .json(
                Method::POST,
                &format!("/api/v2/users/{user_id}/keys/tokens"),
                &token,
                Some(json!({"token_name":format!("openflows-manager-{}", state.id)})),
            )
            .await?;
        let key = Zeroizing::new(field(&minted, "key")?);
        save(
            operator_path
                .parent()
                .context("operator secret needs a parent directory")?,
            operator_path
                .file_name()
                .and_then(|s| s.to_str())
                .context("invalid operator secret path")?,
            key.as_bytes(),
        )?;
    }

    let callback = format!("{}/auth/coder/callback", state.public_url);
    let app = if let Some(id) = &state.oauth_id {
        uuid::Uuid::parse_str(id).context("invalid configured Coder OAuth application ID")?;
        coder
            .json(
                Method::GET,
                &format!("/api/v2/oauth2-provider/apps/{id}"),
                &token,
                None,
            )
            .await?
    } else {
        // Coder applies its resource-name validator to OAuth applications too.
        // Keep the deployment identity deterministic within its 32-character limit.
        let name = format!("of-{}", &state.id.simple().to_string()[..29]);
        let apps = coder
            .json(Method::GET, "/api/v2/oauth2-provider/apps", &token, None)
            .await?;
        let apps = apps
            .as_array()
            .context("invalid Coder OAuth application list")?;
        let matching: Vec<_> = apps.iter().filter(|app| app["name"] == name).collect();
        match matching.as_slice() {
            [app] => (*app).clone(),
            [] => coder.json(Method::POST, "/api/v2/oauth2-provider/apps", &token,
                // v2.37 requires callback_url; newer Coder accepts it alongside
                // the equivalent redirect_uris entry for compatibility.
                Some(json!({"name":name,"callback_url":callback,"redirect_uris":[callback],"scope":"user:read_personal"}))).await?,
            _ => bail!("multiple Coder OAuth applications match this deployment; resolve the duplicate before setup"),
        }
    };
    verify_app(&app, &callback)?;
    let id = field(&app, "id")?;
    state.oauth_id = Some(id.clone());
    save(&root, STATE, &serde_json::to_vec(&state)?)?;
    let oauth_path = secret_path(&root, "OPENFLOWS_CODER_OAUTH_CLIENT_SECRET_REF")?;
    if !oauth_path.exists() {
        let secret = coder
            .json(
                Method::POST,
                &format!("/api/v2/oauth2-provider/apps/{id}/secrets"),
                &token,
                None,
            )
            .await?;
        let secret = Zeroizing::new(field(&secret, "client_secret_full")?);
        save(
            oauth_path
                .parent()
                .context("OAuth secret needs a parent directory")?,
            oauth_path
                .file_name()
                .and_then(|s| s.to_str())
                .context("invalid OAuth secret path")?,
            secret.as_bytes(),
        )?;
    }
    read_secret(&oauth_path)?;
    println!("Setup complete. Internal key, Coder API token, and OAuth application are stored. Run doctor before serve.");
    Ok(())
}

/// Run every diagnostic even after failures; never migrate or provision.
pub async fn doctor() -> Result<()> {
    let mut failed = false;
    let mut report = |label: &str, result: Result<()>| match result {
        Ok(()) => println!("PASS {label}"),
        Err(error) => {
            failed = true;
            println!("FAIL {label}: {error}");
        }
    };
    report(
        "typed configuration",
        crate::config::ManagerConfig::from_env()
            .map(|_| ())
            .map_err(anyhow::Error::msg),
    );
    report("public origin", origin("OPENFLOWS_PUBLIC_URL").map(|_| ()));
    let root = PathBuf::from(setting("OPENFLOWS_SECRET_DIR")?);
    for key in [
        "OPENFLOWS_CRYPTO_MASTER_KEY_REF",
        "OPENFLOWS_CODER_HOOK_SECRET_REF",
        "OPENFLOWS_GITHUB_CLIENT_SECRET_REF",
        "OPENFLOWS_GITHUB_PRIVATE_KEY_REF",
        "OPENFLOWS_GITHUB_WEBHOOK_SECRET_REF",
        "OPENFLOWS_CODER_OPERATOR_TOKEN_REF",
        "OPENFLOWS_CODER_OAUTH_CLIENT_SECRET_REF",
    ] {
        report(
            key,
            (|| {
                let secret = read_secret(&secret_path(&root, key)?)?;
                if key == "OPENFLOWS_CRYPTO_MASTER_KEY_REF" && secret.len() != 32 {
                    bail!("expected 32 bytes");
                }
                if key == "OPENFLOWS_GITHUB_PRIVATE_KEY_REF" {
                    jsonwebtoken::EncodingKey::from_rsa_pem(&secret)
                        .map_err(|_| anyhow!("invalid RSA private key"))?;
                }
                Ok(())
            })(),
        );
    }
    report(
        "PostgreSQL",
        async {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(5))
                .connect(&setting("OPENFLOWS_DATABASE_URL")?)
                .await
                .map_err(|_| anyhow!("cannot connect to the control-plane database"))?;
            let ping = sqlx::query("SELECT 1").execute(&pool).await;
            pool.close().await;
            ping.map_err(|_| anyhow!("database query failed"))?;
            Ok(())
        }
        .await,
    );
    report(
        "Redis",
        async {
            use fred::prelude::*;
            let url = setting("OPENFLOWS_REDIS_ADMIN_URL")?;
            let config = fred::types::config::Config::from_url(&url)
                .map_err(|_| anyhow!("invalid Redis URL"))?;
            let client = fred::types::Builder::from_config(config)
                .build()
                .map_err(|_| anyhow!("invalid Redis configuration"))?;
            let result = tokio::time::timeout(Duration::from_secs(5), client.init()).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), client.quit()).await;
            result
                .map_err(|_| anyhow!("Redis connection timed out"))?
                .map_err(|_| anyhow!("Redis connection failed"))?;
            Ok(())
        }
        .await,
    );
    report(
        "Coder identity and OAuth registration",
        async {
            let coder = Coder::new()?;
            let token = read_secret(&secret_path(&root, "OPENFLOWS_CODER_OPERATOR_TOKEN_REF")?)?;
            let token = std::str::from_utf8(&token)?.trim();
            coder
                .json(Method::GET, "/api/v2/users/me", token, None)
                .await?;
            let id = setting("OPENFLOWS_CODER_OAUTH_CLIENT_ID")?;
            uuid::Uuid::parse_str(&id).context("invalid OAuth application ID")?;
            let app = coder
                .json(
                    Method::GET,
                    &format!("/api/v2/oauth2-provider/apps/{id}"),
                    token,
                    None,
                )
                .await?;
            verify_app(
                &app,
                &format!("{}/auth/coder/callback", origin("OPENFLOWS_PUBLIC_URL")?),
            )
        }
        .await,
    );
    report(
        "template source configuration",
        crate::config::ReleaseSourceConfig::from_env()
            .map(|_| ())
            .map_err(|_| anyhow!("invalid template release configuration")),
    );
    if std::env::var("OPENFLOWS_PROVISIONER_BACKEND").unwrap_or_else(|_| "docker".into())
        == "docker"
    {
        report(
            "Docker provisioner host",
            async {
                let binary =
                    std::env::var("OPENFLOWS_DOCKER_BINARY").unwrap_or_else(|_| "docker".into());
                let output = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::process::Command::new(binary)
                        .args(["info", "--format", "{{.ServerVersion}}"])
                        .kill_on_drop(true)
                        .output(),
                )
                .await
                .map_err(|_| anyhow!("Docker daemon timed out"))?
                .map_err(|_| anyhow!("Docker executable is unavailable"))?;
                if !output.status.success() {
                    bail!("Docker daemon is unavailable");
                }
                Ok(())
            }
            .await,
        );
    }
    if failed {
        bail!("deployment checks failed; no resources were provisioned");
    }
    println!("Checks passed. Browser authorization, workspace-to-Coder connectivity, and provisioning still require an end-to-end test.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_preserves_master_key_and_rejects_invalid_existing_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master");
        ensure_key(&path).unwrap();
        let original = fs::read(&path).unwrap();
        ensure_key(&path).unwrap();
        assert_eq!(original, fs::read(&path).unwrap());
        fs::write(&path, b"invalid").unwrap();
        assert!(ensure_key(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"invalid");
    }
    #[test]
    fn oauth_reuse_requires_exact_callback_and_valid_id() {
        let app = json!({"id":uuid::Uuid::new_v4(),"redirect_uris":["https://flows.test/auth/coder/callback"]});
        assert!(verify_app(&app, "https://flows.test/auth/coder/callback").is_ok());
        assert!(verify_app(&app, "https://other.test/auth/coder/callback").is_err());
    }
}
