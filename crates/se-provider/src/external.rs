//! Agent CLIs the user is already logged into -- Claude Code, Codex,
//! OpenCode, Kilo Code and Cline -- used as SE model backends.
//!
//! This is how a *subscription* (Claude Pro/Max, ChatGPT) reaches SE: only
//! through the vendor's own official CLI. SE never reads, copies or replays
//! those CLIs' login tokens.
//!
//! Each CLI runs as a text-only model. Its own tools are switched off where
//! it allows that (Claude Code), and otherwise held to its read-only mode in
//! an empty scratch folder, so the user's project is touched only through
//! SE's own permission gateway. SE's tools reach it through a text bridge:
//! they are described in the prompt, `<tool_call>` blocks in the reply become
//! ordinary tool calls, and results go back in the next prompt. The prompt goes in on stdin and the CLI is
//! started directly (never through `cmd.exe`), so prompt text can never be
//! parsed as a command line.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::{
    ChatRequest, Message, ModelInfo, ModelProvider, ProviderError, Role, Source, Status,
    StreamEvent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Claude,
    Codex,
    OpenCode,
    Kilo,
    Cline,
}

const ALL: [Kind; 5] = [
    Kind::Claude,
    Kind::Codex,
    Kind::OpenCode,
    Kind::Kilo,
    Kind::Cline,
];

/// Windows' command-line limit is 32767 chars; Cline takes its prompt as an
/// argument, so anything near that is refused rather than truncated.
const MAX_ARG_PROMPT: usize = 30_000;

pub struct ExternalCli {
    kind: Kind,
}

/// Every agent CLI SE knows how to drive, found or not.
pub fn registered() -> Vec<Box<dyn ModelProvider>> {
    ALL.iter()
        .map(|&kind| Box::new(ExternalCli { kind }) as Box<dyn ModelProvider>)
        .collect()
}

impl ExternalCli {
    /// The program to start and the arguments that precede SE's own.
    fn program(&self) -> Option<(PathBuf, Vec<String>)> {
        let npm = |rel: &str| npm_root().map(|r| r.join(rel)).filter(|p| p.exists());
        let via_node = |rel: &str| {
            let script = npm(rel)?;
            Some((on_path("node")?, vec![script.display().to_string()]))
        };
        match self.kind {
            Kind::Claude => Some((on_path("claude")?, Vec::new())),
            Kind::Codex => via_node("@openai/codex/bin/codex.js"),
            Kind::OpenCode => {
                let exe = if cfg!(windows) {
                    "opencode.exe"
                } else {
                    "opencode"
                };
                Some((npm(&format!("@opencode/cli/bin/{exe}"))?, Vec::new()))
            }
            Kind::Kilo => via_node("@kilocode/cli/bin/kilo"),
            Kind::Cline => via_node("cline/bin/cline"),
        }
    }

