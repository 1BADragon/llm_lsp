//! Running the user's `claude` CLI headlessly, one process per task.
//!
//! Each run is isolated from the user's interactive Claude Code setup: no
//! user/project settings, hooks, plugins, MCP servers, slash commands or
//! auto-memory. It gets only the tools we list, and our own short system
//! prompt instead of Claude Code's large default one (faster and cheaper).
//! Project context such as `CLAUDE.md` goes in the prompt explicitly.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Run {
    /// The `claude` executable.
    pub program: PathBuf,
    /// Working directory: the project root, so tool paths resolve there.
    pub cwd: PathBuf,
    pub model: String,
    pub system_prompt: String,
    /// The user message, sent on stdin.
    pub prompt: String,
    /// Built-in tools to enable. They're all pre-approved, so only list
    /// read-only ones.
    pub tools: Vec<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub duration: Duration,
}

/// The parts of `--output-format json` we use. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct ResultJson {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

impl Run {
    /// Command-line arguments, excluding the program name.
    pub fn args(&self) -> Vec<String> {
        let tools = self.tools.join(",");
        let mut args: Vec<String> = [
            "--print",
            "--output-format",
            "json",
            "--no-session-persistence",
            "--model",
            &self.model,
            "--system-prompt",
            &self.system_prompt,
            "--setting-sources",
            "",
            "--settings",
            r#"{"autoMemoryEnabled":false}"#,
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--disable-slash-commands",
            // One comma-separated value, so the variadic flag can't swallow
            // what follows.
            "--tools",
            &tools,
        ]
        .map(String::from)
        .into();
        if !self.tools.is_empty() {
            args.extend(["--allowedTools".into(), tools.clone()]);
        }
        args
    }

    pub async fn execute(&self) -> Result<Output> {
        let started = Instant::now();
        let mut cmd = Command::new(&self.program);
        cmd.args(self.args())
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // If llm_lsp itself runs under Claude Code, don't let the child think
        // it's nested in that session.
        for (key, _) in std::env::vars_os() {
            let k = key.to_string_lossy();
            if k == "CLAUDECODE"
                || k == "CLAUDE_CODE_ENTRYPOINT"
                || k.starts_with("CLAUDE_CODE_SESSION")
                || k.starts_with("CLAUDE_CODE_MESSAGING")
            {
                cmd.env_remove(&key);
            }
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", self.program.display()))?;
        // The prompt goes on stdin: no argument length limit, and a prompt
        // starting with `-` can't be mistaken for a flag. Dropping stdin
        // closes it.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin.write_all(self.prompt.as_bytes()).await?;
        drop(stdin);

        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .with_context(|| format!("claude timed out after {:?}", self.timeout))??;
        let duration = started.elapsed();

        let stdout = String::from_utf8_lossy(&out.stdout);
        match parse(&stdout) {
            Ok((text, cost_usd)) => Ok(Output { text, cost_usd, duration }),
            Err(e) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                bail!("{e:#} (exit {}; stderr: {})", out.status, stderr.trim())
            }
        }
    }
}

fn parse(stdout: &str) -> Result<(String, Option<f64>)> {
    let json = stdout.trim();
    if json.is_empty() {
        bail!("claude produced no output");
    }
    let r: ResultJson = serde_json::from_str(json).context("parsing claude JSON output")?;
    if r.is_error || r.subtype != "success" {
        bail!(
            "claude run failed ({}): {}",
            r.subtype,
            r.result.as_deref().unwrap_or("no details")
        );
    }
    let text = r.result.context("claude result has no text")?;
    Ok((text, r.total_cost_usd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        Run {
            program: "claude".into(),
            cwd: "/repo".into(),
            model: "haiku".into(),
            system_prompt: "You document code.".into(),
            prompt: "--looks like a flag".into(),
            tools: vec!["Read".into(), "Grep".into()],
            timeout: Duration::from_secs(1),
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn args_isolate_the_run() {
        let r = run();
        let args = r.args();
        assert_eq!(value_after(&args, "--model"), Some("haiku"));
        assert_eq!(value_after(&args, "--system-prompt"), Some("You document code."));
        assert_eq!(value_after(&args, "--setting-sources"), Some(""));
        assert_eq!(value_after(&args, "--tools"), Some("Read,Grep"));
        assert_eq!(value_after(&args, "--allowedTools"), Some("Read,Grep"));
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(!args.contains(&r.prompt), "prompt goes on stdin");
    }

    #[test]
    fn no_tools_means_none_allowed() {
        let mut r = run();
        r.tools.clear();
        let args = r.args();
        assert_eq!(value_after(&args, "--tools"), Some(""));
        assert!(!args.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn parses_success_and_ignores_unknown_fields() {
        let out = r#"{"type":"result","subtype":"success","is_error":false,
            "result":"Adds two numbers.","total_cost_usd":0.00056,"num_turns":1,"new_field":{}}"#;
        assert_eq!(parse(out).unwrap(), ("Adds two numbers.".into(), Some(0.00056)));
    }

    #[test]
    fn reports_errors() {
        let out = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        assert!(parse(out).unwrap_err().to_string().contains("error_max_turns"));
        assert!(parse("").is_err());
        assert!(parse("not json").is_err());
    }

    /// Talks to the real CLI (costs a fraction of a cent).
    /// `cargo test real_claude -- --ignored`
    #[tokio::test]
    #[ignore = "calls the real claude CLI"]
    async fn real_claude() {
        let mut r = run();
        r.cwd = std::env::temp_dir();
        r.tools.clear();
        r.prompt = "Reply with exactly: ok".into();
        r.timeout = Duration::from_secs(60);
        let out = r.execute().await.unwrap();
        assert_eq!(out.text.trim(), "ok");
        assert!(out.cost_usd.is_some());
    }
}
