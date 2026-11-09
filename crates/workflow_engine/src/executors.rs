//! §4 of `DOCS/workflow-schema/execution-engine-design.md` — one async
//! function per `type_id`, dispatched by `execute_leaf`'s `match`. This is
//! the `execute` closure `scheduler::run_level` expects, for every action
//! type except containers (§5, scheduled separately by their own executor
//! recursing back into `run_level`).
//!
//! I/O-bound work (`http`, `WaitForPort`, `delay`, `script`) routes through
//! `backend::on_tokio`, same bridge `github/cli.rs` already uses — GPUI has
//! no ambient tokio reactor.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;
use tokio::sync::broadcast;

use crate::async_rt::on_tokio;

use super::engine::{RunContext, VariableType};
use super::process_tracker;
use super::run_control::{RunControl, STOPPED_MESSAGE};
use super::schema::{Action, ScriptRuntime, ScriptSource};
use super::status::{OutputStream, StatusSink};

/// Leaf-action dispatch. `Err` means the action resolved `Failed` (the
/// scheduler, not this function, decides what that means for `runAfter`
/// gating downstream — see `scheduler::run_level`); `Ok` means `Succeeded`
/// with the returned map as `outputs`. `solution_root` is only consulted by
/// `script`'s `file` mode (`DOCS/forge-workflow-engine-design.md` §4: path
/// resolution is always relative to the solution root, never the running
/// process's own cwd — a direct lesson from ForgeFlow's `uri_to_path` bug).
/// `action_id` is only consulted by `StartProcess`, to tag the tracked
/// process with which action started it (and, with the run's process owner,
/// to find what an earlier run of the flow left behind).
pub async fn execute_leaf(
    action_id: &str,
    action: &Action,
    ctx: &RunContext,
    solution_root: &Path,
) -> Result<Map<String, Value>, String> {
    match action.type_id.as_str() {
        "http" => execute_http(action, ctx).await,
        "transform" => execute_transform(action, ctx).await,
        "script" => execute_script(action, ctx, solution_root).await,
        "delay" => execute_delay(action).await,
        "notify" => execute_notify(action, ctx).await,
        "log" => execute_log(action, ctx).await,
        "WaitForPort" => execute_wait_for_port(action).await,
        "SetEnv" => execute_set_env(action),
        "StartProcess" => execute_start_process(action_id, ctx, action, solution_root).await,
        "InitVariable" => execute_init_variable(action, ctx),
        "SetVariable" => execute_set_variable(action, ctx),
        // FUTURE FEATURE — `sendEmail`, `DbMigration` and `AiCheck` are
        // parked; see the notes in `registry::REGISTRY` for what each needs.
        // They fall through to "unknown action type" below until restored.
        //
        // "sendEmail" => execute_send_email(action, ctx).await,
        // "DbMigration" | "AiCheck" => Err(format!(
        //     "{} is not implemented — stub in the original Tauri app too",
        //     action.type_id
        // )),
        other => Err(format!("unknown action type: {other}")),
    }
}

fn require_str<'a>(action: &'a Action, field: &str) -> Result<&'a str, String> {
    action
        .inputs
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "{}: missing or non-string \"{field}\" input",
                action.type_id
            )
        })
}

/// Like `require_str`, but for a `FieldKind::String` field a flow author
/// may reasonably want to interpolate from another action's `outputs` —
/// `log`/`notify`/`sendEmail`'s message-ish fields, not typed
/// `FieldKind::Expression` in the registry (they're ordinary display
/// text most of the time), but a bare string is too limiting for "log
/// what just failed". If the raw JSON value is already a string, use it
/// literally (the common case, and the only case that costs nothing extra
/// to evaluate); otherwise treat it as a JSONLogic expression, evaluate it
/// against `ctx`, and stringify the result (a string result is used as-is,
/// anything else — including `null` for a `{"var": ...}` that resolved to
/// nothing — is JSON-serialized so it's still visible rather than silently
/// blank).
fn resolve_string_field(action: &Action, field: &str, ctx: &RunContext) -> Result<String, String> {
    let value = action
        .inputs
        .get(field)
        .ok_or_else(|| format!("{}: missing \"{field}\" input", action.type_id))?;
    if let Some(s) = value.as_str() {
        return Ok(s.to_string());
    }
    let evaluated = ctx.evaluate(value)?;
    Ok(match evaluated {
        Value::String(s) => s,
        other => serde_json::to_string(&other).unwrap_or_default(),
    })
}

fn require_f64(action: &Action, field: &str) -> Result<f64, String> {
    action
        .inputs
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            format!(
                "{}: missing or non-number \"{field}\" input",
                action.type_id
            )
        })
}

/// The `name` input of a variable action, which is also the key the variable
/// is read under (`variables.<name>`), so it can't be blank or hold a `.`.
fn variable_name(action: &Action) -> Result<String, String> {
    let name = require_str(action, "name")?.trim().to_string();
    if name.is_empty() {
        return Err(format!("{}: \"name\" must not be blank", action.type_id));
    }
    if name.contains('.') {
        return Err(format!(
            "{}: \"name\" can't contain a \".\" — \"{name}\" would be read as a path",
            action.type_id
        ));
    }
    Ok(name)
}

fn variable_outputs(name: String, value: Value) -> Map<String, Value> {
    let mut outputs = Map::new();
    outputs.insert("name".to_string(), Value::String(name));
    outputs.insert("value".to_string(), value);
    outputs
}

/// Creates a flow variable: a name, a type, and a starting value, readable
/// by every later expression as `variables.<name>`. Without a `value` it
/// starts at the type's own empty value.
fn execute_init_variable(action: &Action, ctx: &RunContext) -> Result<Map<String, Value>, String> {
    let name = variable_name(action)?;
    let type_name = require_str(action, "type")?;
    let kind = VariableType::parse(type_name).ok_or_else(|| {
        format!(
            "InitVariable: unknown type \"{type_name}\" — use one of {}",
            VariableType::NAMES.join(", ")
        )
    })?;
    let value = action
        .inputs
        .get("value")
        .cloned()
        .unwrap_or_else(|| kind.default_value());
    let value = ctx
        .init_variable(&name, kind, value)
        .map_err(|e| format!("InitVariable: {e}"))?;
    Ok(variable_outputs(name, value))
}

/// Replaces an initialized variable's value: with `value` as written, or
/// with the result of `valueExpression`. They are separate inputs for the
/// same reason `http` has `body` and `bodyExpression`.
fn execute_set_variable(action: &Action, ctx: &RunContext) -> Result<Map<String, Value>, String> {
    let name = variable_name(action)?;
    let value = match (
        action.inputs.get("value"),
        action.inputs.get("valueExpression"),
    ) {
        (Some(_), Some(_)) => {
            return Err(
                "SetVariable: set either \"value\" or \"valueExpression\", not both".to_string(),
            );
        }
        (Some(value), None) => value.clone(),
        (None, Some(expression)) => ctx
            .evaluate(expression)
            .map_err(|e| format!("SetVariable: \"valueExpression\" failed: {e}"))?,
        (None, None) => {
            return Err("SetVariable: needs a \"value\" or a \"valueExpression\"".to_string());
        }
    };
    let value = ctx
        .set_variable(&name, value)
        .map_err(|e| format!("SetVariable: {e}"))?;
    Ok(variable_outputs(name, value))
}

/// The request body for an `http` action: `body` sent as written, or the
/// result of evaluating `bodyExpression`. They are separate inputs because
/// the JSONLogic engine reads every JSON object as an operator call (see
/// `RunContext::evaluate`), so one field couldn't hold both a literal object
/// and an expression.
fn http_body(action: &Action, ctx: &RunContext) -> Result<Option<Value>, String> {
    match (
        action.inputs.get("body"),
        action.inputs.get("bodyExpression"),
    ) {
        (Some(_), Some(_)) => {
            Err("http: set either \"body\" or \"bodyExpression\", not both".to_string())
        }
        (Some(body), None) => Ok(Some(body.clone())),
        (None, Some(expression)) => ctx
            .evaluate(expression)
            .map(Some)
            .map_err(|e| format!("http: \"bodyExpression\" failed: {e}")),
        (None, None) => Ok(None),
    }
}

/// The request headers for an `http` action, from its `headers` object. A
/// string value is sent as written and an object value is evaluated as
/// JSONLogic, so a token from an earlier action can be passed along.
fn http_headers(action: &Action, ctx: &RunContext) -> Result<Vec<(String, String)>, String> {
    let Some(headers) = action.inputs.get("headers") else {
        return Ok(Vec::new());
    };
    let headers = headers.as_object().ok_or_else(|| {
        "http: \"headers\" must be a JSON object of header names to values".to_string()
    })?;
    headers
        .iter()
        .map(|(name, value)| {
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Object(_) => match ctx
                    .evaluate(value)
                    .map_err(|e| format!("http: header \"{name}\" failed: {e}"))?
                {
                    Value::String(text) => text,
                    other => other.to_string(),
                },
                other => other.to_string(),
            };
            Ok((name.clone(), value))
        })
        .collect()
}

/// How much of a failed response's body goes into the error message.
const HTTP_ERROR_BODY_CHARS: usize = 500;

/// Sends the request and resolves `Succeeded` with `status` and `body` for a
/// 2xx response. Any other status resolves `Failed`, the same as `notify`:
/// a flow that wants to carry on handles it with a `["Failed"]` `runAfter`
/// or a `Try`.
async fn execute_http(action: &Action, ctx: &RunContext) -> Result<Map<String, Value>, String> {
    // Method tokens are case-sensitive on the wire, so a hand-written `get`
    // would otherwise be sent as a literal lowercase method.
    let method = require_str(action, "method")?.trim().to_ascii_uppercase();
    let url = resolve_string_field(action, "url", ctx)?;
    let body = http_body(action, ctx)?;
    let headers = http_headers(action, ctx)?;

    on_tokio(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("http: failed to build client: {e}"))?;
        let parsed_method: reqwest::Method = method
            .parse()
            .map_err(|_| format!("http: invalid method \"{method}\""))?;
        let mut req = client.request(parsed_method, &url);
        let has_content_type = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
        for (name, value) in &headers {
            req = req.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            // A string body is text, not JSON: sending it JSON-encoded would
            // wrap it in quotes.
            let (body_text, content_type) = match body {
                Value::String(text) => (text, "text/plain; charset=utf-8"),
                other => (
                    serde_json::to_string(&other)
                        .map_err(|e| format!("http: failed to serialize body: {e}"))?,
                    "application/json",
                ),
            };
            if !has_content_type {
                req = req.header("Content-Type", content_type);
            }
            req = req.body(body_text);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("http: network error: {e}"))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| format!("http: failed to read response body: {e}"))?;
        if !status.is_success() {
            let shown: String = text.trim().chars().take(HTTP_ERROR_BODY_CHARS).collect();
            return Err(format!(
                "http: {method} {url} returned {} — {shown}",
                status.as_u16()
            ));
        }
        let body_value = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));

        let mut outputs = Map::new();
        outputs.insert("status".to_string(), Value::from(status.as_u16()));
        outputs.insert("body".to_string(), body_value);
        Ok(outputs)
    })
    .await
}