    /// An empty folder the CLI runs in, so whatever tools it keeps see
    /// nothing of the user's project.
    fn scratch(&self) -> std::io::Result<PathBuf> {
        let dir = std::env::temp_dir().join("se-external").join(self.id());
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Full command (args after the program's own) plus what goes on stdin.
    fn invocation(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        scratch: &Path,
    ) -> Result<(Vec<String>, Option<String>), ProviderError> {
        let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let pick = !model.is_empty() && model != "default";
        let with_instructions = || {
            if system.is_empty() {
                prompt.to_string()
            } else {
                format!("Instructions:\n{system}\n\n{prompt}")
            }
        };
        Ok(match self.kind {
            Kind::Claude => {
                let file = scratch.join("system-prompt.txt");
                std::fs::write(&file, system).map_err(|e| {
                    ProviderError::Transport(format!("cannot write the system prompt: {e}"))
                })?;
                let mut args = s(&[
                    "-p",
                    "--output-format",
                    "json",
                    "--tools",
                    "",
                    "--strict-mcp-config",
                    "--setting-sources",
                    "",
                    "--no-session-persistence",
                    "--system-prompt-file",
                ]);
                args.push(file.display().to_string());
                if pick {
                    args.extend(s(&["--model", model]));
                }
                (args, Some(prompt.to_string()))
            }
            Kind::Codex => {
                let mut args = s(&[
                    "exec",
                    "--json",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--sandbox",
                    "read-only",
                    // Its own shell otherwise wins over SE's bridged tools and
                    // fails in the read-only scratch folder (seen live, codex 0.158).
                    "--disable",
                    "shell_tool",
                    "--disable",
                    "unified_exec",
                    "--ignore-rules",
                    "-C",
                ]);
                args.push(scratch.display().to_string());
                if pick {
                    args.extend(s(&["-m", model]));
                }
                args.push("-".to_string());
                (args, Some(with_instructions()))
            }
            Kind::OpenCode | Kind::Kilo => {
                let mut args = s(&["run", "--format", "json", "--agent", "plan"]);
                if pick {
                    args.extend(s(&["-m", model]));
                }
                (args, Some(with_instructions()))
            }
            Kind::Cline => {
                let text = with_instructions();
                if text.len() > MAX_ARG_PROMPT {
                    return Err(ProviderError::BadRequest(format!(
                        "the prompt is {} chars; Cline takes it as a command-line argument, \
                         which is limited to {MAX_ARG_PROMPT}",
                        text.len()
                    )));
                }
                let mut args = s(&["--json", "--plan", "--auto-approve", "false", "-c"]);
                args.push(scratch.display().to_string());
                if pick {
                    args.extend(s(&["-m", model]));
                }
                args.push(text);
                (args, None)
            }
        })
    }
}

impl ModelProvider for ExternalCli {
    fn id(&self) -> &str {
        match self.kind {
            Kind::Claude => "claude-code",
            Kind::Codex => "codex",
            Kind::OpenCode => "opencode",
            Kind::Kilo => "kilo",
            Kind::Cline => "cline",
        }
    }

    fn display_name(&self) -> &str {
        match self.kind {
            Kind::Claude => "Claude Code (your Claude subscription)",
            Kind::Codex => "Codex (your ChatGPT plan)",
            Kind::OpenCode => "OpenCode (its own logins)",
            Kind::Kilo => "Kilo Code (its own logins)",
            Kind::Cline => "Cline (its own login)",
        }
    }

    /// No key of SE's own: the CLI holds its login. Never set.
    fn env_var(&self) -> &str {
        "SE_AGENT_CLI_HAS_NO_KEY"
    }

    fn detect(&self) -> Status {
        let found = self.program().is_some();
        let detail = match (found, self.kind) {
            (false, Kind::Claude) => "Claude Code not found on PATH",
            (false, Kind::Codex) => "Codex not found (npm i -g @openai/codex)",
            (false, Kind::OpenCode) => "OpenCode not found (npm i -g @opencode/cli)",
            (false, Kind::Kilo) => "Kilo Code not found (npm i -g @kilocode/cli)",
            (false, Kind::Cline) => "Cline not found (npm i -g cline)",
            (true, Kind::Claude) => {
                "uses Claude Code's login (`claude auth status`); its own tools are off"
            }
            (true, Kind::Codex) => {
                "uses Codex's ChatGPT login; its shell is off and it runs read-only in an empty \
                 folder, with SE's tools through the bridge; ~30k tokens of Codex's own prompt per call"
            }
            (true, Kind::OpenCode) => {
                "uses OpenCode's logins (`se models opencode`); read-only plan agent in an \
                 empty folder; ~40k tokens of OpenCode's own prompt per call"
            }
            (true, Kind::Kilo) => {
                "uses Kilo Code's logins (`se models kilo`); read-only plan agent in an empty folder"
            }
            (true, Kind::Cline) => {
                "uses Cline's login (`cline auth` if it says re-authenticate); plan mode, \
                 no auto-approve, empty folder"
            }
        };
        Status {
            configured: found,
            detail: detail.to_string(),
            source: Source::None,
        }
    }

    fn is_agent_cli(&self) -> bool {
        true
    }

    fn login_status(&self) -> Option<Result<String, String>> {
        let Some((program, mut args)) = self.program() else {
            return Some(Err(self.detect().detail));
        };
        let sub: &[&str] = match self.kind {
            Kind::Claude => &["auth", "status"],
            Kind::Codex => &["login", "status"],
            Kind::OpenCode | Kind::Kilo => &["auth", "list"],
            Kind::Cline => {
                return Some(Ok(
                    "Cline has no status command; a request will say if it needs `cline auth`"
                        .to_string(),
                ))
            }
        };
        args.extend(sub.iter().map(|a| a.to_string()));
        let mut cmd = Command::new(program);
        cmd.args(args);
        Some(login_check(self.kind, cmd, LOGIN_CHECK_TIMEOUT))
    }

    fn stream(
        &self,
        request: &ChatRequest,
        cancelled: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent) -> bool,
    ) -> Result<(), ProviderError> {
        let (program, mut args) = self
            .program()
            .ok_or_else(|| ProviderError::NotConfigured(self.detect().detail))?;
        let scratch = self
            .scratch()
            .map_err(|e| ProviderError::Transport(format!("no scratch folder: {e}")))?;
        if request.tools.is_empty() {
            let (system, prompt) = split_prompt(&request.messages);
            let (more, stdin_text) = self.invocation(&request.model, &system, &prompt, &scratch)?;
            args.extend(more);
            let mut cmd = Command::new(&program);
            cmd.args(&args).current_dir(&scratch);
            return run_cli(self.kind, self.id(), cmd, stdin_text, cancelled, on_event);
        }
        // Tool bridge: the CLI's own tools stay off; SE's tools are described in
        // the prompt and calls come back as text, so each one still goes through
        // SE's permission gateway like a native tool call.
        let (mut system, prompt) = bridge_prompt(&request.messages, &request.tools);
        if self.kind == Kind::Codex {
            system.push_str(CODEX_JSON_NOTE);
        }
        let (mut more, stdin_text) = self.invocation(&request.model, &system, &prompt, &scratch)?;
        if self.kind == Kind::Codex {
            // Codex's own prompt outweighs text instructions (seen live: it ignored
            // <tool_call> blocks); a schema on its final answer does not.
            let file = scratch.join("tool-calls.schema.json");
            std::fs::write(&file, codex_schema(&request.tools).to_string()).map_err(|e| {
                ProviderError::Transport(format!("cannot write the output schema: {e}"))
            })?;
            let at = more.len().saturating_sub(1); // before the trailing `-` (prompt on stdin)
            more.splice(
                at..at,
                ["--output-schema".to_string(), file.display().to_string()],
            );
        }
        args.extend(more);
        let mut cmd = Command::new(&program);
        cmd.args(&args).current_dir(&scratch);
        // shortcut: the reply is buffered (a tool block can arrive mid-stream), so a
        // tool-enabled turn shows its text at the end; stream prose live if it matters.
        let mut text = String::new();
        let result = run_cli(
            self.kind,
            self.id(),
            cmd,
            stdin_text,
            cancelled,
            &mut |event| match event {
                StreamEvent::TextDelta(t) => {
                    text.push_str(&t);
                    true
                }
                StreamEvent::FinishReason(_) | StreamEvent::Done => true,
                other => on_event(other),
            },
        );
        // For `se exec` debugging: stderr would scribble over the TUI's screen.
        if std::env::var_os("SE_BRIDGE_DEBUG").is_some() {
            eprintln!("\n[bridge raw reply]\n{text}\n[/bridge raw reply]");
        }
        let (prose, calls) = json_tool_calls(&text).unwrap_or_else(|| parse_tool_calls(&text));
        if result.is_err() {
            // Partial text is kept as the partial answer; half-made calls are not run.
            if !text.is_empty() {
                on_event(StreamEvent::TextDelta(text));
            }
            return result;
        }
        if !prose.is_empty() && !on_event(StreamEvent::TextDelta(prose)) {
            return Err(ProviderError::Cancelled);
        }
        let finish = if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        for (id, name, arguments) in calls {
            if !on_event(StreamEvent::ToolCallRequested {
                id,
                name,
                arguments,
            }) {
                return Err(ProviderError::Cancelled);
            }
        }
        on_event(StreamEvent::FinishReason(finish.into()));
        on_event(StreamEvent::Done);
        Ok(())
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let named = |ids: &[&str]| ids.iter().map(|id| model_info(id)).collect();
        match self.kind {
            Kind::Claude => Ok(named(&["default", "sonnet", "opus", "haiku", "fable"])),
            Kind::Codex => Ok(codex_models()),
            Kind::Cline => Ok(named(&["default"])),
            Kind::OpenCode | Kind::Kilo => {
                let (program, args) = self
                    .program()
                    .ok_or_else(|| ProviderError::NotConfigured(self.detect().detail))?;
                let run = |sub: &[&str]| {
                    Command::new(&program)
                        .args(&args)
                        .args(sub)
                        .stdin(Stdio::null())
                        .output()
                        .map_err(|e| ProviderError::Transport(e.to_string()))
                };
                let out = run(&["models"])?;
                // shortcut: a Kilo sign-in is read from `kilo auth list` by name ("● Kilo ..."),
                // never from its token file; revisit if Kilo renames its gateway login.
                let gateway_login = self.kind == Kind::Kilo
                    && run(&["auth", "list"]).is_ok_and(|o| {
                        strip_ansi(&String::from_utf8_lossy(&o.stdout))
                            .lines()
                            .any(|l| {
                                l.trim_start().starts_with('●') && l.to_lowercase().contains("kilo")
                            })
                    });
                let native = native_providers();
                Ok(String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.contains('/') && !l.contains(' '))
                    .filter(|id| offered(self.kind, id, gateway_login, &native))
                    .map(model_info)
                    .collect())
            }
        }
    }
}

