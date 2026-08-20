use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::{Harness, ReasoningEffort};

const SHELL_READY_ATTEMPTS: usize = 100;
const SHELL_READY_RETRY_DELAY: Duration = Duration::from_millis(100);
const AGENT_PANE_BUSY_RETRY_DELAYS: [Duration; 8] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_secs(1),
    Duration::from_secs(1),
    Duration::from_secs(1),
];

#[derive(Debug, Clone)]
pub struct Herdr {
    binary: String,
}

#[derive(Debug, Clone)]
pub struct CreatedTerminal {
    pub workspace_id: Option<String>,
    pub tab_id: String,
    pub pane_id: String,
    pub checkout_path: Option<String>,
}

struct AgentLaunchOptions<'a> {
    model: Option<&'a str>,
    reasoning_effort: ReasoningEffort,
    agent_args: &'a [String],
    developer_instructions: Option<&'a str>,
}

impl Herdr {
    pub fn from_env() -> Self {
        Self {
            binary: std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        }
    }

    fn output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(&self.binary)
            .args(args)
            .output()
            .with_context(|| format!("failed to run {}", self.binary))
    }

    fn checked<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(args)?;
        if !output.status.success() {
            bail!(
                "Herdr command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub fn agent_exists(&self, name: &str) -> bool {
        self.output(["agent", "get", name])
            .is_ok_and(|output| output.status.success())
    }

    pub fn agent_tab_id(&self, name: &str) -> Result<Option<String>> {
        let output = self.output(["agent", "get", name])?;
        if !output.status.success() {
            if has_error_code(&output, "agent_not_found") {
                return Ok(None);
            }
            return Err(command_error(&output));
        }
        let value: Value =
            serde_json::from_slice(&output.stdout).context("Herdr returned invalid agent JSON")?;
        Ok(value
            .pointer("/result/agent/tab_id")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    pub fn workspace_exists(&self, workspace_id: &str) -> bool {
        self.output(["workspace", "get", workspace_id])
            .is_ok_and(|output| output.status.success())
    }

    pub fn focus_agent(&self, name: &str) -> Result<()> {
        self.checked(["agent", "focus", name])?;
        Ok(())
    }

    pub fn prompt_agent(&self, name: &str, prompt: &str) -> Result<()> {
        self.checked(["agent", "prompt", name, prompt])?;
        Ok(())
    }

    pub fn show_notification(&self, title: &str, body: &str) -> Result<()> {
        self.checked(["notification", "show", title, "--body", body])?;
        Ok(())
    }

    pub fn send_ctrl_c(&self, name: &str) -> Result<()> {
        self.checked(["agent", "send-keys", name, "ctrl+c"])?;
        Ok(())
    }

    pub fn create_lead_tab(
        &self,
        workspace_id: &str,
        root: &Path,
        env: &[(&str, String)],
    ) -> Result<CreatedTerminal> {
        let label = lead_label(root);
        let mut args = vec![
            "tab".to_string(),
            "create".into(),
            "--workspace".into(),
            workspace_id.into(),
            "--cwd".into(),
            root.display().to_string(),
            "--label".into(),
            label,
            "--focus".into(),
        ];
        for (key, value) in env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        let mut terminal = parse_created(&self.checked(args)?)?;
        terminal.workspace_id = Some(workspace_id.to_string());
        Ok(terminal)
    }

    pub fn create_agent_worktree(
        &self,
        root: &Path,
        branch: &str,
        base: &str,
        label: &str,
    ) -> Result<CreatedTerminal> {
        let args = vec![
            "worktree".to_string(),
            "create".into(),
            "--cwd".into(),
            root.display().to_string(),
            "--branch".into(),
            branch.into(),
            "--base".into(),
            base.into(),
            "--label".into(),
            label.into(),
            "--no-focus".into(),
            "--json".into(),
        ];
        parse_created(&self.checked(args)?)
    }

    pub fn create_agent_tab(
        &self,
        workspace_id: &str,
        root: &Path,
        label: &str,
    ) -> Result<CreatedTerminal> {
        let args = vec![
            "tab".to_string(),
            "create".into(),
            "--workspace".into(),
            workspace_id.into(),
            "--cwd".into(),
            root.display().to_string(),
            "--label".into(),
            label.into(),
            "--no-focus".into(),
        ];
        let mut terminal = parse_created(&self.checked(args)?)?;
        terminal.workspace_id = None;
        terminal.checkout_path = Some(root.display().to_string());
        Ok(terminal)
    }

    pub fn start_agent(
        &self,
        name: &str,
        harness: Harness,
        pane_id: &str,
        model: Option<&str>,
        reasoning_effort: ReasoningEffort,
        agent_args: &[String],
    ) -> Result<()> {
        self.start_agent_with_options(
            name,
            harness,
            pane_id,
            AgentLaunchOptions {
                model,
                reasoning_effort,
                agent_args,
                developer_instructions: None,
            },
        )
    }

    pub fn start_codex_lead(
        &self,
        name: &str,
        pane_id: &str,
        model: Option<&str>,
        reasoning_effort: ReasoningEffort,
        agent_args: &[String],
        developer_instructions: &str,
    ) -> Result<()> {
        self.start_agent_with_options(
            name,
            Harness::Codex,
            pane_id,
            AgentLaunchOptions {
                model,
                reasoning_effort,
                agent_args,
                developer_instructions: Some(developer_instructions),
            },
        )
    }

    fn start_agent_with_options(
        &self,
        name: &str,
        harness: Harness,
        pane_id: &str,
        options: AgentLaunchOptions<'_>,
    ) -> Result<()> {
        self.wait_for_available_shell(pane_id)?;
        let args = start_agent_args(
            name,
            harness,
            pane_id,
            options.model,
            options.reasoning_effort,
            options.agent_args,
            options.developer_instructions,
        )?;
        for delay in AGENT_PANE_BUSY_RETRY_DELAYS {
            let output = self.output(&args)?;
            if output.status.success() {
                return Ok(());
            }
            if !has_error_code(&output, "agent_pane_busy") {
                return Err(command_error(&output));
            }
            thread::sleep(delay);
        }
        let output = self.output(args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_error(&output))
        }
    }

    fn wait_for_available_shell(&self, pane_id: &str) -> Result<()> {
        for attempt in 0..SHELL_READY_ATTEMPTS {
            let raw = self.checked(["pane", "process-info", "--pane", pane_id])?;
            if shell_is_foreground(&raw)? {
                return Ok(());
            }
            if attempt + 1 < SHELL_READY_ATTEMPTS {
                thread::sleep(SHELL_READY_RETRY_DELAY);
            }
        }
        bail!("Herdr pane {pane_id} did not become an available shell")
    }

    pub fn remove_worktree(&self, workspace_id: &str) -> Result<()> {
        self.checked(["worktree", "remove", "--workspace", workspace_id, "--force"])?;
        Ok(())
    }

    pub fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.checked(["tab", "close", tab_id])?;
        Ok(())
    }

    pub fn tab_exists(&self, tab_id: &str) -> Result<bool> {
        let output = self.output(["tab", "get", tab_id])?;
        if output.status.success() {
            Ok(true)
        } else if has_error_code(&output, "tab_not_found") {
            Ok(false)
        } else {
            Err(command_error(&output))
        }
    }
}