/// §4's "most important to resolve" decision (2026-09-10, now Locked in
/// the design doc): evaluate `expression` (a real `JSONLogicExpr` input,
/// replacing the old free-text `strategy: String` the registry used to
/// have) against the current `RunContext`, store the result as
/// `outputs.result`.
async fn execute_transform(
    action: &Action,
    ctx: &RunContext,
) -> Result<Map<String, Value>, String> {
    let expr = action
        .inputs
        .get("expression")
        .ok_or_else(|| "transform: missing \"expression\" input".to_string())?;
    let result = ctx.evaluate(expr)?;
    let mut outputs = Map::new();
    outputs.insert("result".to_string(), result);
    Ok(outputs)
}

/// What a `script` action pipes to its process's stdin: `args` as written,
/// or the result of evaluating `argsExpression`, which is how a script is
/// given a variable, the loop's `$item`, or an earlier action's result. Two
/// inputs for the same reason `http` has `body` and `bodyExpression`.
fn script_stdin(action: &Action, ctx: &RunContext) -> Result<Option<Value>, String> {
    match (
        action.inputs.get("args"),
        action.inputs.get("argsExpression"),
    ) {
        (Some(_), Some(_)) => {
            Err("script: set either \"args\" or \"argsExpression\", not both".to_string())
        }
        (Some(args), None) => Ok(Some(args.clone())),
        (None, Some(expression)) => ctx
            .evaluate(expression)
            .map(Some)
            .map_err(|e| format!("script: \"argsExpression\" failed: {e}")),
        (None, None) => Ok(None),
    }
}

/// `DOCS/forge-workflow-engine-design.md` §4's already-locked contract:
/// `inputs.args` or the result of `inputs.argsExpression` (if present) is
/// serialized JSON piped to the process's
/// **stdin**; the script's **last line of stdout** must be a JSON object,
/// which becomes `outputs`; a non-zero exit is `Failed`. Spawns an
/// independent process per node — deliberately **not** routed through
/// `ScriptRunner` (`script_runner_panel.rs`), which is single-concurrency
/// by design and would silently serialize the exact thing this schema
/// exists to let run in parallel.
async fn execute_script(
    action: &Action,
    ctx: &RunContext,
    solution_root: &Path,
) -> Result<Map<String, Value>, String> {
    let source_value = action
        .inputs
        .get("source")
        .ok_or_else(|| "script: missing \"source\" input".to_string())?;
    let source: ScriptSource = serde_json::from_value(source_value.clone())
        .map_err(|e| format!("script: invalid \"source\": {e}"))?;
    let stdin_args = script_stdin(action, ctx)?;
    let context = ctx.script_context(stdin_args.as_ref());
    let solution_root = solution_root.to_path_buf();

    on_tokio(async move {
        let (program, args, _temp_guard) = resolve_script_invocation(&source, &solution_root)?;
        // One context file per launch, named to this process alone. A shared
        // file would be rewritten by the next iteration of a loop while this
        // one's script was still reading it. Dropped, and so deleted, once
        // the process has exited.
        let context_file = write_script_context(&context)?;
        run_script_process(
            &program,
            &args,
            stdin_args,
            Some(&*context_file),
            working_dir(&solution_root),
        )
        .await
    })
    .await
}

/// The directory a script or started process runs in when the flow doesn't
/// name one: the project root, so a relative path in a script means the
/// same thing however the app itself was launched. `None`, which leaves the
/// app's own working directory in place, when there is no such directory
/// (a flow opened from outside any project).
fn working_dir(solution_root: &Path) -> Option<&Path> {
    solution_root.is_dir().then_some(solution_root)
}

/// Environment variable holding the path of the launched script's context
/// file: the JSON from [`RunContext::script_context`]. It carries a path and
/// not the data because a single environment variable is capped (about 32,000
/// characters on Windows), which a list of any size would overrun.
pub const SCRIPT_CONTEXT_ENV: &str = "FORGE_CONTEXT";

fn write_script_context(context: &Value) -> Result<tempfile::TempPath, String> {
    let mut file = tempfile::Builder::new()
        .prefix("forge-context-")
        .suffix(".json")
        .tempfile()
        .map_err(|e| format!("script: failed to create the context file: {e}"))?;
    let json = serde_json::to_vec(context)
        .map_err(|e| format!("script: failed to serialize the run context: {e}"))?;
    std::io::Write::write_all(&mut file, &json)
        .map_err(|e| format!("script: failed to write the context file: {e}"))?;
    Ok(file.into_temp_path())
}

/// Resolves what to actually spawn. For `Inline` mode, spills `code` to a
/// real temp file first (§4: "needed for a real interpreter invocation
/// either way") and returns it as the third element so the caller can keep
/// it alive (and get it cleaned up on drop) until the process has finished
/// reading it.
fn resolve_script_invocation(
    source: &ScriptSource,
    solution_root: &Path,
) -> Result<(String, Vec<String>, Option<tempfile::TempPath>), String> {
    let (runtime, path, temp_guard) = match source {
        ScriptSource::Inline { runtime, code } => {
            // §4: ".NET deliberately excluded at the JSON Schema level (no
            // equivalent to executing a loose code string)" — file mode's
            // .csproj is the natural fit for .NET instead.
            if *runtime == ScriptRuntime::Dotnet {
                return Err(
                    "script: inline mode doesn't support the \"dotnet\" runtime — use file mode with a .csproj"
                        .to_string(),
                );
            }
            let mut file = tempfile::Builder::new()
                .prefix("forge-workflow-")
                .suffix(&format!(".{}", runtime.extension()))
                .tempfile()
                .map_err(|e| format!("script: failed to create a temp file: {e}"))?;
            std::io::Write::write_all(&mut file, code.as_bytes())
                .map_err(|e| format!("script: failed to write the temp file: {e}"))?;
            let temp_path = file.into_temp_path();
            (*runtime, temp_path.to_path_buf(), Some(temp_path))
        }
        ScriptSource::File { path, runtime } => {
            // §4: "always relative to the solution root, explicitly —
            // never relative to the running process's own working
            // directory" — the direct lesson from ForgeFlow's
            // `uri_to_path` bug.
            let resolved = solution_root.join(path);
            let runtime = match runtime {
                Some(r) => *r,
                None => ScriptRuntime::from_path(&resolved).ok_or_else(|| {
                    format!("script: couldn't infer a runtime from \"{path}\" — give an explicit \"runtime\" override")
                })?,
            };
            (runtime, resolved, None)
        }
    };
    let (program, args) = build_command_args(runtime, &path)?;
    Ok((program, args, temp_guard))
}

/// Error text for a missing `bash`, split per-platform so each names the
/// install the user actually needs: on Windows bash is almost never on `PATH`
/// (it ships inside Git for Windows), everywhere else it's a package.
#[cfg(windows)]
const BASH_NOT_FOUND: &str =
    "script: bash not found — install Git for Windows (Git Bash) or add bash to PATH";
#[cfg(not(windows))]
const BASH_NOT_FOUND: &str = "script: bash not found — install bash or add it to PATH";

/// Locate the `bash` to spawn.
///
/// Windows first asks Zed's own Git Bash locator, which searches
/// `GIT_INSTALL_ROOT` and then the install root above `git` on `PATH` — the
/// same resolution the terminal uses, so a workflow script and a terminal tab
/// agree on what "bash" means. That misses the other common Windows installs
/// (scoop, a manual MSYS2, WSL-forwarded shims), so a plain `PATH` lookup is
/// the fallback before giving up.
fn resolve_bash() -> Result<String, String> {
    #[cfg(windows)]
    if let Some(bash) = util::shell::get_windows_bash() {
        return Ok(bash);
    }

    which::which("bash")
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| BASH_NOT_FOUND.to_string())
}

fn build_command_args(
    runtime: ScriptRuntime,
    path: &Path,
) -> Result<(String, Vec<String>), String> {
    let path = path.to_string_lossy().to_string();
    let program = match runtime {
        ScriptRuntime::Python => "python".to_string(),
        ScriptRuntime::Node => "node".to_string(),
        ScriptRuntime::Powershell => "pwsh".to_string(),
        ScriptRuntime::Dotnet => "dotnet".to_string(),
        // Resolved rather than spawned as a bare `bash`, so a missing
        // interpreter is reported as "not found" with install guidance
        // instead of the raw OS spawn error.
        ScriptRuntime::Bash => resolve_bash()?,
    };
    let args = match runtime {
        ScriptRuntime::Python | ScriptRuntime::Node | ScriptRuntime::Bash => vec![path],
        ScriptRuntime::Powershell => vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-File".to_string(),
            path,
        ],
        ScriptRuntime::Dotnet => vec!["run".to_string(), "--project".to_string(), path],
    };
    Ok((program, args))
}

/// Spawns `program args...`, writes `stdin_json` (if any) to its stdin then
/// closes it (so the script sees EOF even when there's nothing to write),
/// and parses the last non-empty stdout line as the JSON-object `outputs`.
async fn run_script_process(
    program: &str,
    args: &[String],
    stdin_json: Option<Value>,
    context_file: Option<&Path>,
    working_dir: Option<&Path>,
) -> Result<Map<String, Value>, String> {
    let mut cmd = TokioCommand::new(program);
    if let Some(context_file) = context_file {
        cmd.env(SCRIPT_CONTEXT_ENV, context_file);
    }
    if let Some(working_dir) = working_dir {
        cmd.current_dir(working_dir);
    }
    // The IDE's own runtimes (the built-in Node.js, …) go first on `PATH`,
    // so `node` is the version the project should use, installed or not on
    // the machine itself.
    if let Some(path) = ide_env::path_env(working_dir) {
        cmd.env("PATH", path);
    }
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash.

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("script: failed to start \"{program}\": {e}"))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    if let Some(value) = stdin_json {
        let payload = serde_json::to_vec(&value)
            .map_err(|e| format!("script: failed to serialize \"args\": {e}"))?;
        stdin
            .write_all(&payload)
            .await
            .map_err(|e| format!("script: failed to write stdin: {e}"))?;
    }
    drop(stdin); // EOF, whether or not anything was written.

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("script: failed waiting for the process: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "script: exited with {} — {}",
            output.status,
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| {
            "script: no stdout output — the last line must be a JSON object".to_string()
        })?;
    match serde_json::from_str::<Value>(last_line) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("script: last stdout line wasn't a JSON object".to_string()),
        Err(e) => Err(format!("script: last stdout line wasn't valid JSON: {e}")),
    }
}

async fn execute_delay(action: &Action) -> Result<Map<String, Value>, String> {
    let ms = require_f64(action, "ms")?;
    if ms < 0.0 {
        return Err("delay: \"ms\" must be >= 0".to_string());
    }
    on_tokio(async move {
        tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
    })
    .await;
    Ok(Map::new())
}