/// Runs one agent-CLI request to the end, a cancel (the caller's deadline
/// included), or a refusal from `on_event`. Text already streamed stays with
/// the caller, which keeps it as the partial answer when this returns an error.
fn run_cli(
    kind: Kind,
    id: &str,
    mut cmd: Command,
    stdin_text: Option<String>,
    cancelled: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(StreamEvent) -> bool,
) -> Result<(), ProviderError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ProviderError::Transport(format!("cannot start {program}: {e}")))?;
    if let (Some(text), Some(mut stdin)) = (stdin_text, child.stdin.take()) {
        // Written on its own thread: a CLI that fills its stdout pipe
        // before reading all of stdin would otherwise deadlock.
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Readers are never joined: a grandchild (an npm shim's node) can hold a pipe open.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let (etx, erx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        let _ = etx.send(s);
    });

    let stop = |child: &mut Child| {
        kill_tree(child);
        Err(ProviderError::Cancelled)
    };
    let mut parser = Parser::new(kind);
    loop {
        if cancelled() {
            return stop(&mut child);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                for event in parser.line(&line) {
                    if !on_event(event) {
                        return stop(&mut child);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if cancelled() => return stop(&mut child),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return Err(ProviderError::Transport(format!(
                    "lost the CLI process: {e}"
                )))
            }
        }
    };
    let stderr_text = erx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let events = parser.finish();
    if !parser.answered() {
        let why = parser
            .error
            .clone()
            .or_else(|| first_line(&stderr_text))
            .unwrap_or_else(|| format!("{id} exited with {status} and no answer"));
        return Err(classify(&why));
    }
    let complete = status.success();
    for event in events {
        // A failed exit after some text: deliver the text, but not the end-of-answer markers.
        if !complete && matches!(event, StreamEvent::FinishReason(_) | StreamEvent::Done) {
            continue;
        }
        if !on_event(event) {
            return Err(ProviderError::Cancelled);
        }
    }
    if !complete {
        return Err(ProviderError::Truncated(format!(
            "[partial answer: {id} exited with {status}]"
        )));
    }
    Ok(())
}

/// How long `kill_tree` waits for the killed process to be gone.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// Kills `child` and everything it started, then waits at most `KILL_WAIT` for it to exit.
// taskkill /T on Windows rather than a job object: se-provider has no windows-sys dependency.
fn kill_tree(child: &mut Child) {
    let tree_killed = cfg!(windows)
        && Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if !tree_killed {
        let _ = child.kill();
    }
    let until = Instant::now() + KILL_WAIT;
    while Instant::now() < until {
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
}

/// How long a login-status command may take before its answer is "unknown".
const LOGIN_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs a login-status command for at most `limit`. A command that cannot
/// start or does not answer in time is "unknown" (an `Ok` that says so, like
/// Cline's): never "logged out", and never a hang.
fn login_check(kind: Kind, mut cmd: Command, limit: Duration) -> Result<String, String> {
    let unknown = |why: String| {
        Ok(format!(
            "login status unknown ({why}); a request will say if it needs a login"
        ))
    };
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return unknown(format!("could not start the check: {e}")),
    };
    let (mut out, mut err) = (child.stdout.take(), child.stderr.take());
    let (tx, rx) = mpsc::channel();
    // Never joined: after a kill, a grandchild (an npm shim's node) can hold the pipe open.
    std::thread::spawn(move || {
        let mut text = Vec::new();
        let err_thread = std::thread::spawn(move || {
            let mut e = Vec::new();
            if let Some(s) = err.as_mut() {
                let _ = s.read_to_end(&mut e);
            }
            e
        });
        if let Some(s) = out.as_mut() {
            let _ = s.read_to_end(&mut text);
        }
        text.extend(err_thread.join().unwrap_or_default());
        let _ = tx.send(text);
    });
    match rx.recv_timeout(limit) {
        Ok(bytes) => {
            let _ = child.wait();
            parse_login(kind, &strip_ansi(&String::from_utf8_lossy(&bytes)))
        }
        Err(_) => {
            kill_tree(&mut child);
            unknown(format!("no answer within {} s", limit.as_secs_f32()))
        }
    }
}