fn launch_model(
    harness: Harness,
    model: Option<&str>,
    reasoning_effort: ReasoningEffort,
) -> Result<Option<String>> {
    let Some(reasoning_effort) = reasoning_effort.as_str() else {
        return Ok(model.map(str::to_string));
    };
    if harness != Harness::Opencode {
        return Ok(model.map(str::to_string));
    }
    let model = model.context(
        "OpenCode reasoning_effort requires an explicit model so Cadence can select its variant",
    )?;
    let model = model.split_once('#').map_or(model, |(model, _)| model);
    Ok(Some(format!("{model}#{reasoning_effort}")))
}

fn start_agent_args(
    name: &str,
    harness: Harness,
    pane_id: &str,
    model: Option<&str>,
    reasoning_effort: ReasoningEffort,
    agent_args: &[String],
    developer_instructions: Option<&str>,
) -> Result<Vec<String>> {
    let model = launch_model(harness, model, reasoning_effort)?;
    let mut args = vec![
        "agent".to_string(),
        "start".into(),
        name.into(),
        "--kind".into(),
        harness.as_str().into(),
        "--pane".into(),
        pane_id.into(),
        "--timeout".into(),
        "120000".into(),
    ];
    if model.is_some()
        || reasoning_effort.as_str().is_some()
        || !agent_args.is_empty()
        || (harness == Harness::Codex && developer_instructions.is_some())
    {
        args.push("--".into());
    }
    if let Some(model) = &model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(reasoning_effort) = reasoning_effort.as_str() {
        match harness {
            Harness::Claude => args.extend(["--effort".into(), reasoning_effort.into()]),
            Harness::Codex => args.extend([
                "--config".into(),
                format!("model_reasoning_effort=\"{reasoning_effort}\""),
            ]),
            Harness::Opencode => {}
        }
    }
    if harness == Harness::Codex
        && let Some(developer_instructions) = developer_instructions
    {
        let encoded = serde_json::to_string(developer_instructions)
            .context("failed to encode Codex developer instructions")?;
        args.extend([
            "--config".into(),
            format!("developer_instructions={encoded}"),
        ]);
    }
    args.extend(agent_args.iter().cloned());
    Ok(args)
}

fn lead_label(root: &Path) -> String {
    let project = root
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("project");
    format!("[Lead] {project}")
}