/// Webhook-based (decided 2026-09-12, superseding the original "UI-only
/// toast" v1 decision — no `Window`/`App` needed for this module to stay
/// gpui-free, just like `http`). POSTs a Slack/Mattermost-compatible
/// JSON body (`{"channel": ..., "text": ...}`, `channel` omitted when not
/// set — that shape is readable by a generic webhook receiver too, not
/// just those two specifically) to `url`. A non-2xx response or network
/// failure resolves `Failed`, same as `http`.
async fn execute_notify(action: &Action, ctx: &RunContext) -> Result<Map<String, Value>, String> {
    let url = resolve_string_field(action, "url", ctx)?;
    let channel = action
        .inputs
        .contains_key("channel")
        .then(|| resolve_string_field(action, "channel", ctx))
        .transpose()?;
    let message = resolve_string_field(action, "message", ctx)?;

    on_tokio(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| format!("notify: failed to build client: {e}"))?;

        let mut body = Map::new();
        if let Some(channel) = channel {
            body.insert("channel".to_string(), Value::String(channel));
        }
        body.insert("text".to_string(), Value::String(message));

        let body_text = serde_json::to_string(&Value::Object(body))
            .map_err(|e| format!("notify: failed to serialize body: {e}"))?;
        let res = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body_text)
            .send()
            .await
            .map_err(|e| format!("notify: network error: {e}"))?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            return Err(format!(
                "notify: webhook returned {} — {}",
                status.as_u16(),
                text.trim()
            ));
        }

        let mut outputs = Map::new();
        outputs.insert("status".to_string(), Value::from(status.as_u16()));
        Ok(outputs)
    })
    .await
}

async fn execute_log(action: &Action, ctx: &RunContext) -> Result<Map<String, Value>, String> {
    let level = action
        .inputs
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info");
    let message = resolve_string_field(action, "message", ctx)?;
    match level {
        "error" => log::error!("[workflow] {message}"),
        "warn" | "warning" => log::warn!("[workflow] {message}"),
        "debug" => log::debug!("[workflow] {message}"),
        _ => log::info!("[workflow] {message}"),
    }
    // The message is also the action's output. The app's log file is the
    // wrong place to go looking for what a flow said: a run reports each
    // action's outputs, so this is how the text reaches the run log.
    let mut outputs = Map::new();
    outputs.insert("level".to_string(), Value::String(level.to_string()));
    outputs.insert("message".to_string(), Value::String(message));
    Ok(outputs)
}

// FUTURE FEATURE — `sendEmail` is parked (see `registry::REGISTRY`). This was
// only ever a log-only stand-in (design doc §4, decided 2026-09-10): there is
// no SMTP or mail-API transport in this tree. The field resolution below is
// the part worth keeping; the `log::info!` is where a real send goes.
//
// async fn execute_send_email(
//     action: &Action,
//     ctx: &RunContext,
// ) -> Result<Map<String, Value>, String> {
//     let to = resolve_string_field(action, "to", ctx)?;
//     let subject = resolve_string_field(action, "subject", ctx)?;
//     let body = resolve_string_field(action, "body", ctx)?;
//     log::info!("[workflow sendEmail] would send to={to:?} subject={subject:?}: {body}");
//     Ok(Map::new())
// }

/// How long one connection attempt may take before the next poll.
const PORT_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// Tries `127.0.0.1` and `[::1]` at once. A server bound to only one of them
/// is up all the same: Vite, for one, can listen on `[::1]` alone, and Django
/// on `127.0.0.1` alone. Returns the address that answered, or what each said.
async fn connect_to_loopback(port: u16) -> Result<&'static str, String> {
    async fn attempt(addr: SocketAddr) -> Result<(), String> {
        match tokio::time::timeout(PORT_CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("{addr}: {e}")),
            Err(_) => Err(format!("{addr}: no answer within {PORT_CONNECT_TIMEOUT:?}")),
        }
    }
    let (v4, v6) = futures::future::join(
        attempt(SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
        attempt(SocketAddr::from((Ipv6Addr::LOCALHOST, port))),
    )
    .await;
    match (v4, v6) {
        (Ok(()), _) => Ok("127.0.0.1"),
        (_, Ok(())) => Ok("[::1]"),
        (Err(v4), Err(v6)) => Err(format!("{v4}; {v6}")),
    }
}

/// Whether `url` answers with a 2xx or 3xx status. `Err` says what it did
/// instead, for the timeout message.
async fn url_answers(client: &reqwest::Client, url: &str) -> Result<(), String> {
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() || response.status().is_redirection() => {
            Ok(())
        }
        Ok(response) => Err(format!("HTTP {}", response.status().as_u16())),
        Err(e) => Err(e.to_string()),
    }
}

