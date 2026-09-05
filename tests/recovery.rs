#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use herdr_cadence::config::Config;
use herdr_cadence::state::project_key;
use serde_json::{Value, json};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        let root = fixture.dir.path();
        fs::create_dir(root.join("repo")).unwrap();
        fs::create_dir(root.join("state")).unwrap();
        let repo = root.join("repo");
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "Cadence Test"]);
        git(&repo, &["config", "user.email", "cadence@example.test"]);
        Config::default().save(&repo).unwrap();
        git(&repo, &["add", ".cadence.toml"]);
        git(&repo, &["commit", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        let checkout = root.join("checkout");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "cadence/test",
                checkout.to_str().unwrap(),
            ],
        );
        fs::write(checkout.join("file.txt"), "reviewed\n").unwrap();
        git(&checkout, &["add", "file.txt"]);
        git(&checkout, &["commit", "-m", "reviewed"]);
        let commit = git(&checkout, &["rev-parse", "HEAD"]);
        let agent = json!({
            "id": "agent-1", "title": "Test", "task": "Test recovery",
            "scope": ["file.txt"], "acceptance": ["Tests pass"], "harness": "codex",
            "branch": "cadence/test", "base_sha": base, "agent_name": "cadence-test",
            "status": "completed", "use_worktree": true, "workspace_id": "workspace-agent",
            "tab_id": "tab-agent", "pane_id": "pane-agent", "checkout_path": checkout,
            "report": {"status": "completed", "summary": "Reviewed", "commit_sha": commit,
                "changed_paths": ["file.txt"]}
        });
        let store = json!({"schema_version": 1, "projects": {project_key(&repo): {
            "root": repo, "active_run": "run-test", "runs": {"run-test": {
                "id": "run-test", "status": "active", "base_branch": "main",
                "base_workspace_id": "workspace-base", "lead": {"name": "cadence-lead", "harness": "codex"},
                "created_unix_ms": 1, "next_agent": 2, "agents": {"agent-1": agent}
            }}
        }}});
        fs::write(
            root.join("state/state.json"),
            serde_json::to_vec(&store).unwrap(),
        )
        .unwrap();
        fixture.herdr("exit 0\n");
        fixture
    }

    fn herdr(&self, body: &str) {
        let path = self.dir.path().join("herdr");
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-cadence"));
        command
            .arg("--project-root")
            .arg(self.dir.path().join("repo"))
            .arg("--state-dir")
            .arg(self.dir.path().join("state"))
            .env_remove("CADENCE_CONFIG_DIR")
            .env_remove("HERDR_PLUGIN_CONFIG_DIR")
            .env("HERDR_BIN_PATH", self.dir.path().join("herdr"))
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Value {
        let output = self.command(args).output().unwrap();
        success(output)
    }

    fn edit_agent(&self, edit: impl FnOnce(&mut Value)) {
        let path = self.dir.path().join("state/state.json");
        let mut store: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let key = project_key(&self.dir.path().join("repo"));
        edit(&mut store["projects"][key]["runs"]["run-test"]["agents"]["agent-1"]);
        fs::write(path, serde_json::to_vec(&store).unwrap()).unwrap();
    }
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn integration_rejects_changes_after_review() {
    for committed in [false, true] {
        let fixture = Fixture::new();
        let root = fixture.dir.path();
        let before = git(&root.join("repo"), &["rev-parse", "HEAD"]);
        fs::write(root.join("checkout/unreviewed.txt"), "preserve me\n").unwrap();
        if committed {
            git(&root.join("checkout"), &["add", "unreviewed.txt"]);
            git(&root.join("checkout"), &["commit", "-m", "unreviewed"]);
        }
        let result = fixture.run(&["agent", "integrate", "agent-1"]);
        assert_eq!(result["status"], "conflict");
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("completed report")
        );
        assert_eq!(git(&root.join("repo"), &["rev-parse", "HEAD"]), before);
        assert!(root.join("checkout/unreviewed.txt").exists());
        assert_eq!(result["workspace_id"], "workspace-agent");
    }
}

#[test]
fn cleanup_preserves_edits_made_after_integration() {
    let fixture = Fixture::new();
    fixture.edit_agent(|agent| agent["status"] = "integrated".into());
    fs::write(fixture.dir.path().join("checkout/file.txt"), "new work\n").unwrap();
    let result = fixture.run(&["startup"]);
    assert_eq!(result["cleanup_warnings"].as_array().unwrap().len(), 1);
    let agent = fixture.run(&["agent", "status", "agent-1"]);
    assert_eq!(agent["workspace_id"], "workspace-agent");
    assert_eq!(
        fs::read_to_string(fixture.dir.path().join("checkout/file.txt")).unwrap(),
        "new work\n"
    );
}

#[test]
fn exits_preserve_reviewed_and_integrating_work() {
    let fixture = Fixture::new();
    for status in [
        "completed",
        "integrating",
        "conflict",
        "integrated",
        "cancelled",
        "failed",
    ] {
        fixture.edit_agent(|agent| agent["status"] = status.into());
        let before = fixture.run(&["agent", "status", "agent-1"]);
        success(
            fixture
                .command(&["event"])
                .env("HERDR_PLUGIN_EVENT", "pane.exited")
                .env("HERDR_PLUGIN_EVENT_JSON", r#"{"pane_id":"pane-agent"}"#)
                .output()
                .unwrap(),
        );
        assert_eq!(fixture.run(&["agent", "status", "agent-1"]), before);
    }
    for status in ["starting", "working", "blocked"] {
        fixture.edit_agent(|agent| agent["status"] = status.into());
        success(
            fixture
                .command(&["event"])
                .env("HERDR_PLUGIN_EVENT", "pane.exited")
                .env("HERDR_PLUGIN_EVENT_JSON", r#"{"pane_id":"pane-agent"}"#)
                .output()
                .unwrap(),
        );
        assert_eq!(
            fixture.run(&["agent", "status", "agent-1"])["status"],
            "failed"
        );
    }
    fixture.edit_agent(|agent| agent["status"] = "completed".into());
    success(
        fixture
            .command(&["event"])
            .env("HERDR_PLUGIN_EVENT", "pane.exited")
            .env("HERDR_PLUGIN_EVENT_JSON", r#"{"pane_id":"pane-agent"}"#)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fixture.run(&["agent", "integrate", "agent-1"])["status"],
        "integrated"
    );
}