/// Reads a login-status command's output. Only whether and how the user
/// is logged in is kept: never an email, organisation or token.
fn parse_login(kind: Kind, text: &str) -> Result<String, String> {
    match kind {
        Kind::Claude => {
            let v: serde_json::Value = serde_json::from_str(text.trim()).map_err(|_| {
                "Claude Code did not report its login; run `claude auth status`".to_string()
            })?;
            if v["loggedIn"].as_bool() == Some(true) {
                let how = match v["authMethod"].as_str() {
                    Some("claude.ai") => "your Claude subscription",
                    Some(other) => other,
                    None => "Claude Code",
                };
                Ok(format!("logged in with {how}"))
            } else {
                Err("not logged in: run `claude auth login` in a terminal".to_string())
            }
        }
        Kind::Codex => {
            let lower = text.to_ascii_lowercase();
            if lower.contains("not logged in") {
                Err("not logged in: run `codex login` in a terminal".to_string())
            } else if lower.contains("logged in using chatgpt") {
                Ok("logged in with your ChatGPT plan".to_string())
            } else if lower.contains("logged in") {
                Ok(first_line(text).unwrap_or_else(|| "logged in".into()))
            } else {
                Err("not logged in: run `codex login` in a terminal".to_string())
            }
        }
        Kind::OpenCode | Kind::Kilo => {
            let tool = if kind == Kind::Kilo {
                "kilo"
            } else {
                "opencode"
            };
            // Lines naming a credential: "<name> ... stored" (OpenCode) or
            // a provider under Kilo's credential/environment sections.
            let stored = text
                .lines()
                .filter(|l| {
                    let l = l.trim();
                    l.ends_with("stored") || l.ends_with("environment") || l.starts_with('●')
                })
                .count();
            if stored > 0 {
                Ok(format!("{stored} login(s) or key(s) available in {tool}"))
            } else {
                Err(format!(
                    "no logins in {tool}: run `{tool} auth login` in a terminal"
                ))
            }
        }
        Kind::Cline => unreachable!("answered without running anything"),
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Codex's own model list for the logged-in account, from its cache
/// (`$CODEX_HOME` or `~/.codex`): the models it shows, its configured
/// default first. Metadata only -- never `auth.json`.
fn codex_models() -> Vec<ModelInfo> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|h| PathBuf::from(h).join(".codex"))
        });
    let Some(home) = home else {
        return vec![model_info("default")];
    };
    let default = std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.trim().strip_prefix("model ="))
                .map(|v| v.trim().trim_matches('"').to_string())
        });
    let mut models: Vec<ModelInfo> = std::fs::read(home.join("models_cache.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["models"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|m| m["visibility"].as_str() != Some("hide"))
        .filter_map(|m| {
            let mut info = model_info(m["slug"].as_str()?);
            info.display_name = m["display_name"].as_str().map(str::to_string);
            info.context_window = m["context_window"].as_u64();
            Some(info)
        })
        .collect();
    if let Some(d) = default {
        if let Some(i) = models.iter().position(|m| m.id == d) {
            let first = models.remove(i);
            models.insert(0, first);
        }
    }
    if models.is_empty() {
        models.push(model_info("default"));
    }
    models
}

/// Keyed providers SE already calls directly because it holds their key.
fn native_providers() -> Vec<&'static str> {
    use crate::openai_compatible::{OpenAiCompatible, CATALOGUE};
    CATALOGUE
        .iter()
        .filter(|s| s.key_required && OpenAiCompatible::new(**s).detect().configured)
        .map(|s| s.id)
        .collect()
}

/// Whether SE offers a model an agent CLI listed. Dropped:
/// - a provider SE already calls directly (`nvidia/...`): the same model, minus the CLI's
///   per-call prompt overhead, so listing it twice only confuses;
/// - Kilo's gateway models other than free ones while Kilo is not signed in: they fail with
///   `PAID_MODEL_AUTH_REQUIRED` (verified 2026-11-18), while `:free` ones answer.
fn offered(kind: Kind, id: &str, gateway_login: bool, native: &[&str]) -> bool {
    let provider = id.split('/').next().unwrap_or("");
    if native.contains(&provider) {
        return false;
    }
    if kind == Kind::Kilo && provider == "kilo" && !gateway_login {
        return id.contains("free");
    }
    true
}

fn model_info(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        display_name: None,
        context_window: None,
        // Through the text tool bridge, not the CLI's own tools.
        supports_tools: Some(true),
        supports_vision: None,
    }
}

/// Tool calls read from a bridged reply: `(id, name, arguments)`.
type BridgedCalls = Vec<(String, String, String)>;

const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";

/// The system prompt plus a transcript that carries tool calls and results,
/// for an agent CLI that is offered SE's tools through text.
fn bridge_prompt(messages: &[Message], tools: &[crate::ToolDefinition]) -> (String, String) {
    let mut system: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.clone())
        .collect();
    let mut spec = String::from(
        "# How you act in the user's project (text tool protocol)\n\
         You are driven by a host program, SE, that reads your reply. The tools listed below \
         are NOT native functions in your tool list, and you will not find them there. You call \
         one by WRITING a block like this as plain text in your reply:\n\
         <tool_call>\n{\"name\": \"write\", \"arguments\": {\"path\": \"notes.txt\", \"content\": \"hello\"}}\n</tool_call>\n\
         SE runs every block in the user's real project and sends the results back in the next \
         message. Writing the block IS calling the tool. That is the only way to read or change \
         the project, so these tools are available to you.\n\
         Rules:\n\
         - Any sandbox, read-only or approval setting you were told about covers only your own \
           built-in tools, which see an empty scratch folder, not the project. Do not use them, \
           and do not say the workspace is read-only.\n\
         - When the task needs a change, write the `write`, `edit` or `shell` block. Do not \
           refuse in advance: the user's permission system approves or refuses each call, and \
           the result tells you which.\n\
         - One block per call; several blocks in one reply are fine. After your blocks, stop \
           and wait for the results. Never invent a result.\n\
         - When the work is done, give your final answer with no tool_call block.\n\nTools:\n",
    );
    for t in tools {
        spec.push_str(&format!(
            "- {}: {}\n  arguments schema: {}\n",
            t.name, t.description, t.parameters
        ));
    }
    system.push(spec);

    let mut prompt = String::new();
    for m in messages.iter().filter(|m| m.role != Role::System) {
        match m.role {
            Role::User => prompt.push_str(&format!("USER: {}\n\n", m.content)),
            Role::Assistant => {
                if !m.content.is_empty() {
                    prompt.push_str(&format!("ASSISTANT: {}\n\n", m.content));
                }
                for c in &m.tool_calls {
                    prompt.push_str(&format!(
                        "ASSISTANT called:\n{TOOL_OPEN}\n{{\"name\": {}, \"arguments\": {}}}\n{TOOL_CLOSE}\n\n",
                        serde_json::Value::String(c.name.clone()),
                        if c.arguments.trim().is_empty() { "{}" } else { &c.arguments }
                    ));
                }
            }
            Role::Tool => prompt.push_str(&format!(
                "TOOL RESULT ({}):\n{}\n\n",
                m.tool_call_id.as_deref().unwrap_or("?"),
                m.content
            )),
            Role::System => {}
        }
    }
    prompt.push_str("Continue: call tools if you need to, otherwise give your final answer.");
    (system.join("\n\n"), prompt)
}

