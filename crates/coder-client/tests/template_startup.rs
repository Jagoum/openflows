use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn runtime_containers_follow_coder_start_count() {
    for (role, template) in [
        (
            "nexus",
            include_str!("../templates/openflows-nexus/main.tf"),
        ),
        (
            "forge",
            include_str!("../templates/openflows-forge/main.tf"),
        ),
        (
            "sentinel",
            include_str!("../templates/openflows-sentinel/main.tf"),
        ),
        (
            "vessel",
            include_str!("../templates/openflows-vessel/main.tf"),
        ),
        ("lore", include_str!("../templates/openflows-lore/main.tf")),
    ] {
        let container = template
            .split_once("resource \"docker_container\" \"workspace\" {")
            .unwrap_or_else(|| panic!("{role} has no workspace container"))
            .1;
        assert!(
            container.contains("count = data.coder_workspace.me.start_count"),
            "{role} container must be absent when Coder stops its workspace"
        );
    }
}

#[test]
fn hosted_worker_instructions_are_locally_excluded_from_git() {
    for (role, template) in [
        (
            "forge",
            include_str!("../templates/openflows-forge/main.tf"),
        ),
        (
            "sentinel",
            include_str!("../templates/openflows-sentinel/main.tf"),
        ),
        (
            "vessel",
            include_str!("../templates/openflows-vessel/main.tf"),
        ),
        ("lore", include_str!("../templates/openflows-lore/main.tf")),
    ] {
        for pattern in [
            "rev-parse --absolute-git-dir",
            "/AGENTS.md",
            "/.agents/skills/",
            "/.agents/commands/",
            "/CODING.md",
            "/SECURITY.md",
            "/REVIEW.md",
        ] {
            assert!(
                template.contains(pattern),
                "{role} must locally exclude generated instruction path {pattern}"
            );
        }
    }
}

#[test]
fn hosted_runtime_bundles_are_copied_without_relaxing_host_permissions() {
    for (role, template) in [
        (
            "nexus",
            include_str!("../templates/openflows-nexus/main.tf"),
        ),
        (
            "forge",
            include_str!("../templates/openflows-forge/main.tf"),
        ),
        (
            "sentinel",
            include_str!("../templates/openflows-sentinel/main.tf"),
        ),
        (
            "vessel",
            include_str!("../templates/openflows-vessel/main.tf"),
        ),
    ] {
        assert!(
            template.contains(
                r#"sudo cat "$OPENFLOWS_RUNTIME_CREDENTIAL_FILE" > "$RUNTIME_CREDENTIAL_COPY""#
            ),
            "{role} must read the root-owned mount through its privileged copy step"
        );
        assert!(template.contains(r#"chmod 600 "$RUNTIME_CREDENTIAL_COPY""#));
        assert!(template.contains(r#"python3 - "$RUNTIME_CREDENTIAL_COPY""#));
        assert!(template.contains(r#"rm -f "$RUNTIME_CREDENTIAL_COPY""#));
        assert!(!template.contains(r#"test -r "$OPENFLOWS_RUNTIME_CREDENTIAL_FILE""#));
    }
}

#[test]
fn sentinel_clone_fallback_uses_the_workspace_parameter() {
    let template = include_str!("../templates/openflows-sentinel/main.tf");
    assert!(template.contains(
        r#"sudo git clone "${data.coder_parameter.repo_url.value}" /home/coder/workspace"#
    ));
    assert!(!template.contains("sudo git clone ${var.repo_url}"));
}

#[test]
fn forge_hosted_cli_uses_api_token_without_personal_identity() {
    let template = include_str!("../templates/openflows-forge/main.tf");
    assert!(template.contains(r#"--data '{"purpose":"git"}'"#));
    assert!(template.contains(r#"--data '{"purpose":"api"}'"#));
    assert!(template.matches(r#"purpose\":\"git"#).count() >= 1);
    assert!(template.matches(r#"purpose\":\"api"#).count() >= 1);
    let marker = template
        .find("# Store the API-purpose installation token in gh's protected config")
        .unwrap();
    let start = template[..marker].rfind("    if [").unwrap();
    let end = template[start..]
        .find("    # Set git author identity")
        .unwrap()
        + start;
    for host in ["github.com", "github.example.com"] {
        let body = template[start..end]
            .replace("${data.coder_parameter.hosted.value}", "true")
            .replace("$${", "${")
            .replace("/home/coder", "\"$TEST_ROOT/coder\"")
            .replace("/root", "\"$TEST_ROOT/root\"");
        let script = format!(
            "set -e\nlog() {{ :; }}\nsudo() {{ \"$@\"; }}\ngh() {{ echo 'unexpected personal gh request' >&2; return 97; }}\ncurl() {{ return 98; }}\nGIT_TOKEN=test-git-token\nAPI_TOKEN=test-api-token\nGITHUB_CLI_HOST={host}\n{body}\ntest -z \"${{GH_TOKEN+x}}\"\ntest -z \"${{GH_ENTERPRISE_TOKEN+x}}\"\ngrep -F \"$GITHUB_CLI_HOST:\" \"$TEST_ROOT/coder/.config/gh/hosts.yml\"\ngrep -F 'oauth_token: test-api-token' \"$TEST_ROOT/coder/.config/gh/hosts.yml\"\n! grep -F 'test-git-token' \"$TEST_ROOT/coder/.config/gh/hosts.yml\"\ncmp \"$TEST_ROOT/coder/.config/gh/hosts.yml\" \"$TEST_ROOT/root/.config/gh/hosts.yml\"\ntest \"$EXPECTED_LOGIN\" = 'openflows[bot]'\n"
        );
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new("bash")
            .args(["-c", &script])
            .env("TEST_ROOT", dir.path())
            .env_remove("GH_TOKEN")
            .env_remove("GH_ENTERPRISE_TOKEN")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn forge_hook_setup_runs_with_terraform_heredoc_indentation() {
    let template = include_str!("../templates/openflows-forge/main.tf");
    let startup = template
        .split_once("startup_script = <<-EOT\n")
        .unwrap()
        .1
        .split_once("\n  EOT")
        .unwrap()
        .0;
    // Terraform's indented heredoc removes the smallest common indentation
    // across the entire script, including nested shell/Python heredocs.
    let indent = startup
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap();
    let lines: Vec<_> = startup
        .lines()
        .map(|line| &line[indent.min(line.len())..])
        .collect();
    let start = lines
        .iter()
        .position(|line| line.contains("<<'PYEOF'"))
        .unwrap();
    let end = lines
        .iter()
        .position(|line| line.trim() == "PYEOF")
        .unwrap();
    let script = format!(
        "set -e\npython3 - \"$1\" \"$2\" <<'PYEOF'\n{}\nprintf 'hooks-complete'\n",
        lines[start + 1..=end].join("\n")
    );
    let dir = tempfile::tempdir().unwrap();
    let hooks = dir.path().join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
    for installed in [false, true] {
        if installed {
            std::fs::write(hooks.join("session_start.sh"), "#!/bin/sh\n").unwrap();
        }
        let mut child = Command::new("bash")
            .args(["-s", "--"])
            .arg(&hooks)
            .arg(&settings)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"hooks-complete");
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");
        assert_eq!(
            settings["hooks"].as_object().unwrap().len(),
            usize::from(installed)
        );
        if installed {
            assert_eq!(
                settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
                hooks.join("session_start.sh").to_str().unwrap()
            );
        }
    }
}