fn command_error(output: &Output) -> anyhow::Error {
    anyhow::anyhow!(
        "Herdr command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn has_error_code(output: &Output, expected: &str) -> bool {
    serde_json::from_slice::<Value>(&output.stderr)
        .is_ok_and(|value| value.pointer("/error/code").and_then(Value::as_str) == Some(expected))
}

fn shell_is_foreground(raw: &str) -> Result<bool> {
    let value: Value = serde_json::from_str(raw).context("Herdr returned invalid JSON")?;
    let process = value
        .pointer("/result/process_info")
        .or_else(|| value.get("process_info"))
        .context("Herdr response omitted result.process_info")?;
    let shell_pid = process.get("shell_pid").and_then(Value::as_u64);
    let foreground_process_group_id = process
        .get("foreground_process_group_id")
        .and_then(Value::as_u64);
    Ok(shell_pid.is_some() && shell_pid == foreground_process_group_id)
}

fn parse_created(raw: &str) -> Result<CreatedTerminal> {
    let value: Value = serde_json::from_str(raw).context("Herdr returned invalid JSON")?;
    if let Some(error) = value.get("error") {
        bail!("Herdr returned an error: {error}");
    }
    let result = value.get("result").unwrap_or(&value);
    let workspace_id = result
        .pointer("/workspace/workspace_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let tab_id = result
        .pointer("/tab/tab_id")
        .and_then(Value::as_str)
        .context("Herdr response omitted tab.tab_id")?
        .to_string();
    let pane_id = result
        .pointer("/root_pane/pane_id")
        .and_then(Value::as_str)
        .context("Herdr response omitted root_pane.pane_id")?
        .to_string();
    let checkout_path = result
        .pointer("/worktree/path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            result
                .pointer("/workspace/worktree/checkout_path")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    Ok(CreatedTerminal {
        workspace_id,
        tab_id,
        pane_id,
        checkout_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_created_response() {
        let parsed = parse_created(
            r#"{"id":"1","result":{"type":"worktree_created","workspace":{"workspace_id":"w1","worktree":{"checkout_path":"/tmp/w"}},"tab":{"tab_id":"t1"},"root_pane":{"pane_id":"p1"},"worktree":{"path":"/tmp/w"}}}"#,
        )
        .unwrap();
        assert_eq!(parsed.workspace_id.as_deref(), Some("w1"));
        assert_eq!(parsed.pane_id, "p1");
        assert_eq!(parsed.checkout_path.as_deref(), Some("/tmp/w"));
    }

    #[test]
    fn labels_the_lead_as_project_lead() {
        assert_eq!(
            lead_label(Path::new("/tmp/example-project")),
            "[Lead] example-project"
        );
    }

    #[test]
    fn maps_opencode_reasoning_to_model_variant() {
        assert_eq!(
            launch_model(
                Harness::Opencode,
                Some("openai/gpt-5.2#low"),
                ReasoningEffort::Xhigh,
            )
            .unwrap()
            .as_deref(),
            Some("openai/gpt-5.2#xhigh")
        );
        assert!(launch_model(Harness::Opencode, None, ReasoningEffort::High).is_err());
    }

    #[test]
    fn maps_claude_model_and_reasoning_to_native_flags() {
        let args = start_agent_args(
            "reviewer",
            Harness::Claude,
            "pane-1",
            Some("opus"),
            ReasoningEffort::High,
            &["--dangerously-skip-permissions".into()],
            None,
        )
        .unwrap();

        assert_eq!(
            args,
            [
                "agent",
                "start",
                "reviewer",
                "--kind",
                "claude",
                "--pane",
                "pane-1",
                "--timeout",
                "120000",
                "--",
                "--model",
                "opus",
                "--effort",
                "high",
                "--dangerously-skip-permissions",
            ]
        );
    }

    #[test]
    fn passes_codex_developer_instructions_as_one_escaped_config_value() {
        let instructions = "Lead at C:\\repo\\\"quoted\"\nNext line\r\n tab\t";
        let args = start_agent_args(
            "lead",
            Harness::Codex,
            "pane-1",
            Some("gpt-lead"),
            ReasoningEffort::High,
            &["--add-dir".into(), "/state/with space".into()],
            Some(instructions),
        )
        .unwrap();

        assert_eq!(
            args,
            [
                "agent",
                "start",
                "lead",
                "--kind",
                "codex",
                "--pane",
                "pane-1",
                "--timeout",
                "120000",
                "--",
                "--model",
                "gpt-lead",
                "--config",
                "model_reasoning_effort=\"high\"",
                "--config",
                "developer_instructions=\"Lead at C:\\\\repo\\\\\\\"quoted\\\"\\nNext line\\r\\n tab\\t\"",
                "--add-dir",
                "/state/with space",
            ]
        );

        let developer_config = args
            .iter()
            .find(|arg| arg.starts_with("developer_instructions="))
            .unwrap();
        let parsed: toml::Value = toml::from_str(developer_config).unwrap();
        assert_eq!(
            parsed["developer_instructions"].as_str(),
            Some(instructions)
        );
    }

    #[test]
    fn does_not_add_developer_instructions_when_not_supplied() {
        for harness in [Harness::Codex, Harness::Claude, Harness::Opencode] {
            let args = start_agent_args(
                "agent",
                harness,
                "pane-1",
                None,
                ReasoningEffort::Default,
                &[],
                None,
            )
            .unwrap();
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.starts_with("developer_instructions="))
            );
        }
    }
}