const CODEX_JSON_NOTE: &str = "

Your final response is JSON in a fixed shape. Put what you      want to say in `message`, and each tool call in `tool_calls` as `name` plus `arguments_json`      (the arguments object written as a JSON string). Use an empty `tool_calls` list when the work      is done. Do not write <tool_call> blocks in this shape: the list replaces them.";

/// The JSON Schema Codex's final answer must match: a message plus SE tool calls.
/// Strict-mode friendly: every property required, no free-form objects.
fn codex_schema(tools: &[crate::ToolDefinition]) -> serde_json::Value {
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": {"type": "string"},
            "tool_calls": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "enum": names},
                        "arguments_json": {"type": "string"}
                    },
                    "required": ["name", "arguments_json"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["message", "tool_calls"],
        "additionalProperties": false
    })
}

/// A reply holding the `codex_schema` object, as `(prose, calls)`. Text before
/// and after the object is kept as prose: Codex can send further messages after
/// it in one turn, and requiring the object to end the reply dropped its tool
/// call (seen live, gpt-5.6-luna). `None`: no such object.
fn json_tool_calls(text: &str) -> Option<(String, BridgedCalls)> {
    let (start, end, v) = text.match_indices('{').find_map(|(i, _)| {
        let mut values =
            serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
        let v = values.next()?.ok()?;
        v.get("tool_calls")?.as_array()?;
        Some((i, i + values.byte_offset(), v))
    })?;
    let mut blocks = String::new();
    for c in v["tool_calls"].as_array()? {
        let name = c["name"].as_str().unwrap_or("invalid_tool_call");
        let args = c["arguments_json"].as_str().unwrap_or("{}");
        blocks.push_str(&format!(
            "{TOOL_OPEN}{{\"name\": {}, \"arguments\": {}}}{TOOL_CLOSE}",
            serde_json::Value::String(name.into()),
            serde_json::Value::String(args.into())
        ));
    }
    let before = text[..start].trim();
    let message = v["message"].as_str().unwrap_or("").trim();
    let after = text[end..].trim();
    let prose = [before, message, after]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    let (_, calls) = parse_tool_calls(&blocks);
    Some((prose, calls))
}

/// Splits a bridged reply into its prose and its tool calls `(id, name, arguments)`.
/// A block that is not valid JSON becomes a call named `invalid_tool_call`, so the
/// model is told it was refused instead of the block vanishing.
fn parse_tool_calls(text: &str) -> (String, BridgedCalls) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut prose = String::new();
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(TOOL_OPEN) {
        prose.push_str(&rest[..start]);
        let after = &rest[start + TOOL_OPEN.len()..];
        let (body, next) = match after.find(TOOL_CLOSE) {
            Some(end) => (&after[..end], &after[end + TOOL_CLOSE.len()..]),
            None => (after, ""),
        };
        let body = body
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        let id = format!(
            "bridge-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
        match parsed.as_ref().and_then(|v| Some((v["name"].as_str()?, v))) {
            Some((name, v)) => {
                let arguments = match &v["arguments"] {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => "{}".to_string(),
                    a => a.to_string(),
                };
                calls.push((id, name.to_string(), arguments));
            }
            None => calls.push((id, "invalid_tool_call".to_string(), body.to_string())),
        }
        rest = next;
    }
    prose.push_str(rest);
    (prose.trim().to_string(), calls)
}

/// Splits SE's messages into the system prompt and one prompt string: the
/// request alone, or the earlier exchanges as a transcript followed by it.
fn split_prompt(messages: &[Message]) -> (String, String) {
    let system: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect();
    let turns: Vec<&Message> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::Assistant) && !m.content.is_empty())
        .collect();
    let prompt = match turns.split_last() {
        None => String::new(),
        Some((last, [])) => last.content.clone(),
        Some((last, earlier)) => {
            let mut p = String::from("Earlier in this conversation:\n\n");
            for m in earlier {
                let who = if m.role == Role::User {
                    "USER"
                } else {
                    "ASSISTANT"
                };
                p.push_str(&format!("{who}: {}\n\n", m.content));
            }
            p.push_str(&format!("Now respond to:\n\n{}", last.content));
            p
        }
    };
    (system.join("\n\n"), prompt)
}

/// Turns each CLI's output lines into SE stream events.
struct Parser {
    kind: Kind,
    /// Claude Code prints one JSON object, possibly over several lines.
    buffered: String,
    text: bool,
    error: Option<String>,
}

impl Parser {
    fn new(kind: Kind) -> Self {
        Parser {
            kind,
            buffered: String::new(),
            text: false,
            error: None,
        }
    }

    fn answered(&self) -> bool {
        self.text && self.error.is_none()
    }

    fn line(&mut self, line: &str) -> Vec<StreamEvent> {
        if self.kind == Kind::Claude {
            self.buffered.push_str(line);
            self.buffered.push('\n');
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return Vec::new(); // banners and log lines are not events
        };
        let mut out = Vec::new();
        let ty = v["type"].as_str().unwrap_or_default();
        match self.kind {
            Kind::Codex => match ty {
                "item.completed" => {
                    let item = &v["item"];
                    let text = item["text"].as_str().unwrap_or_default();
                    match item["type"].as_str() {
                        Some("agent_message") => self.push_text(text, &mut out),
                        Some("reasoning") => out.push(StreamEvent::ReasoningDelta(text.into())),
                        _ => {}
                    }
                }
                "turn.completed" => out.push(usage(
                    &v["usage"]["input_tokens"],
                    &v["usage"]["output_tokens"],
                    &v["usage"]["cached_input_tokens"],
                )),
                "error" | "turn.failed" => self.fail(&v),
                _ => {}
            },
            Kind::OpenCode | Kind::Kilo => match ty {
                "text" => {
                    let text = v["part"]["text"].as_str().unwrap_or_default();
                    self.push_text(text, &mut out);
                }
                "step_finish" => {
                    let t = &v["part"]["tokens"];
                    out.push(usage(&t["input"], &t["output"], &t["cache"]["read"]));
                }
                "error" => self.fail(&v),
                _ => {}
            },
            // shortcut: Cline's JSON shapes were not observable here (its
            // login had expired); text is read from the common fields.
            Kind::Cline => match ty {
                "error" => self.fail(&v),
                _ => {
                    let say = v["say"].as_str().unwrap_or_default();
                    if ty == "text" || say == "text" || say == "completion_result" {
                        let text = v["text"].as_str().unwrap_or_default();
                        self.push_text(text, &mut out);
                    }
                }
            },
            Kind::Claude => unreachable!("buffered above"),
        }
        out
    }

