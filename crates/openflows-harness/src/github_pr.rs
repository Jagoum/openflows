//! PR creation through the brokered API-purpose GitHub credential. This path
//! works even when the optional `gh` package is unavailable in a workspace.

use anyhow::{bail, Context, Result};
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct PullRequest {
    number: u64,
    title: String,
    html_url: String,
    state: String,
}

#[derive(Debug)]
pub struct CreatedPr {
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Serialize)]
struct NewPullRequest<'a> {
    title: &'a str,
    head: &'a str,
    base: &'a str,
    body: &'a str,
}

fn valid_repo_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_branch(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

fn validate<'a>(repo: &'a str, base: &str, head: &str, title: &str) -> Result<(&'a str, &'a str)> {
    let (owner, name) = repo.split_once('/').context("--repo must be OWNER/REPO")?;
    if !valid_repo_component(owner) || !valid_repo_component(name) {
        bail!("--repo must be OWNER/REPO with valid GitHub names");
    }
    if !valid_branch(base) || !valid_branch(head) {
        bail!("--base and --head must be valid branch names");
    }
    if title.trim().is_empty() {
        bail!("--title must not be empty");
    }
    Ok((owner, name))
}

fn api_token() -> Result<String> {
    let path = PathBuf::from(
        std::env::var("OPENFLOWS_GITHUB_API_TOKEN_FILE")
            .context("OPENFLOWS_GITHUB_API_TOKEN_FILE is not set")?,
    );
    let token = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read brokered GitHub API token at {}",
            path.display()
        )
    })?;
    let token = token.trim();
    if token.is_empty() {
        bail!("brokered GitHub API token file is empty");
    }
    Ok(token.to_owned())
}

/// Find an already-open PR first so a retry after an interrupted response does
/// not open a duplicate. A closed match is not silently resurrected.
pub async fn create_or_find(
    repo: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<CreatedPr> {
    let (owner, _name) = validate(repo, base, head, title)?;
    let token = api_token()?;
    let api_base =
        std::env::var("GITHUB_API_BASE").unwrap_or_else(|_| "https://api.github.com".to_owned());
    let api_base = api_base.trim_end_matches('/');
    if !api_base.starts_with("https://") && !api_base.starts_with("http://") {
        bail!("GITHUB_API_BASE must be an HTTP(S) URL");
    }
    let url = format!("{api_base}/repos/{repo}/pulls");
    let client = Client::builder()
        .user_agent("openflows-harness")
        .default_headers({
            let mut headers = header::HeaderMap::new();
            headers.insert(header::ACCEPT, "application/vnd.github+json".parse()?);
            headers
        })
        .build()?;
    let head_filter = format!("{owner}:{head}");
    let response = client
        .get(&url)
        .bearer_auth(&token)
        .query(&[("state", "all"), ("head", &head_filter), ("base", base)])
        .send()
        .await
        .context("GitHub PR lookup failed")?;
    if !response.status().is_success() {
        bail!("GitHub PR lookup failed with HTTP {}", response.status());
    }
    let existing: Vec<PullRequest> = response
        .json()
        .await
        .context("invalid GitHub PR lookup response")?;
    if let Some(pr) = existing.iter().find(|pr| pr.state == "open") {
        return Ok(CreatedPr {
            number: pr.number,
            title: pr.title.clone(),
            url: pr.html_url.clone(),
        });
    }
    if let Some(pr) = existing.first() {
        bail!(
            "PR #{} for {head} -> {base} is {}; reopen it or choose another branch",
            pr.number,
            pr.state
        );
    }

    let response = client
        .post(&url)
        .bearer_auth(&token)
        .json(&NewPullRequest {
            title,
            head,
            base,
            body,
        })
        .send()
        .await
        .context("GitHub PR creation failed")?;
    if response.status() != StatusCode::CREATED {
        bail!("GitHub PR creation failed with HTTP {}; check Pull requests:Write permission and branch availability", response.status());
    }
    let pr: PullRequest = response
        .json()
        .await
        .context("invalid GitHub PR creation response")?;
    Ok(CreatedPr {
        number: pr.number,
        title: pr.title,
        url: pr.html_url,
    })
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn validates_pr_arguments_before_external_effects() {
        assert!(validate("org/repo", "main", "forge/T-003", "Ticket title").is_ok());
        assert!(validate("org/repo/extra", "main", "forge/T-003", "Title").is_err());
        assert!(validate("org/repo", "main", "../escape", "Title").is_err());
        assert!(validate("org/repo", "main", "forge/T-003", " ").is_err());
    }
}