/// Polls until a server answers on `port`, up to `timeout_ms` (default 30s).
///
/// "Answers" means a TCP connect to `127.0.0.1` or `[::1]` succeeds. With the
/// optional `url` input it also means that address returns 2xx or 3xx, which
/// shows the app is serving and not only that something owns the port (a
/// health endpoint is the usual choice). Both stages share the one timeout.
///
/// Outputs `ready` and `address`, the loopback address that answered, so a
/// flow author can see which one the server listens on.
async fn execute_wait_for_port(action: &Action) -> Result<Map<String, Value>, String> {
    let port = require_f64(action, "port")? as u16;
    let timeout_ms = action
        .inputs
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
    let url = action
        .inputs
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string);
    if let Some(url) = &url
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err(format!(
            "WaitForPort: \"url\" must start with http:// or https://, not \"{url}\""
        ));
    }

    on_tokio(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

        let address = loop {
            let connect_error = match connect_to_loopback(port).await {
                Ok(address) => break address,
                Err(error) => error,
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "WaitForPort: port {port} not ready after {timeout_ms}ms ({connect_error})"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        if let Some(url) = url {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                // A redirect is itself an answer; following it could end up
                // somewhere that is not up yet.
                .redirect_policy(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("WaitForPort: failed to build client: {e}"))?;
            loop {
                let last_url_error = match url_answers(&client, &url).await {
                    Ok(()) => break,
                    Err(error) => error,
                };
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!(
                        "WaitForPort: port {port} is open, but {url} did not return 2xx or 3xx within {timeout_ms}ms (last: {last_url_error})"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        let mut outputs = Map::new();
        outputs.insert("ready".to_string(), Value::Bool(true));
        outputs.insert("address".to_string(), Value::String(address.to_string()));
        Ok(outputs)
    })
    .await
}

/// How many of a failed process's last output lines go into the error.
const STARTUP_FAILURE_LOG_LINES: usize = 20;

/// Time for the output readers to hand over what a process printed on its way
/// out, once it has exited: the pipes close with the process, but the readers
/// are separate tasks.
const OUTPUT_DRAIN: Duration = Duration::from_millis(200);

/// How long a node waits for its `ready_*` settings unless `ready_timeout_ms`
/// says otherwise.
const DEFAULT_READY_TIMEOUT_MS: u64 = 60_000;

/// How often a starting process is checked for exit, for a stop and for
/// readiness.
const READY_POLL: Duration = Duration::from_millis(100);

/// What a `StartProcess` node waits for before the nodes after it may start.
struct ReadySettings {
    port: Option<u16>,
    url: Option<String>,
    text: Option<String>,
    timeout: Duration,
}

impl ReadySettings {
    fn from_action(action: &Action) -> Result<Self, String> {
        let port = match action.inputs.get("ready_port") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port != 0)
                    .ok_or_else(|| {
                        "StartProcess: \"ready_port\" must be a port number from 1 to 65535"
                            .to_string()
                    })?,
            ),
        };
        let url = optional_text(action, "ready_url")?;
        if let Some(url) = &url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(format!(
                "StartProcess: \"ready_url\" must start with http:// or https://, not \"{url}\""
            ));
        }
        let text = optional_text(action, "ready_text")?;
        let timeout_ms = action
            .inputs
            .get("ready_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_READY_TIMEOUT_MS);
        Ok(Self {
            port,
            url,
            text,
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    /// With none of the three settings the node waits for the command to exit
    /// instead.
    fn is_empty(&self) -> bool {
        self.port.is_none() && self.url.is_none() && self.text.is_none()
    }
}

/// A text input that may be left out or blank.
fn optional_text(action: &Action, field: &str) -> Result<Option<String>, String> {
    match action.inputs.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            Ok((!text.is_empty()).then(|| text.to_string()))
        }
        Some(_) => Err(format!("StartProcess: \"{field}\" must be text")),
    }
}

/// Which ready settings have passed so far. Each stays passed once it has.
#[derive(Default)]
struct ReadyProgress {
    /// The loopback address that answered, once the port has.
    port: Option<&'static str>,
    url: bool,
    text: bool,
}

/// Checks the settings that have not passed yet, and says whether every one
/// that was asked for now has.
async fn check_ready(
    settings: &ReadySettings,
    progress: &mut ReadyProgress,
    client: Option<&reqwest::Client>,
    registry: &process_tracker::ProcessRegistry,
    process_id: &str,
) -> bool {
    if let Some(port) = settings.port
        && progress.port.is_none()
        && let Ok(address) = connect_to_loopback(port).await
    {
        progress.port = Some(address);
    }
    if let (Some(url), Some(client)) = (&settings.url, client)
        && !progress.url
        && url_answers(client, url).await.is_ok()
    {
        progress.url = true;
    }
    if let Some(text) = &settings.text
        && !progress.text
        && registry.log_contains(process_id, text)
    {
        progress.text = true;
    }
    (settings.port.is_none() || progress.port.is_some())
        && (settings.url.is_none() || progress.url)
        && (settings.text.is_none() || progress.text)
}

/// What is still being waited for, for the timeout message.
fn waiting_for(settings: &ReadySettings, progress: &ReadyProgress) -> Vec<String> {
    let mut waiting = Vec::new();
    if let Some(port) = settings.port
        && progress.port.is_none()
    {
        waiting.push(format!("port {port}"));
    }
    if let Some(url) = &settings.url
        && !progress.url
    {
        waiting.push(format!("{url} to return 2xx or 3xx"));
    }
    if let Some(text) = &settings.text
        && !progress.text
    {
        waiting.push(format!("\"{text}\" in its output"));
    }
    waiting
}

/// How long the output forwarder waits for a line before checking whether the
/// process has ended.
const OUTPUT_FORWARD_IDLE: Duration = Duration::from_millis(300);

/// How long after the last line the forwarder keeps listening once the
/// process has ended, for what is still on its way from the pipes.
const OUTPUT_FORWARD_DRAIN: Duration = Duration::from_millis(200);

/// Hands `sink` each line the process prints, as it arrives, until the process
/// has ended and its last lines have been passed on. `lines` must have been
/// subscribed before the output readers started, or the first lines are lost.
fn forward_output(
    registry: Arc<process_tracker::ProcessRegistry>,
    process_id: String,
    action_id: String,
    sink: StatusSink,
    mut lines: broadcast::Receiver<String>,
) {
    // The registry keeps stderr lines marked with `"! "`.
    fn pass_on(sink: &StatusSink, action_id: &str, line: &str) {
        match line.strip_prefix("! ") {
            Some(text) => sink.report_output(action_id, OutputStream::Stderr, text),
            None => sink.report_output(action_id, OutputStream::Stdout, line),
        }
    }

    tokio::spawn(async move {
        loop {
            match tokio::time::timeout(OUTPUT_FORWARD_IDLE, lines.recv()).await {
                Ok(Ok(line)) => pass_on(&sink, &action_id, &line),
                // Lines were dropped because this fell behind: carry on.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => return,
                Err(_) if registry.exit_code_if_exited(&process_id).is_some() => break,
                Err(_) => {}
            }
        }
        // The process has ended; pass on what was still in flight.
        while let Ok(received) = tokio::time::timeout(OUTPUT_FORWARD_DRAIN, lines.recv()).await {
            match received {
                Ok(line) => pass_on(&sink, &action_id, &line),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// The outputs every `StartProcess` result has.
fn process_outputs(pid: Option<u32>, process_id: &str) -> Map<String, Value> {
    let mut outputs = Map::new();
    outputs.insert(
        "pid".to_string(),
        pid.map(Value::from).unwrap_or(Value::Null),
    );
    outputs.insert(
        "processId".to_string(),
        Value::String(process_id.to_string()),
    );
    outputs
}

/// The result of a process that is up and still running.
fn running_outputs(
    pid: Option<u32>,
    process_id: &str,
    address: Option<&'static str>,
) -> Map<String, Value> {
    let mut outputs = process_outputs(pid, process_id);
    outputs.insert("ready".to_string(), Value::Bool(true));
    if let Some(address) = address {
        outputs.insert("address".to_string(), Value::String(address.to_string()));
    }
    outputs
}

/// The result of a process that ended: its exit code, with the last lines it
/// printed when that code is not 0.
fn finished(
    registry: &process_tracker::ProcessRegistry,
    process_id: &str,
    pid: Option<u32>,
    command_line: &str,
    code: i32,
) -> Result<Map<String, Value>, String> {
    if code != 0 {
        let output = registry.log_tail(process_id, STARTUP_FAILURE_LOG_LINES);
        return Err(exit_failure_message(command_line, code, &output));
    }
    let mut outputs = process_outputs(pid, process_id);
    outputs.insert("exit_code".to_string(), Value::from(0));
    outputs.insert("ready".to_string(), Value::Bool(false));
    Ok(outputs)
}

/// Waits for a process that has just been started, and returns the node's
/// result:
///
/// - with `ready_*` settings, once they all pass: the process is running and
///   the node is ready;
/// - when the process exits first, or when there are no settings and it exits:
///   its exit code (an error for any code but 0);
/// - when the settings do not pass in time: the process is stopped and the
///   node fails;
/// - when the run is stopped: the process is stopped and the node fails with
///   [`STOPPED_MESSAGE`].
async fn wait_for_start(
    registry: &process_tracker::ProcessRegistry,
    process_id: &str,
    pid: Option<u32>,
    command_line: &str,
    settings: &ReadySettings,
    control: &RunControl,
) -> Result<Map<String, Value>, String> {
    let client = if settings.url.is_some() {
        Some(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect_policy(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("StartProcess: failed to build client: {e}"))?,
        )
    } else {
        None
    };
    let mut progress = ReadyProgress::default();
    let deadline = tokio::time::Instant::now() + settings.timeout;

    loop {
        if control.is_stopped() {
            registry.stop(process_id).ok();
            return Err(STOPPED_MESSAGE.to_string());
        }
        if let Some(code) = registry.exit_code_if_exited(process_id) {
            tokio::time::sleep(OUTPUT_DRAIN).await;
            // The output may only have been read just now. A process that
            // printed what it was waited for and then ended was ready: the run
            // reports how it ended.
            if !settings.is_empty()
                && check_ready(
                    settings,
                    &mut progress,
                    client.as_ref(),
                    registry,
                    process_id,
                )
                .await
            {
                return Ok(running_outputs(pid, process_id, progress.port));
            }
            return finished(registry, process_id, pid, command_line, code);
        }
        if !settings.is_empty() {
            if check_ready(
                settings,
                &mut progress,
                client.as_ref(),
                registry,
                process_id,
            )
            .await
            {
                return Ok(running_outputs(pid, process_id, progress.port));
            }
            if tokio::time::Instant::now() >= deadline {
                let waiting = waiting_for(settings, &progress);
                registry.stop(process_id).ok();
                tokio::time::sleep(OUTPUT_DRAIN).await;
                let output = registry.log_tail(process_id, STARTUP_FAILURE_LOG_LINES);
                return Err(not_ready_message(
                    command_line,
                    settings.timeout,
                    &waiting,
                    &output,
                ));
            }
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// Starts a command and follows its life. The node:
///
/// - **ends with the command**, when no `ready_*` setting is given: `Succeeded`
///   on exit code 0, `Failed` (with the last lines it printed) on any other
///   code. This is a build, a test run, a migration.
/// - **is ready while the command keeps running**, when `ready_port`,
///   `ready_url` or `ready_text` is given (all that are given must pass). A
///   dev server never ends on its own, so these say when it is up. The node
///   then returns `ready: true` and the nodes after it can start, while the
///   run keeps watching the server (see `runner::run_workflow_controlled`)
///   until it exits or the run is stopped. If the command exits first, or
///   the settings do not pass within `ready_timeout_ms` (default 60 s), the
///   node fails.
///
/// Servers an *earlier run of the same flow* left running for this same node
/// are stopped first, so re-running a flow replaces its servers and does not
/// pile up beside them.
///
/// `args` is a JSON array of strings, not one shell command-line string, which
/// keeps the schema free of quoting ambiguity. `cwd`, when given, resolves
/// against `solution_root` (same convention as `script`'s `file` mode). The
/// spawned child is handed to `process_tracker`'s registry; its stdout and
/// stderr are piped and drained by background readers
/// (`process_tracker::spawn_log_reader`) so the pipe buffer can never fill and
/// block the child, and each line lands in that process's own retained log.
///
/// **Windows-only wrinkle:** `Command::new` can only launch real PE
/// executables, but `npm`, `npx` and `flask` are `.cmd` shims, which only the
/// command interpreter can run. So the command line goes through `cmd.exe`,
/// meaning the *tracked* child is `cmd.exe` and not the real program;
/// `process_tracker::stop`'s tree-kill exists to account for that.
async fn execute_start_process(
    action_id: &str,
    ctx: &RunContext,
    action: &Action,
    solution_root: &Path,
) -> Result<Map<String, Value>, String> {
    let command = require_str(action, "command")?.to_string();
    let args: Vec<String> = action
        .inputs
        .get("args")
        .map(|v| {
            v.as_array()
                .ok_or_else(|| "StartProcess: \"args\" must be an array of strings".to_string())?
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        "StartProcess: \"args\" must be an array of strings".to_string()
                    })
                })
                .collect::<Result<Vec<String>, String>>()
        })
        .transpose()?
        .unwrap_or_default();
    // Without a `cwd` the process starts in the project root, as the input's
    // help text says, not wherever the app happened to be launched from.
    let cwd = action
        .inputs
        .get("cwd")
        .and_then(Value::as_str)
        .map(|c| solution_root.join(c))
        .or_else(|| working_dir(solution_root).map(Path::to_path_buf));
    let url = action
        .inputs
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string);
    let ready = ReadySettings::from_action(action)?;
    let action_id = action_id.to_string();
    let owner = ctx.process_owner().cloned();
    let control = ctx.control().clone();
    // Only a caller that asked for output gets it forwarded.
    let output = ctx
        .status_sink()
        .filter(|sink| sink.wants_output())
        .cloned();

    on_tokio(async move {
        if let Some(owner) = &owner {
            let stopped = process_tracker::registry().stop_running_from_earlier_runs(owner, &action_id);
            if stopped > 0 {
                log::info!(
                    "[workflow StartProcess] stopped {stopped} process(es) that an earlier run of \"{action_id}\" left running"
                );
            }
        }

        #[cfg(target_os = "windows")]
        let mut cmd = {
            use std::os::windows::process::CommandExt as _;
            let mut c = TokioCommand::new("cmd.exe");
            // Raw, not `args`: `args` wraps an argument that has a space in
            // quotes and escapes the quotes inside it as `\"`, which cmd.exe
            // does not understand, so a path with a space reached the shell
            // mangled. `/S` makes cmd strip exactly the outer pair of quotes
            // added here and take the rest of the line as it is.
            c.as_std_mut()
                .raw_arg("/S")
                .raw_arg("/C")
                .raw_arg(format!("\"{}\"", windows_command_line(&command, &args)));
            c
        };
        #[cfg(not(target_os = "windows"))]
        let mut cmd = {
            let mut c = TokioCommand::new(&command);
            // Its own process group, so stopping it from the Processes panel
            // takes everything it started (`process_tracker::stop`).
            c.args(&args).process_group(0);
            c
        };

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
        }
        // As for scripts: the IDE's own runtimes first, so `npm run dev`
        // starts under the project's Node.js version.
        if let Some(path) = ide_env::path_env(cwd.as_deref()) {
            cmd.env("PATH", path);
        }
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash.

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("StartProcess: failed to start \"{command}\": {e}"))?;
        let pid = child.id();
        // Taken before `child` moves into `register` — piped stdout/stderr
        // are read by dedicated background readers (`spawn_log_reader`),
        // not by anything that owns `child` itself, so the Processes panel
        // can show live output without `process_tracker` needing to know
        // anything about reading pipes.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let command_line = std::iter::once(command.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let registry = process_tracker::registry();
        let report_id = action_id.clone();
        let process_id = registry.register_owned(owner, action_id, command, args, url, child);
        // Subscribed before the readers start, so the first lines are not lost.
        let lines = output
            .as_ref()
            .and_then(|_| registry.subscribe_log(&process_id));
        if let Some(stdout) = stdout {
            process_tracker::spawn_log_reader(process_id.clone(), stdout, false);
        }
        if let Some(stderr) = stderr {
            process_tracker::spawn_log_reader(process_id.clone(), stderr, true);
        }
        if let (Some(sink), Some(lines)) = (output, lines) {
            forward_output(registry.clone(), process_id.clone(), report_id, sink, lines);
        }

        wait_for_start(&registry, &process_id, pid, &command_line, &ready, &control).await
    })
    .await
}

/// The error for a process that exited with `code`: what it was, and what it
/// printed on the way out.
fn exit_failure_message(command_line: &str, code: i32, output: &[String]) -> String {
    let mut message = format!("StartProcess: \"{command_line}\" exited with code {code}");
    append_output(&mut message, output);
    message
}

/// The error for a process whose ready settings did not pass in time.
fn not_ready_message(
    command_line: &str,
    timeout: Duration,
    waiting: &[String],
    output: &[String],
) -> String {
    let mut message = format!(
        "StartProcess: \"{command_line}\" was not ready after {}ms (still waiting for {})",
        timeout.as_millis(),
        waiting.join(", ")
    );
    append_output(&mut message, output);
    message
}

fn append_output(message: &mut String, output: &[String]) {
    let output = output
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if output.is_empty() {
        message.push_str(" (it printed nothing)");
    } else {
        message.push_str(":\n");
        message.push_str(&output.join("\n"));
    }
}

/// Builds the `cmd.exe /C` command line for [`execute_start_process`] —
/// double-quotes any piece containing whitespace/quotes (covers the common
/// case: a path with spaces, an arg with an embedded flag value), not a
/// fully general `cmd.exe` escaper — `cmd.exe` quoting rules are notoriously
/// inconsistent even in the real shell, and `script_runner_panel`'s own
/// shelling-out doesn't attempt full generality either.
#[cfg(target_os = "windows")]
fn windows_command_line(command: &str, args: &[String]) -> String {
    let mut line = quote_windows_arg(command);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_windows_arg(arg));
    }
    line
}