    /// Events only known once the CLI has exited (Claude Code's result).
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.kind == Kind::Claude {
            match serde_json::from_str::<serde_json::Value>(self.buffered.trim()) {
                Ok(v) if v["is_error"].as_bool() == Some(true) => {
                    self.error = Some(
                        v["result"]
                            .as_str()
                            .unwrap_or("Claude Code reported an error")
                            .to_string(),
                    );
                }
                Ok(v) => {
                    let text = v["result"].as_str().unwrap_or_default().to_string();
                    self.push_text(&text, &mut out);
                    let u = &v["usage"];
                    let input = [
                        "input_tokens",
                        "cache_creation_input_tokens",
                        "cache_read_input_tokens",
                    ]
                    .iter()
                    .filter_map(|k| u[k].as_u64())
                    .reduce(|a, b| a + b);
                    out.push(StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: u["output_tokens"].as_u64(),
                        cached_input_tokens: u["cache_read_input_tokens"].as_u64(),
                    });
                }
                Err(_) => self.error = first_line(&self.buffered),
            }
        }
        if self.answered() {
            out.push(StreamEvent::FinishReason("stop".into()));
            out.push(StreamEvent::Done);
        }
        out
    }

    fn push_text(&mut self, text: &str, out: &mut Vec<StreamEvent>) {
        if text.is_empty() {
            return;
        }
        if self.text {
            out.push(StreamEvent::TextDelta("\n\n".into()));
        }
        self.text = true;
        out.push(StreamEvent::TextDelta(text.into()));
    }

    fn fail(&mut self, v: &serde_json::Value) {
        let msg = [&v["error"]["message"], &v["message"], &v["error"]]
            .into_iter()
            .find_map(|m| m.as_str())
            .unwrap_or("the CLI reported an error");
        self.error = Some(msg.to_string());
    }
}

fn usage(
    input: &serde_json::Value,
    output: &serde_json::Value,
    cached: &serde_json::Value,
) -> StreamEvent {
    StreamEvent::Usage {
        input_tokens: input.as_u64(),
        output_tokens: output.as_u64(),
        cached_input_tokens: cached.as_u64(),
    }
}

/// An expired login is an auth problem the user must fix, not a retry.
fn classify(why: &str) -> ProviderError {
    let lower = why.to_ascii_lowercase();
    if ["auth", "log in", "login", "logged in"]
        .iter()
        .any(|k| lower.contains(k))
    {
        ProviderError::Auth(why.to_string())
    } else {
        ProviderError::Protocol(why.to_string())
    }
}

fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(400).collect())
}

/// The global npm `node_modules` folder.
// shortcut: Windows' %APPDATA% layout plus the two common Unix prefixes;
// ask `npm root -g` if a user's setup differs.
fn npm_root() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("npm").join("node_modules")),
        Some(PathBuf::from("/usr/local/lib/node_modules")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".npm-global/lib/node_modules")),
    ];
    candidates.into_iter().flatten().find(|p| p.is_dir())
}

/// A real executable on PATH (`.exe` on Windows; never a `.cmd`/`.ps1`
/// shim, which would route the arguments through a shell).
fn on_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(kind: Kind, lines: &[&str]) -> (Vec<StreamEvent>, Parser) {
        let mut p = Parser::new(kind);
        let mut out: Vec<StreamEvent> = lines.iter().flat_map(|l| p.line(l)).collect();
        out.extend(p.finish());
        (out, p)
    }

    fn text(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    // The fixtures below are trimmed from real output of each CLI.

    #[test]
    fn only_models_the_user_can_run_and_not_already_offered_directly_are_listed() {
        let native = ["nvidia"];
        // Same model SE already calls with the user's own key: listed once, not twice.
        assert!(!offered(Kind::Kilo, "nvidia/llama-x", false, &native));
        assert!(!offered(Kind::OpenCode, "nvidia/llama-x", false, &native));
        // Kilo not signed in: free gateway models only.
        assert!(offered(Kind::Kilo, "kilo/kilo-auto/free", false, &native));
        assert!(offered(
            Kind::Kilo,
            "kilo/cohere/north-mini-code:free",
            false,
            &native
        ));
        assert!(!offered(
            Kind::Kilo,
            "kilo/~anthropic/claude-sonnet-latest",
            false,
            &native
        ));
        // Signed in: the whole gateway.
        assert!(offered(
            Kind::Kilo,
            "kilo/~anthropic/claude-sonnet-latest",
            true,
            &native
        ));
        // OpenCode already filters its own gateway; its other logins pass through.
        assert!(offered(
            Kind::OpenCode,
            "claude-agent/claude-opus-5",
            false,
            &native
        ));
        assert!(offered(
            Kind::OpenCode,
            "opencode/big-pickle",
            false,
            &native
        ));
    }

    #[test]
    fn claude_code_result_and_usage_are_read() {
        let (events, p) = feed(
            Kind::Claude,
            &[
                r#"{"type":"result","subtype":"success","is_error":false,"result":"PONG","usage":{"input_tokens":2,"cache_creation_input_tokens":746,"cache_read_input_tokens":10,"output_tokens":5}}"#,
            ],
        );
        assert!(p.answered());
        assert_eq!(text(&events), "PONG");
        assert!(events.contains(&StreamEvent::Usage {
            input_tokens: Some(758),
            output_tokens: Some(5),
            cached_input_tokens: Some(10),
        }));
        assert_eq!(events.last(), Some(&StreamEvent::Done));
    }

    #[test]
    fn a_claude_code_error_is_an_error_not_an_answer() {
        let (_, p) = feed(
            Kind::Claude,
            &[r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#],
        );
        assert!(!p.answered());
        assert!(matches!(
            classify(p.error.as_deref().unwrap()),
            ProviderError::Auth(_)
        ));
    }

    #[test]
    fn codex_messages_reasoning_and_usage_are_read() {
        let (events, p) = feed(
            Kind::Codex,
            &[
                r#"{"type":"thread.started","thread_id":"x"}"#,
                r#"{"type":"item.completed","item":{"id":"i0","type":"reasoning","text":"hmm"}}"#,
                r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"PONG"}}"#,
                r#"{"type":"turn.completed","usage":{"input_tokens":32664,"cached_input_tokens":0,"output_tokens":6}}"#,
            ],
        );
        assert!(p.answered());
        assert_eq!(text(&events), "PONG");
        assert!(events.contains(&StreamEvent::ReasoningDelta("hmm".into())));
        assert!(events.contains(&StreamEvent::Usage {
            input_tokens: Some(32664),
            output_tokens: Some(6),
            cached_input_tokens: Some(0),
        }));
    }

    #[test]
    fn opencode_text_usage_and_errors_are_read() {
        let (events, p) = feed(
            Kind::OpenCode,
            &[
                "INFO some log line",
                r#"{"type":"text","part":{"type":"text","text":"PONG"}}"#,
                r#"{"type":"step_finish","part":{"tokens":{"input":39275,"output":4,"cache":{"read":1792}}}}"#,
            ],
        );
        assert!(p.answered());
        assert_eq!(text(&events), "PONG");
        assert!(events.contains(&StreamEvent::Usage {
            input_tokens: Some(39275),
            output_tokens: Some(4),
            cached_input_tokens: Some(1792),
        }));
        let (_, p) = feed(
            Kind::OpenCode,
            &[
                r#"{"type":"error","error":{"type":"provider.no-route","message":"Model unavailable: nosuch/model"}}"#,
            ],
        );
        assert_eq!(p.error.as_deref(), Some("Model unavailable: nosuch/model"));
        assert!(!p.answered());
    }

    #[test]
    fn login_status_is_read_without_keeping_personal_details() {
        let claude = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "someone@example.com", "orgName": "Org"}"#;
        let ok = parse_login(Kind::Claude, claude).unwrap();
        assert_eq!(ok, "logged in with your Claude subscription");
        assert!(!ok.contains('@'));
        assert!(parse_login(Kind::Claude, r#"{"loggedIn": false}"#).is_err());
        assert_eq!(
            parse_login(Kind::Codex, "Logged in using ChatGPT\n").unwrap(),
            "logged in with your ChatGPT plan"
        );
        assert!(parse_login(Kind::Codex, "Not logged in").is_err());
        let oc = "apper         apper                       stored\nNVIDIA NIM    NVIDIA_API_KEY              environment\n";
        assert!(parse_login(Kind::OpenCode, oc).unwrap().starts_with("2 "));
        let kilo = strip_ansi("\u{1b}[90m\u{2514}\u{1b}[39m  0 credentials\n\u{1b}[34m\u{25cf}\u{1b}[39m  Nvidia \u{1b}[90mNVIDIA_API_KEY\n");
        assert!(parse_login(Kind::Kilo, &kilo).is_ok(), "{kilo}");
        assert!(parse_login(Kind::Kilo, "0 credentials\n0 environment variables").is_err());
    }

    #[test]
    fn a_login_check_that_hangs_or_cannot_start_is_unknown_not_logged_out() {
        let sleeper = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "ping -n 6 127.0.0.1 >NUL"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("5");
            c
        };
        let started = std::time::Instant::now();
        let r = login_check(Kind::Claude, sleeper, Duration::from_millis(300));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(r.as_ref().is_ok_and(|s| s.contains("unknown")), "{r:?}");

        let missing = Command::new("se-no-such-program-for-a-login-check");
        let r = login_check(Kind::Codex, missing, Duration::from_secs(5));
        assert!(r.as_ref().is_ok_and(|s| s.contains("unknown")), "{r:?}");
    }

    /// A fake CLI: prints one OpenCode text event, then runs `then`.
    #[cfg(windows)]
    fn fake_cli(text: &str, then_windows: &str, _then_unix: &str) -> Command {
        use std::os::windows::process::CommandExt;
        let line = format!(r#"{{"type":"text","part":{{"text":"{text}"}}}}"#);
        let mut c = Command::new("cmd");
        // raw: cmd.exe does not understand Rust's backslash-escaped quotes.
        c.arg("/C").raw_arg(format!("echo {line}& {then_windows}"));
        c
    }

    #[cfg(not(windows))]
    fn fake_cli(text: &str, _then_windows: &str, then_unix: &str) -> Command {
        let line = format!(r#"{{"type":"text","part":{{"text":"{text}"}}}}"#);
        let mut c = Command::new("sh");
        c.args(["-c", &format!("printf '%s\\n' '{line}'; {then_unix}")]);
        c
    }

    fn run_fake(cmd: Command, cancelled: &dyn Fn() -> bool) -> (Result<(), ProviderError>, String) {
        let mut events = Vec::new();
        let r = run_cli(Kind::OpenCode, "fake", cmd, None, cancelled, &mut |e| {
            events.push(e);
            true
        });
        (r, text(&events))
    }

    #[test]
    fn a_failed_exit_after_some_text_is_a_partial_answer_not_success() {
        let (r, got) = run_fake(fake_cli("hello", "exit /b 1", "exit 1"), &|| false);
        assert_eq!(got, "hello", "the partial text still reaches the caller");
        match r {
            Err(ProviderError::Truncated(d)) => {
                assert!(d.starts_with("[partial answer: fake exited with"), "{d}")
            }
            other => panic!("{other:?}"),
        }
        let (r, got) = run_fake(fake_cli("hello", "exit /b 0", "exit 0"), &|| false);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(got, "hello");
    }

    #[test]
    fn a_deadline_kills_the_cli_in_bounded_time_and_keeps_the_partial_text() {
        let cmd = fake_cli("part", "ping -n 30 127.0.0.1 >nul", "sleep 30");
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_millis(800);
        let (r, got) = run_fake(cmd, &|| std::time::Instant::now() >= deadline);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(matches!(r, Err(ProviderError::Cancelled)), "{r:?}");
        assert_eq!(got, "part");
    }

    #[test]
    fn cline_reauthentication_is_an_auth_error() {
        let (_, p) = feed(
            Kind::Cline,
            &[
                r#"{"ts":"2026-11-17T12:46:20.069Z","type":"error","message":"cline requires re-authentication."}"#,
            ],
        );
        assert!(matches!(
            classify(p.error.as_deref().unwrap()),
            ProviderError::Auth(_)
        ));
    }

    #[test]
    fn a_conversation_becomes_one_prompt_with_the_system_kept_apart() {
        let (system, prompt) = split_prompt(&[
            Message::text(Role::System, "be an architect"),
            Message::text(Role::User, "plan it"),
            Message::text(Role::Assistant, "here is the plan"),
            Message::text(Role::User, "now review it"),
        ]);
        assert_eq!(system, "be an architect");
        assert!(prompt.starts_with("Earlier in this conversation:"));
        assert!(prompt.contains("USER: plan it") && prompt.contains("ASSISTANT: here is the plan"));
        assert!(prompt.ends_with("Now respond to:\n\nnow review it"));
        let (_, single) = split_prompt(&[Message::text(Role::User, "hi")]);
        assert_eq!(single, "hi");
    }

    #[test]
    fn claude_code_runs_with_every_tool_off_and_the_prompt_on_stdin() {
        let dir = std::env::temp_dir().join("se-external-test");
        std::fs::create_dir_all(&dir).unwrap();
        let cli = ExternalCli { kind: Kind::Claude };
        let (args, stdin) = cli.invocation("opus", "sys", "hello", &dir).unwrap();
        let joined = args.join(" ");
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--tools" && w[1].is_empty()));
        assert!(joined.contains("--strict-mcp-config") && joined.contains("--model opus"));
        assert!(
            !args.iter().any(|a| a.contains("hello")),
            "prompt never on the command line"
        );
        assert_eq!(stdin.as_deref(), Some("hello"));
        let codex = ExternalCli { kind: Kind::Codex };
        let (args, _) = codex.invocation("default", "", "x", &dir).unwrap();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--sandbox" && w[1] == "read-only"));
        assert!(
            !args.iter().any(|a| a == "-m"),
            "`default` lets the CLI choose"
        );
    }

    #[test]
    fn bridged_reply_splits_into_prose_and_tool_calls() {
        let text = r#"Looking first.
<tool_call>
{"name": "read", "arguments": {"path": "a.rs"}}
</tool_call><tool_call>```json
{"name": "shell", "arguments": "{\"command\":\"ls\"}"}
```</tool_call><tool_call>not json</tool_call> done"#;
        let (prose, calls) = parse_tool_calls(text);
        assert_eq!(prose, "Looking first.\n done");
        let names: Vec<&str> = calls.iter().map(|c| c.1.as_str()).collect();
        assert_eq!(names, ["read", "shell", "invalid_tool_call"]);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&calls[0].2).unwrap()["path"],
            "a.rs"
        );
        assert_eq!(calls[1].2, "{\"command\":\"ls\"}");
        assert_eq!(calls[2].2, "not json");
        assert!(calls[0].0 != calls[1].0, "ids are unique");
        // No block: the whole reply is prose and there are no calls.
        assert_eq!(parse_tool_calls(" plain ").0, "plain");
        assert!(parse_tool_calls("plain").1.is_empty());
    }

    #[test]
    fn bridge_prompt_lists_tools_and_carries_calls_and_results() {
        let tools = vec![crate::ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let msgs = vec![
            Message::text(Role::System, "ROLE"),
            Message::text(Role::User, "fix it"),
            Message::assistant_tool_calls(vec![crate::ToolCallRecord {
                id: "bridge-1".into(),
                name: "read".into(),
                arguments: "{\"path\":\"a.rs\"}".into(),
            }]),
            Message::tool_result("bridge-1", "fn main() {}"),
        ];
        let (system, prompt) = bridge_prompt(&msgs, &tools);
        assert!(system.starts_with("ROLE") && system.contains("- read: Read a file"));
        assert!(prompt.contains("USER: fix it"));
        assert!(prompt.contains("{\"name\": \"read\", \"arguments\": {\"path\":\"a.rs\"}}"));
        assert!(prompt.contains("TOOL RESULT (bridge-1):\nfn main() {}"));
        // What the model wrote back parses into the same call.
        let echoed = &prompt[prompt.find(TOOL_OPEN).unwrap()..];
        assert_eq!(parse_tool_calls(echoed).1[0].1, "read");
    }

    #[test]
    fn a_codex_json_answer_becomes_tool_calls_and_its_schema_names_the_tools() {
        let text = "Working on it.\n\n{\"message\": \"Writing now.\", \"tool_calls\": [{\"name\": \"write\", \"arguments_json\": \"{\\\"path\\\":\\\"a.txt\\\",\\\"content\\\":\\\"x\\\"}\"}]}";
        let (prose, calls) = json_tool_calls(text).unwrap();
        assert_eq!(prose, "Working on it.\n\nWriting now.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "write");
        let args: serde_json::Value = serde_json::from_str(&calls[0].2).unwrap();
        assert_eq!(args["path"], "a.txt");
        // Done: no calls. Plain text is not this shape at all.
        assert!(json_tool_calls("{\"message\": \"ok\", \"tool_calls\": []}")
            .unwrap()
            .1
            .is_empty());
        assert!(json_tool_calls("just text {not json").is_none());
        // Seen live: the object, then more text in the same turn. The call still runs.
        let trailing = "{\"message\": \"Writing.\", \"tool_calls\": [{\"name\": \"write\", \"arguments_json\": \"{}\"}]}Escalation: user - none";
        let (prose, calls) = json_tool_calls(trailing).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(prose, "Writing.\n\nEscalation: user - none");
        let tools = vec![crate::ToolDefinition {
            name: "read".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }];
        assert_eq!(
            codex_schema(&tools)["properties"]["tool_calls"]["items"]["properties"]["name"]["enum"]
                [0],
            "read"
        );
    }
}