#[cfg(target_os = "windows")]
fn quote_windows_arg(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

/// Direct port of the old `chain.rs`'s `execute_set_env_step`. Process-
/// global, same as the original — not sandboxed per-run.
fn execute_set_env(action: &Action) -> Result<Map<String, Value>, String> {
    let key = require_str(action, "key")?;
    let value = require_str(action, "value")?;
    // SAFETY: matches the existing `unsafe { std::env::set_var(...) }`
    // call sites already in this codebase (e.g. `zed/util/src/util.rs`) —
    // edition 2024 marks `set_var` unsafe because it's not thread-safe to
    // race with a concurrent `getenv` on some platforms; this call site
    // has no such concurrent reader.
    unsafe {
        std::env::set_var(key, value);
    }
    Ok(Map::new())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn action_with_inputs(type_id: &str, inputs: Vec<(&str, Value)>) -> Action {
        let mut action = Action::new(type_id);
        for (k, v) in inputs {
            action.inputs.insert(k.to_string(), v);
        }
        action
    }

    /// Spawns a one-shot raw HTTP mock server on an OS-assigned local port
    /// (no real network access, no external dependency) that answers every
    /// connection with `body` as a `200 application/json` response, then
    /// returns the `http://127.0.0.1:<port>/` URL to hit it.
    async fn mock_json_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        format!("http://127.0.0.1:{port}/")
    }

    #[tokio::test]
    async fn http_get_parses_json_body_and_status() {
        let url = mock_json_server(r#"{"total": 42}"#).await;
        let action = action_with_inputs(
            "http",
            vec![("method", Value::from("GET")), ("url", Value::from(url))],
        );
        let outputs = execute_http(&action, &RunContext::new()).await.unwrap();
        assert_eq!(outputs.get("status"), Some(&Value::from(200)));
        assert_eq!(outputs.get("body"), Some(&serde_json::json!({"total": 42})));
    }

    #[tokio::test]
    async fn http_rejects_a_missing_url() {
        let action = action_with_inputs("http", vec![("method", Value::from("GET"))]);
        let err = execute_http(&action, &RunContext::new())
            .await
            .unwrap_err();
        assert!(err.contains("url"));
    }

    #[tokio::test]
    async fn http_fails_on_a_non_2xx_response() {
        let received = Arc::new(Mutex::new(None));
        let url = mock_webhook_server(500, received).await;
        let action = action_with_inputs(
            "http",
            vec![("method", Value::from("GET")), ("url", Value::from(url))],
        );
        let err = execute_http(&action, &RunContext::new())
            .await
            .unwrap_err();
        assert!(err.contains("returned 500"), "{err}");
    }

    #[tokio::test]
    async fn http_uppercases_the_method_and_sends_the_evaluated_body_expression() {
        let ctx = RunContext::new();
        ctx.record_outputs(
            "normalize",
            serde_json::json!({"result": {"sku": "ITEM-1"}})
                .as_object()
                .unwrap()
                .clone(),
        );
        let received = Arc::new(Mutex::new(None));
        let url = mock_webhook_server(200, received.clone()).await;
        let action = action_with_inputs(
            "http",
            vec![
                ("method", Value::from("post")),
                ("url", Value::from(url)),
                (
                    "bodyExpression",
                    serde_json::json!({"var": "normalize.outputs.result"}),
                ),
            ],
        );
        execute_http(&action, &ctx).await.unwrap();

        let (method, _, body) = received.lock().unwrap().clone().unwrap();
        assert_eq!(method, "POST");
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, serde_json::json!({"sku": "ITEM-1"}));
    }

    #[tokio::test]
    async fn http_rejects_body_and_body_expression_together() {
        let action = action_with_inputs(
            "http",
            vec![
                ("method", Value::from("POST")),
                ("url", Value::from("http://127.0.0.1:1/")),
                ("body", serde_json::json!({"a": 1})),
                ("bodyExpression", serde_json::json!({"var": "x"})),
            ],
        );
        let err = execute_http(&action, &RunContext::new())
            .await
            .unwrap_err();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn http_headers_are_literal_or_evaluated() {
        let ctx = RunContext::new();
        ctx.record_outputs(
            "login",
            serde_json::json!({"token": "abc"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let action = action_with_inputs(
            "http",
            vec![(
                "headers",
                serde_json::json!({
                    "Accept": "application/json",
                    "X-Token": {"var": "login.outputs.token"},
                    "X-Retry": 3,
                }),
            )],
        );
        let headers = http_headers(&action, &ctx).unwrap();
        assert!(headers.contains(&("Accept".to_string(), "application/json".to_string())));
        assert!(headers.contains(&("X-Token".to_string(), "abc".to_string())));
        assert!(headers.contains(&("X-Retry".to_string(), "3".to_string())));

        let bad = action_with_inputs("http", vec![("headers", Value::from("nope"))]);
        assert!(http_headers(&bad, &ctx).unwrap_err().contains("JSON object"));
    }

    #[tokio::test]
    async fn transform_evaluates_expression_against_run_context() {
        let ctx = RunContext::new();
        ctx.record_outputs(
            "fetchOrder",
            serde_json::json!({"total": 42})
                .as_object()
                .unwrap()
                .clone(),
        );
        let action = action_with_inputs(
            "transform",
            vec![(
                "expression",
                serde_json::json!({"var": "fetchOrder.outputs.total"}),
            )],
        );
        let outputs = execute_transform(&action, &ctx).await.unwrap();
        assert_eq!(outputs.get("result"), Some(&Value::from(42)));
    }

    #[tokio::test]
    async fn delay_actually_waits_at_least_the_requested_time() {
        let action = action_with_inputs("delay", vec![("ms", Value::from(30))]);
        let started = std::time::Instant::now();
        execute_delay(&action).await.unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(30));
    }

    #[tokio::test]
    async fn wait_for_port_succeeds_once_something_is_listening() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Keep the listener alive for the duration of the wait.
        let _keep_alive = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let action = action_with_inputs(
            "WaitForPort",
            vec![
                ("port", Value::from(port)),
                ("timeout_ms", Value::from(2000)),
            ],
        );
        let outputs = execute_wait_for_port(&action).await.unwrap();
        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
        assert_eq!(outputs.get("address"), Some(&Value::from("127.0.0.1")));
    }

    #[tokio::test]
    async fn wait_for_port_times_out_when_nothing_is_listening() {
        // A bound-then-immediately-dropped listener frees the port but is
        // very likely nothing else grabs it in the ~150ms this test runs.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let action = action_with_inputs(
            "WaitForPort",
            vec![
                ("port", Value::from(port)),
                ("timeout_ms", Value::from(150)),
            ],
        );
        let err = execute_wait_for_port(&action).await.unwrap_err();
        assert!(err.contains("not ready"));
    }

    /// A local HTTP server that answers every request, with the next status
    /// from `statuses` each time and the last one for every request after
    /// that. Returns its `http://127.0.0.1:<port>/` address.
    async fn mock_status_server(statuses: &'static [u16]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut served = 0;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let status = statuses[served.min(statuses.len() - 1)];
                served += 1;
                // One task per connection, so a client that connects and says
                // nothing (the port probe) never holds up the next request.
                tokio::spawn(async move {
                    // Read the whole request head before answering: closing a
                    // socket with unread data makes Windows reset the
                    // connection, and the client then sees an error instead of
                    // the status.
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 {status} Status\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                    // Wait for the client to hang up first.
                    let _ = tokio::time::timeout(Duration::from_millis(500), socket.read(&mut buf))
                        .await;
                });
            }
        });
        format!("http://127.0.0.1:{port}/")
    }

    #[tokio::test]
    async fn wait_for_port_sees_a_server_that_listens_on_ipv6_only() {
        // Vite can bind `[::1]` alone; a machine without IPv6 cannot run this.
        let Ok(listener) = TcpListener::bind("[::1]:0").await else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let _keep_alive = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let action = action_with_inputs(
            "WaitForPort",
            vec![
                ("port", Value::from(port)),
                ("timeout_ms", Value::from(2000)),
            ],
        );
        let outputs = execute_wait_for_port(&action).await.unwrap();
        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
        assert_eq!(outputs.get("address"), Some(&Value::from("[::1]")));
    }

    #[tokio::test]
    async fn wait_for_port_timeout_says_which_addresses_it_tried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let action = action_with_inputs(
            "WaitForPort",
            vec![
                ("port", Value::from(port)),
                ("timeout_ms", Value::from(150)),
            ],
        );
        let err = execute_wait_for_port(&action).await.unwrap_err();
        assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
        assert!(err.contains(&format!("[::1]:{port}")), "{err}");
    }

    fn wait_with_url(url: &str, timeout_ms: u64) -> Action {
        let port: u16 = url
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches('/')
            .parse()
            .unwrap();
        action_with_inputs(
            "WaitForPort",
            vec![
                ("port", Value::from(port)),
                ("url", Value::from(url)),
                ("timeout_ms", Value::from(timeout_ms)),
            ],
        )
    }

    #[tokio::test]
    async fn wait_for_port_with_a_url_keeps_waiting_until_it_answers_well() {
        let url = mock_status_server(&[500, 503, 200]).await;
        let outputs = execute_wait_for_port(&wait_with_url(&url, 5000))
            .await
            .unwrap();
        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
    }

    #[tokio::test]
    async fn wait_for_port_with_a_url_times_out_when_it_keeps_failing() {
        let url = mock_status_server(&[500]).await;
        let err = execute_wait_for_port(&wait_with_url(&url, 400))
            .await
            .unwrap_err();
        assert!(err.contains("is open, but"), "{err}");
        assert!(err.contains("HTTP 500"), "{err}");
    }

    #[tokio::test]
    async fn wait_for_port_with_a_url_counts_a_redirect_as_an_answer() {
        let url = mock_status_server(&[302]).await;
        execute_wait_for_port(&wait_with_url(&url, 2000))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wait_for_port_rejects_a_url_that_is_not_http() {
        let action = action_with_inputs(
            "WaitForPort",
            vec![
                ("port", Value::from(8000)),
                ("url", Value::from("ftp://127.0.0.1/")),
            ],
        );
        let err = execute_wait_for_port(&action).await.unwrap_err();
        assert!(err.contains("http://"), "{err}");
    }

    /// A command that keeps running for `seconds`, in the shape `StartProcess`
    /// takes. (Not `timeout /T`: it quits at once when its input is not a
    /// console, which `StartProcess` never gives it.)
    fn long_running_command(seconds: u64) -> (&'static str, Vec<String>) {
        #[cfg(target_os = "windows")]
        return (
            "cmd",
            vec![
                "/C".to_string(),
                format!("ping -n {} 127.0.0.1 > nul", seconds + 1),
            ],
        );
        #[cfg(not(target_os = "windows"))]
        return ("sleep", vec![seconds.to_string()]);
    }

    /// A command that prints `boom` on stderr and exits with `code`.
    fn failing_command(code: i32) -> (&'static str, Vec<String>) {
        #[cfg(target_os = "windows")]
        return (
            "cmd",
            vec!["/C".to_string(), format!("echo boom 1>&2 & exit {code}")],
        );
        #[cfg(not(target_os = "windows"))]
        return (
            "sh",
            vec!["-c".to_string(), format!("echo boom >&2; exit {code}")],
        );
    }

    /// A command that prints `text`, then keeps running for 30 seconds.
    fn prints_then_runs(text: &str) -> (&'static str, Vec<String>) {
        #[cfg(target_os = "windows")]
        return (
            "cmd",
            vec![
                "/C".to_string(),
                format!("echo {text} & ping -n 31 127.0.0.1 > nul"),
            ],
        );
        #[cfg(not(target_os = "windows"))]
        return (
            "sh",
            vec!["-c".to_string(), format!("echo {text}; sleep 30")],
        );
    }

    fn start_action(command: &str, args: Vec<String>, extra: Vec<(&str, Value)>) -> Action {
        let mut inputs = vec![
            ("command", Value::from(command)),
            ("args", serde_json::json!(args)),
        ];
        inputs.extend(extra);
        action_with_inputs("StartProcess", inputs)
    }

    fn owner(flow: &str, run: &str) -> process_tracker::ProcessOwner {
        process_tracker::ProcessOwner {
            flow: flow.to_string(),
            run: run.to_string(),
        }
    }

    fn root() -> &'static std::path::Path {
        std::path::Path::new(".")
    }

    /// A TCP listener that stays open for the rest of the test, so a
    /// `ready_port` pointing at it is always ready. Returns its port.
    async fn open_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        port
    }

    fn process_id_of(outputs: &Map<String, Value>) -> String {
        outputs["processId"].as_str().unwrap().to_string()
    }

    fn process_status(process_id: &str) -> process_tracker::ProcessStatus {
        process_tracker::registry()
            .list()
            .iter()
            .find(|p| p.id == process_id)
            .map(|p| p.status)
            .unwrap()
    }

    /// Whether any process started by the node `action_id` is still running.
    fn any_running(action_id: &str) -> bool {
        process_tracker::registry().list().iter().any(|p| {
            p.action_id == action_id && p.status == process_tracker::ProcessStatus::Running
        })
    }

    #[tokio::test]
    async fn a_command_that_fails_fails_the_node_with_its_output() {
        let (command, args) = failing_command(3);
        let action = start_action(command, args, vec![]);
        let err = execute_start_process("s1", &RunContext::new(), &action, root())
            .await
            .unwrap_err();
        assert!(err.contains("exited with code 3"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    #[tokio::test]
    async fn a_command_that_ends_cleanly_finishes_the_node() {
        #[cfg(target_os = "windows")]
        let (command, args) = ("cmd", vec!["/C".to_string(), "exit 0".to_string()]);
        #[cfg(not(target_os = "windows"))]
        let (command, args) = ("true", Vec::new());
        let action = start_action(command, args, vec![]);
        let outputs = execute_start_process("s1", &RunContext::new(), &action, root())
            .await
            .unwrap();
        assert_eq!(outputs.get("exit_code"), Some(&Value::from(0)));
        assert_eq!(outputs.get("ready"), Some(&Value::Bool(false)));
    }

    #[tokio::test]
    async fn with_no_ready_settings_the_node_lasts_as_long_as_the_command() {
        let (command, args) = long_running_command(1);
        let action = start_action(command, args, vec![]);
        let started = std::time::Instant::now();
        let outputs = execute_start_process("s1", &RunContext::new(), &action, root())
            .await
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(800),
            "the node returned after {:?}, before the command ended",
            started.elapsed()
        );
        assert_eq!(outputs.get("exit_code"), Some(&Value::from(0)));
    }

    #[tokio::test]
    async fn ready_port_lets_the_node_finish_while_the_server_keeps_running() {
        let port = open_port().await;
        let (command, args) = long_running_command(30);
        let action = start_action(command, args, vec![("ready_port", Value::from(port))]);
        let outputs = execute_start_process("web", &RunContext::new(), &action, root())
            .await
            .unwrap();

        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
        assert_eq!(outputs.get("address"), Some(&Value::from("127.0.0.1")));
        let id = process_id_of(&outputs);
        assert_eq!(process_status(&id), process_tracker::ProcessStatus::Running);
        process_tracker::registry().stop(&id).unwrap();
    }

    #[tokio::test]
    async fn ready_text_waits_for_the_output() {
        let (command, args) = prints_then_runs("server listening");
        let action = start_action(
            command,
            args,
            vec![("ready_text", Value::from("listening"))],
        );
        let outputs = execute_start_process("web", &RunContext::new(), &action, root())
            .await
            .unwrap();

        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
        process_tracker::registry()
            .stop(&process_id_of(&outputs))
            .unwrap();
    }

    type SeenOutput = Arc<Mutex<Vec<(String, OutputStream, String)>>>;

    /// A status sink that collects the output lines it is given.
    fn collecting_output_sink() -> (StatusSink, SeenOutput) {
        let seen: SeenOutput = Arc::new(Mutex::new(Vec::new()));
        let seen_in_sink = seen.clone();
        let sink = StatusSink::none().with_output(move |_scope, action_id, stream, line| {
            seen_in_sink
                .lock()
                .unwrap()
                .push((action_id.to_string(), stream, line.to_string()))
        });
        (sink, seen)
    }

    /// Waits (the forwarder is a separate task) for a line containing `needle`.
    async fn wait_for_line(
        seen: &SeenOutput,
        needle: &str,
    ) -> Option<(String, OutputStream, String)> {
        for _ in 0..80 {
            if let Some(found) = seen
                .lock()
                .unwrap()
                .iter()
                .find(|(_, _, line)| line.contains(needle))
                .cloned()
            {
                return Some(found);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    #[tokio::test]
    async fn a_commands_output_is_passed_to_the_sink_while_it_runs() {
        let (sink, seen) = collecting_output_sink();
        let ctx = RunContext::new().with_status_sink(sink);
        let (command, args) = prints_then_runs("hello from the server");
        let action = start_action(
            command,
            args,
            vec![("ready_text", Value::from("hello from"))],
        );

        let outputs = execute_start_process("chatty", &ctx, &action, root())
            .await
            .unwrap();

        let found = wait_for_line(&seen, "hello from the server")
            .await
            .expect("the line was never passed on");
        assert_eq!(found.0, "chatty");
        assert_eq!(found.1, OutputStream::Stdout);
        process_tracker::registry()
            .stop(&process_id_of(&outputs))
            .unwrap();
    }

    #[tokio::test]
    async fn stderr_lines_are_marked_as_stderr_and_lose_the_registrys_mark() {
        let (sink, seen) = collecting_output_sink();
        let ctx = RunContext::new().with_status_sink(sink);
        let (command, args) = failing_command(3);
        let action = start_action(command, args, vec![]);

        // The command fails; its output is still passed on.
        let _ = execute_start_process("noisy", &ctx, &action, root()).await;

        let found = wait_for_line(&seen, "boom")
            .await
            .expect("the line was never passed on");
        assert_eq!(found.1, OutputStream::Stderr);
        assert!(!found.2.starts_with("! "), "{:?}", found.2);
    }

    #[tokio::test]
    async fn a_sink_that_did_not_ask_for_output_gets_none_and_the_node_still_works() {
        let seen: SeenOutput = Arc::new(Mutex::new(Vec::new()));
        // A status sink with no output callback.
        let ctx = RunContext::new().with_status_sink(StatusSink::new(|_, _, _| {}));
        let (command, args) = prints_then_runs("nobody is listening");
        let action = start_action(command, args, vec![("ready_text", Value::from("nobody"))]);

        let outputs = execute_start_process("quiet", &ctx, &action, root())
            .await
            .unwrap();

        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
        assert!(seen.lock().unwrap().is_empty());
        process_tracker::registry()
            .stop(&process_id_of(&outputs))
            .unwrap();
    }

    #[tokio::test]
    async fn ready_url_waits_for_a_good_answer() {
        let url = mock_status_server(&[500, 200]).await;
        let (command, args) = long_running_command(30);
        let action = start_action(command, args, vec![("ready_url", Value::from(url))]);
        let outputs = execute_start_process("web", &RunContext::new(), &action, root())
            .await
            .unwrap();

        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
        process_tracker::registry()
            .stop(&process_id_of(&outputs))
            .unwrap();
    }

    #[tokio::test]
    async fn every_ready_setting_must_pass() {
        let port = open_port().await;
        let (command, args) = long_running_command(30);
        let action = start_action(
            command,
            args,
            vec![
                ("ready_port", Value::from(port)),
                ("ready_text", Value::from("never printed")),
                ("ready_timeout_ms", Value::from(400)),
            ],
        );
        let err = execute_start_process("all-must-pass", &RunContext::new(), &action, root())
            .await
            .unwrap_err();

        assert!(err.contains("was not ready after 400ms"), "{err}");
        // The port had passed, so only the text is still waited for.
        assert!(err.contains("\"never printed\" in its output"), "{err}");
        assert!(!err.contains(&format!("port {port}")), "{err}");
        assert!(!any_running("all-must-pass"));
    }

    #[tokio::test]
    async fn a_server_that_never_gets_ready_is_stopped_and_fails_the_node() {
        let (command, args) = long_running_command(30);
        let action = start_action(
            command,
            args,
            vec![
                ("ready_text", Value::from("never printed")),
                ("ready_timeout_ms", Value::from(300)),
            ],
        );
        let err = execute_start_process("stuck-server", &RunContext::new(), &action, root())
            .await
            .unwrap_err();

        assert!(err.contains("was not ready after 300ms"), "{err}");
        assert!(!any_running("stuck-server"));
    }

    #[tokio::test]
    async fn a_process_that_exits_before_it_is_ready_fails_with_its_output() {
        let (command, args) = failing_command(4);
        let action = start_action(
            command,
            args,
            vec![
                ("ready_text", Value::from("never printed")),
                ("ready_timeout_ms", Value::from(30_000)),
            ],
        );
        let started = std::time::Instant::now();
        let err = execute_start_process("s1", &RunContext::new(), &action, root())
            .await
            .unwrap_err();

        assert!(err.contains("exited with code 4"), "{err}");
        assert!(err.contains("boom"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it waited for the timeout instead of noticing the exit"
        );
    }

    #[tokio::test]
    async fn a_process_that_printed_its_ready_text_and_then_ended_was_ready() {
        #[cfg(target_os = "windows")]
        let (command, args) = (
            "cmd",
            vec!["/C".to_string(), "echo all systems go".to_string()],
        );
        #[cfg(not(target_os = "windows"))]
        let (command, args) = (
            "sh",
            vec!["-c".to_string(), "echo all systems go".to_string()],
        );
        let action = start_action(
            command,
            args,
            vec![("ready_text", Value::from("systems go"))],
        );

        let outputs = execute_start_process("brief", &RunContext::new(), &action, root())
            .await
            .unwrap();

        assert_eq!(outputs.get("ready"), Some(&Value::Bool(true)));
    }

    #[tokio::test]
    async fn waiting_for_ready_ends_when_the_run_is_stopped() {
        let control = RunControl::new();
        let ctx = RunContext::new().with_control(control.clone());
        let (command, args) = long_running_command(30);
        let action = start_action(
            command,
            args,
            vec![
                ("ready_text", Value::from("never printed")),
                ("ready_timeout_ms", Value::from(30_000)),
            ],
        );
        let stopper = async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            control.stop();
        };

        let (result, ()) = futures::join!(
            execute_start_process("stoppable", &ctx, &action, root()),
            stopper
        );

        assert_eq!(result.unwrap_err(), STOPPED_MESSAGE);
        assert!(!any_running("stoppable"));
    }

    #[tokio::test]
    async fn bad_ready_settings_are_rejected_before_anything_starts() {
        let (command, args) = long_running_command(30);
        let cases = [
            ("ready_port", Value::from(0), "ready_port"),
            ("ready_port", Value::from(70_000), "ready_port"),
            ("ready_url", Value::from("ftp://127.0.0.1/"), "ready_url"),
            ("ready_text", Value::from(5), "ready_text"),
        ];
        for (field, value, expected) in cases {
            let action = start_action(command, args.clone(), vec![(field, value)]);
            let err = execute_start_process("rejected", &RunContext::new(), &action, root())
                .await
                .unwrap_err();
            assert!(err.contains(expected), "{field}: {err}");
        }
        assert!(!any_running("rejected"));
    }

    #[tokio::test]
    async fn start_process_replaces_what_an_earlier_run_left_running() {
        let port = open_port().await;
        let (command, args) = long_running_command(30);
        let action = start_action(command, args, vec![("ready_port", Value::from(port))]);
        let start = |owner: process_tracker::ProcessOwner| {
            let action = action.clone();
            let ctx = RunContext::new().with_process_owner(owner);
            async move {
                let outputs = execute_start_process("web", &ctx, &action, root())
                    .await
                    .unwrap();
                process_id_of(&outputs)
            }
        };

        let first = owner("project|flow-replace", "run-1");
        let before = start(first.clone()).await;
        assert_eq!(
            process_status(&before),
            process_tracker::ProcessStatus::Running
        );

        // The same run starting the same node again (a loop) keeps it.
        let same_run = start(first).await;
        assert_eq!(
            process_status(&before),
            process_tracker::ProcessStatus::Running
        );

        // Another flow's node with the same id is not touched either.
        let other_flow = start(owner("project|other", "run-9")).await;
        assert_eq!(
            process_status(&before),
            process_tracker::ProcessStatus::Running
        );

        // The next run of the flow replaces what the first run started.
        let after = start(owner("project|flow-replace", "run-2")).await;
        // Stopped, or already seen as exited once the kill has been noticed.
        assert_ne!(
            process_status(&before),
            process_tracker::ProcessStatus::Running
        );
        assert_ne!(
            process_status(&same_run),
            process_tracker::ProcessStatus::Running
        );
        assert_eq!(
            process_status(&after),
            process_tracker::ProcessStatus::Running
        );
        assert_eq!(
            process_status(&other_flow),
            process_tracker::ProcessStatus::Running
        );

        for id in [&after, &other_flow] {
            process_tracker::registry().stop(id).unwrap();
        }
    }

    #[tokio::test]
    async fn start_process_rejects_non_array_args() {
        let action = action_with_inputs(
            "StartProcess",
            vec![
                ("command", Value::from("echo")),
                ("args", Value::from("not an array")),
            ],
        );
        let err =
            execute_start_process("s1", &RunContext::new(), &action, std::path::Path::new("."))
                .await
                .unwrap_err();
        assert!(err.contains("array"));
    }

    // Not `start_process_fails_clearly_for_a_nonexistent_command` any more —
    // on Windows, `StartProcess` now always launches through `cmd.exe /C`
    // (see `execute_start_process`'s doc comment), and `cmd.exe` itself
    // spawns successfully regardless of whether the *inner* command exists;
    // that failure now happens asynchronously inside the (headless,
    // output-discarded) shell, not as a synchronous spawn error here. A
    // nonexistent `cwd`, by contrast, still fails `spawn()` itself on every
    // platform (the OS validates the working directory before starting
    // anything) — this is the assertion that survived that change.
    #[tokio::test]
    async fn start_process_fails_clearly_when_cwd_does_not_exist() {
        let action = action_with_inputs(
            "StartProcess",
            vec![
                ("command", Value::from("cmd")),
                ("cwd", Value::from("this/dir/does/not/exist")),
            ],
        );
        let err =
            execute_start_process("s1", &RunContext::new(), &action, std::path::Path::new("."))
                .await
                .unwrap_err();
        assert!(err.contains("failed to start"));
    }

    #[test]
    fn set_env_sets_a_process_env_var() {
        let action = action_with_inputs(
            "SetEnv",
            vec![
                ("key", Value::from("FORGE_WORKFLOW_TEST_VAR")),
                ("value", Value::from("hello")),
            ],
        );
        execute_set_env(&action).unwrap();
        assert_eq!(std::env::var("FORGE_WORKFLOW_TEST_VAR").unwrap(), "hello");
    }

    #[tokio::test]
    async fn log_outputs_what_it_logged() {
        let ctx = RunContext::new();
        let log_action = action_with_inputs(
            "log",
            vec![
                ("level", Value::from("info")),
                ("message", Value::from("hi")),
            ],
        );
        // What it logged is its output, which is how the text reaches a run's
        // own log and not only the app's.
        let outputs = execute_log(&log_action, &ctx).await.unwrap();
        assert_eq!(outputs.get("message"), Some(&Value::from("hi")));
        assert_eq!(outputs.get("level"), Some(&Value::from("info")));

        // FUTURE FEATURE — restore with `execute_send_email`.
        //
        // let email_action = action_with_inputs(
        //     "sendEmail",
        //     vec![
        //         ("to", Value::from("a@b.com")),
        //         ("subject", Value::from("subj")),
        //         ("body", Value::from("body")),
        //     ],
        // );
        // assert!(
        //     execute_send_email(&email_action, &ctx)
        //         .await
        //         .unwrap()
        //         .is_empty()
        // );
    }

    /// Spawns a one-shot raw HTTP mock server that records the request it
    /// received (method/path/body) into `received`, then answers with
    /// `status` and an empty JSON object — used for `notify`'s webhook
    /// POST, where the *outgoing* request shape matters more than the
    /// response.
    async fn mock_webhook_server(
        status: u16,
        received: Arc<Mutex<Option<(String, String, String)>>>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request_text = String::from_utf8_lossy(&buf[..n]).to_string();
            let mut lines = request_text.split("\r\n");
            let request_line = lines.next().unwrap_or_default().to_string();
            let mut parts = request_line.split(' ');
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let body = request_text
                .split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .to_string();
            *received.lock().unwrap() = Some((method, path, body));

            let status_line = match status {
                200 => "200 OK",
                _ => "500 Internal Server Error",
            };
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        format!("http://127.0.0.1:{port}/webhook")
    }

    #[tokio::test]
    async fn notify_posts_a_slack_compatible_webhook_body() {
        let ctx = RunContext::new();
        let received = Arc::new(Mutex::new(None));
        let url = mock_webhook_server(200, received.clone()).await;

        let notify_action = action_with_inputs(
            "notify",
            vec![
                ("url", Value::from(url)),
                ("channel", Value::from("ops")),
                ("message", Value::from("hi")),
            ],
        );
        let outputs = execute_notify(&notify_action, &ctx).await.unwrap();
        assert_eq!(outputs.get("status"), Some(&Value::from(200)));

        let (method, path, body) = received.lock().unwrap().clone().unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/webhook");
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.get("channel"), Some(&Value::from("ops")));
        assert_eq!(parsed.get("text"), Some(&Value::from("hi")));
    }

    #[tokio::test]
    async fn notify_omits_channel_from_the_body_when_not_set() {
        let ctx = RunContext::new();
        let received = Arc::new(Mutex::new(None));
        let url = mock_webhook_server(200, received.clone()).await;

        let notify_action = action_with_inputs(
            "notify",
            vec![("url", Value::from(url)), ("message", Value::from("hi"))],
        );
        execute_notify(&notify_action, &ctx).await.unwrap();

        let (_, _, body) = received.lock().unwrap().clone().unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.get("channel").is_none());
    }

    #[tokio::test]
    async fn notify_fails_on_a_non_2xx_webhook_response() {
        let ctx = RunContext::new();
        let received = Arc::new(Mutex::new(None));
        let url = mock_webhook_server(500, received.clone()).await;

        let notify_action = action_with_inputs(
            "notify",
            vec![("url", Value::from(url)), ("message", Value::from("hi"))],
        );
        let err = execute_notify(&notify_action, &ctx).await.unwrap_err();
        assert!(err.contains("500"), "{err}");
    }

    #[tokio::test]
    async fn log_message_can_be_a_jsonlogic_expression_referencing_upstream_outputs() {
        // Exactly the `.forge/order-processing.flow.json` pattern that
        // exposed this: `message: {"var": "tryCharge.outputs.error.message"}`
        // instead of a plain string.
        let ctx = RunContext::new();
        ctx.record_outputs(
            "tryCharge",
            serde_json::json!({"error": {"message": "boom"}})
                .as_object()
                .unwrap()
                .clone(),
        );
        let log_action = action_with_inputs(
            "log",
            vec![
                ("level", Value::from("error")),
                (
                    "message",
                    serde_json::json!({"var": "tryCharge.outputs.error.message"}),
                ),
            ],
        );
        // The output carries the evaluated text, not the expression.
        let outputs = execute_log(&log_action, &ctx).await.unwrap();
        assert_eq!(outputs.get("message"), Some(&Value::from("boom")));
    }

    #[tokio::test]
    async fn notify_channel_and_send_email_recipient_fields_also_accept_expressions() {
        let ctx = RunContext::new();
        ctx.record_outputs(
            "fetchCustomer",
            serde_json::json!({"email": "a@b.com", "tier": "ops"})
                .as_object()
                .unwrap()
                .clone(),
        );

        let received = Arc::new(Mutex::new(None));
        let url = mock_webhook_server(200, received.clone()).await;
        let notify_action = action_with_inputs(
            "notify",
            vec![
                ("url", Value::from(url)),
                (
                    "channel",
                    serde_json::json!({"var": "fetchCustomer.outputs.tier"}),
                ),
                ("message", Value::from("hi")),
            ],
        );
        execute_notify(&notify_action, &ctx).await.unwrap();
        let (_, _, body) = received.lock().unwrap().clone().unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.get("channel"), Some(&Value::from("ops")));

        // FUTURE FEATURE — restore with `execute_send_email`.
        //
        // let email_action = action_with_inputs(
        //     "sendEmail",
        //     vec![
        //         (
        //             "to",
        //             serde_json::json!({"var": "fetchCustomer.outputs.email"}),
        //         ),
        //         ("subject", Value::from("subj")),
        //         ("body", Value::from("body")),
        //     ],
        // );
        // assert!(
        //     execute_send_email(&email_action, &ctx)
        //         .await
        //         .unwrap()
        //         .is_empty()
        // );
    }

    #[tokio::test]
    async fn execute_leaf_reports_unimplemented_and_unknown_types_clearly() {
        let ctx = RunContext::new();
        let root = std::path::Path::new(".");

        let script_action = Action::new("script"); // no "source" input
        let err = execute_leaf("s1", &script_action, &ctx, root)
            .await
            .unwrap_err();
        assert!(err.contains("script"));

        // FUTURE FEATURE — `DbMigration` is parked, so it is reported as an
        // unknown type like any other unregistered id, not as "not
        // implemented". Restore the stricter assertion with its dispatch arm.
        let db_action = Action::new("DbMigration");
        let err = execute_leaf("d1", &db_action, &ctx, root)
            .await
            .unwrap_err();
        assert!(err.contains("unknown action type"));
        // assert!(err.contains("not implemented"));

        let unknown = Action::new("TotallyMadeUp");
        let err = execute_leaf("u1", &unknown, &ctx, root).await.unwrap_err();
        assert!(err.contains("unknown action type"));
    }

    fn python_available() -> bool {
        std::process::Command::new("python")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Mirrors the availability probe the production path uses, so a skipped
    /// test means "no bash on this machine", not "the resolver is broken".
    fn bash_available() -> bool {
        resolve_bash().is_ok()
    }

    #[test]
    fn missing_bash_error_names_the_install() {
        assert!(
            BASH_NOT_FOUND.contains("bash not found"),
            "the error must say bash is missing: {BASH_NOT_FOUND}"
        );
        if cfg!(windows) {
            assert!(
                BASH_NOT_FOUND.contains("Git Bash"),
                "on Windows the error should point at Git Bash: {BASH_NOT_FOUND}"
            );
        }
    }

    #[test]
    fn bash_args_pass_the_script_path_to_the_resolved_interpreter() {
        let (program, args) = match build_command_args(ScriptRuntime::Bash, Path::new("calc.sh")) {
            Ok(resolved) => resolved,
            Err(error) => {
                eprintln!("skipping: {error}");
                return;
            }
        };
        assert_eq!(args, vec!["calc.sh".to_string()]);
        assert!(
            !program.is_empty(),
            "the interpreter must be resolved to a real program"
        );
    }

    #[test]
    fn extension_inference_covers_known_and_unknown_extensions() {
        assert_eq!(
            ScriptRuntime::from_path(Path::new("x.py")),
            Some(ScriptRuntime::Python)
        );
        assert_eq!(
            ScriptRuntime::from_path(Path::new("x.js")),
            Some(ScriptRuntime::Node)
        );
        assert_eq!(
            ScriptRuntime::from_path(Path::new("x.ts")),
            Some(ScriptRuntime::Node)
        );
        assert_eq!(
            ScriptRuntime::from_path(Path::new("x.ps1")),
            Some(ScriptRuntime::Powershell)
        );
        assert_eq!(
            ScriptRuntime::from_path(Path::new("x.csproj")),
            Some(ScriptRuntime::Dotnet)
        );
        // Both spellings of a shell script infer `bash`; companions are
        // written as `.sh` (see `designer_panel::script_companion`), while a
        // hand-written `.bash` file has to infer the same runtime.
        assert_eq!(
            ScriptRuntime::from_path(Path::new("x.sh")),
            Some(ScriptRuntime::Bash)
        );
        assert_eq!(
            ScriptRuntime::from_path(Path::new("x.bash")),
            Some(ScriptRuntime::Bash)
        );
        assert_eq!(ScriptRuntime::from_path(Path::new("x.exe")), None);
    }

    #[test]
    fn bash_spills_inline_code_to_a_sh_temp_file() {
        assert_eq!(ScriptRuntime::Bash.extension(), "sh");
    }

    #[tokio::test]
    async fn script_inline_dotnet_is_rejected_without_spawning_anything() {
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "inline", "runtime": "dotnet", "code": "// n/a"}),
        );
        let err = execute_script(&action, &RunContext::new(), Path::new(".")).await.unwrap_err();
        assert!(err.contains("dotnet"));
    }

    #[tokio::test]
    async fn script_file_mode_requires_runtime_when_extension_is_unrecognized() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("thing.xyz"), "whatever").unwrap();
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "file", "path": "thing.xyz"}),
        );
        let err = execute_script(&action, &RunContext::new(), dir.path()).await.unwrap_err();
        assert!(err.contains("infer"));
    }

    #[tokio::test]
    async fn script_inline_python_round_trips_stdin_args_to_outputs() {
        if !python_available() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let code = "import json, sys\ndata = json.loads(sys.stdin.read() or '{}')\nprint(json.dumps({'doubled': data.get('n', 0) * 2}))\n";
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "inline", "runtime": "python", "code": code}),
        );
        action
            .inputs
            .insert("args".to_string(), serde_json::json!({"n": 21}));

        let outputs = execute_script(&action, &RunContext::new(), Path::new(".")).await.unwrap();
        assert_eq!(outputs.get("doubled"), Some(&Value::from(42)));
    }

    /// A script is handed the run's variables, loop item and earlier outputs
    /// through the context file, without the flow passing any of them.
    #[tokio::test]
    async fn script_reads_the_run_context_from_its_context_file() {
        if !python_available() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let ctx = RunContext::new();
        ctx.init_variable("ids", VariableType::Array, serde_json::json!([1, 2]))
            .unwrap();
        ctx.record_outputs(
            "fetch",
            serde_json::json!({"status": 200})
                .as_object()
                .unwrap()
                .clone(),
        );
        let ctx = ctx.with_item(serde_json::json!({"id": 7}));

        let code = "import json, os\n\
                    context = json.load(open(os.environ['FORGE_CONTEXT'], encoding='utf-8'))\n\
                    print(json.dumps({\n\
                    \x20   'ids': context['variables']['ids'],\n\
                    \x20   'item': context['item']['id'],\n\
                    \x20   'status': context['actions']['fetch']['status'],\n\
                    }))\n";
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "inline", "runtime": "python", "code": code}),
        );

        let outputs = execute_script(&action, &ctx, Path::new(".")).await.unwrap();
        assert_eq!(outputs.get("ids"), Some(&serde_json::json!([1, 2])));
        assert_eq!(outputs.get("item"), Some(&Value::from(7)));
        assert_eq!(outputs.get("status"), Some(&Value::from(200)));
    }

    /// `argsExpression` is how live values reach a script: a variable, the
    /// loop item, an earlier result.
    #[test]
    fn script_stdin_is_the_literal_args_or_the_evaluated_expression() {
        let ctx = RunContext::new();
        ctx.init_variable(
            "ids",
            VariableType::Array,
            serde_json::json!([1, 2]),
        )
        .unwrap();
        let ctx = ctx.with_item(serde_json::json!({"id": 7}));

        let action = |inputs: Vec<(&str, Value)>| action_with_inputs("script", inputs);
        assert_eq!(script_stdin(&action(vec![]), &ctx), Ok(None));
        // `args` is sent as written, even when it looks like an expression.
        assert_eq!(
            script_stdin(
                &action(vec![("args", serde_json::json!({"var": "variables.ids"}))]),
                &ctx
            ),
            Ok(Some(serde_json::json!({"var": "variables.ids"})))
        );
        assert_eq!(
            script_stdin(
                &action(vec![(
                    "argsExpression",
                    serde_json::json!({"var": "variables.ids"})
                )]),
                &ctx
            ),
            Ok(Some(serde_json::json!([1, 2])))
        );
        // Every variable at once, and the loop item.
        assert_eq!(
            script_stdin(
                &action(vec![("argsExpression", serde_json::json!({"var": "variables"}))]),
                &ctx
            ),
            Ok(Some(serde_json::json!({"ids": [1, 2]})))
        );
        assert_eq!(
            script_stdin(
                &action(vec![("argsExpression", serde_json::json!({"var": "$item.id"}))]),
                &ctx
            ),
            Ok(Some(Value::from(7)))
        );
        let both = action(vec![
            ("args", Value::from(1)),
            ("argsExpression", serde_json::json!({"var": "$item"})),
        ]);
        assert!(script_stdin(&both, &ctx).unwrap_err().contains("not both"));
    }

    #[tokio::test]
    async fn script_nonzero_exit_is_failed() {
        if !python_available() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "inline", "runtime": "python", "code": "import sys\nsys.exit(3)\n"}),
        );
        let err = execute_script(&action, &RunContext::new(), Path::new(".")).await.unwrap_err();
        assert!(err.contains("exited with"));
    }

    #[tokio::test]
    async fn script_last_stdout_line_must_be_a_json_object() {
        if !python_available() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "inline", "runtime": "python", "code": "print('not json')\n"}),
        );
        let err = execute_script(&action, &RunContext::new(), Path::new(".")).await.unwrap_err();
        assert!(err.contains("JSON"));
    }

    #[tokio::test]
    async fn script_file_mode_resolves_against_solution_root_and_infers_runtime() {
        if !python_available() {
            eprintln!("skipping: python not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("calc.py"), "print('{\"ok\": true}')\n").unwrap();
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "file", "path": "calc.py"}),
        );

        let outputs = execute_script(&action, &RunContext::new(), dir.path()).await.unwrap();
        assert_eq!(outputs.get("ok"), Some(&Value::Bool(true)));
    }

    #[tokio::test]
    async fn script_inline_bash_round_trips_stdin_args_to_outputs() {
        if !bash_available() {
            eprintln!("skipping: bash not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Reads the piped `args` JSON the same way the Python test does, using
        // only POSIX string operations so it runs under any `bash` (Git Bash
        // included) without needing jq.
        std::fs::write(
            dir.path().join("calc.sh"),
            "read -r payload\necho \"{\\\"doubled\\\": $(( $(echo \"$payload\" | sed 's/[^0-9]//g') * 2 ))}\"\n",
        )
        .unwrap();
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "file", "path": "calc.sh"}),
        );
        action
            .inputs
            .insert("args".to_string(), serde_json::json!({"n": 21}));

        let outputs = execute_script(&action, &RunContext::new(), dir.path()).await.unwrap();
        assert_eq!(outputs.get("doubled"), Some(&Value::from(42)));
    }

    #[tokio::test]
    async fn script_file_mode_honors_a_bash_runtime_override_for_an_unknown_extension() {
        if !bash_available() {
            eprintln!("skipping: bash not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("runnable"), "echo '{\"ok\": true}'\n").unwrap();
        let mut action = Action::new("script");
        action.inputs.insert(
            "source".to_string(),
            serde_json::json!({"kind": "file", "path": "runnable", "runtime": "bash"}),
        );

        let outputs = execute_script(&action, &RunContext::new(), dir.path()).await.unwrap();
        assert_eq!(outputs.get("ok"), Some(&Value::Bool(true)));
    }
}
