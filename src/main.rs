#![forbid(unsafe_code)]

mod api;
mod config;
mod prompts;
mod sandbox;
mod tools;
mod verify;

use config::{Config, Context, Outcome, truncate_middle};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const SNAPSHOT_SKIP: &[&str] = &["tmp", "verifier"];
const SNAPSHOT_MARKER: &str = ".pringleton";
const MAX_SNAPSHOT_FILE_BYTES: u64 = 64 << 20;

struct BestBuild {
    total_milliseconds: f64,
    reference_milliseconds: Option<f64>,
    test_count: usize,
}

struct LoopState {
    best: Option<BestBuild>,
    optimisation_left: usize,
    finished: bool,
    tokens: [u64; 4],
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nerror: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Outcome<()> {
    let config = config::parse_arguments()?;
    let api_key = config::api_key()?;
    fs::create_dir_all(&config.workspace).context("create workspace")?;
    let workspace = config.workspace.canonicalize().context("workspace")?;
    if config.no_sandbox {
        eprintln!("WARNING: --no-sandbox: model-written code will run with your full user permissions");
    }
    let sandbox = sandbox::create(&workspace, config.no_sandbox)?;
    eprintln!("workspace: {}", workspace.display());
    let verifier = verify::initialise(&config, &sandbox)?;
    eprintln!("reference: {}", verifier.reference_description);
    let client = api::new_client(api_key, &config.model, &config.effort, !config.no_fallback)?;
    let best = agent_loop(&config, &client, &sandbox, &verifier)?;
    print_summary(&config, &workspace, best)
}

fn agent_loop(config: &Config, client: &api::Client, sandbox: &sandbox::Sandbox, verifier: &verify::Verifier) -> Outcome<Option<BestBuild>> {
    let system_text = prompts::system_prompt(&verifier.extension);
    let system = json!([{"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}]);
    let tool_definitions = tools::definitions();
    let context = tools::ToolContext { sandbox, verifier, timeout: config.timeout };
    let task = prompts::task_prompt(config, verifier, &config::machine_description());
    let mut messages = vec![json!({"role": "user", "content": task})];
    let mut state = LoopState { best: None, optimisation_left: config.optimisation_rounds, finished: false, tokens: [0; 4] };
    for turn in 0..config.max_turns {
        eprintln!("\n\x1b[1m== turn {}/{} ({}) ==\x1b[0m", turn + 1, config.max_turns, client.model);
        let response = api::call(client, &system, &tool_definitions, &messages)?;
        record_usage(&mut state, &response);
        if response.stop_reason == "refusal" {
            return Err(format!("model declined the request (refusal): {}", response.stop_details));
        }
        let content = api::strip_before_fallback(response.content);
        if content.is_empty() {
            return Err(format!("empty response (stop_reason {})", response.stop_reason));
        }
        let tool_calls: Vec<Value> = content.iter().filter(|block| block["type"] == "tool_use").cloned().collect();
        messages.push(json!({"role": "assistant", "content": content}));
        let invalid = &response.invalid_tool_ids;
        let results = handle_turn(&context, &mut state, &tool_calls, invalid, &response.stop_reason, config)?;
        save_transcript(&sandbox.workspace, &messages);
        if state.finished {
            break;
        }
        messages.push(json!({"role": "user", "content": results}));
    }
    let [input, cache_write, cache_read, output] = state.tokens;
    eprintln!("\ntokens: input {input}, cache write {cache_write}, cache read {cache_read}, output {output}");
    Ok(state.best)
}

fn record_usage(state: &mut LoopState, response: &api::Response) {
    let keys = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"];
    for (index, key) in keys.iter().enumerate() {
        state.tokens[index] += response.usage[key].as_u64().unwrap_or(0);
    }
    let output_tokens = &response.usage["output_tokens"];
    eprintln!("\n[{} | stop {} | output {output_tokens}]", response.served_model, response.stop_reason);
}

fn handle_turn(
    context: &tools::ToolContext,
    state: &mut LoopState,
    tool_calls: &[Value],
    invalid_tool_ids: &[String],
    stop_reason: &str,
    config: &Config,
) -> Outcome<Vec<Value>> {
    if tool_calls.is_empty() && state.best.is_some() {
        state.finished = true;
        return Ok(vec![]);
    }
    if tool_calls.is_empty() {
        let message = if stop_reason == "max_tokens" { prompts::OUTPUT_LIMIT } else { prompts::NUDGE };
        return Ok(vec![json!({"type": "text", "text": message})]);
    }
    let mut results = Vec::new();
    for tool_call in tool_calls {
        results.push(handle_tool_call(context, state, tool_call, invalid_tool_ids, config)?);
    }
    Ok(results)
}

fn describe_call(name: &str, input: &Value) -> String {
    let join = |value: &Value| value.as_array().map(|words| words.iter().filter_map(|word| word.as_str()).collect::<Vec<_>>().join(" "));
    let content_length = input["content"].as_str().map_or(0, |content| content.len());
    let description = match name {
        "run" => join(&input["command"]),
        "submit" => join(&input["compile"]),
        "write_file" => input["path"].as_str().map(|path| format!("{path} ({content_length} bytes)")),
        _ => input["path"].as_str().map(String::from),
    };
    description.unwrap_or_default()
}

fn handle_tool_call(
    context: &tools::ToolContext,
    state: &mut LoopState,
    tool_call: &Value,
    invalid_tool_ids: &[String],
    config: &Config,
) -> Outcome<Value> {
    let id = tool_call["id"].as_str().unwrap_or("");
    let name = tool_call["name"].as_str().unwrap_or("");
    let input = &tool_call["input"];
    eprintln!("\n\x1b[36m-> {name}\x1b[0m {}", truncate_middle(&describe_call(name, input), 200));
    let invalid = invalid_tool_ids.iter().any(|invalid_id| invalid_id == id);
    let outcome = if invalid { Err(prompts::INVALID_INPUT.to_string()) } else { tools::execute(context, name, input) };
    let (text, is_error) = match outcome {
        Ok(tools::ToolOutcome::Text(text)) => (text, false),
        Ok(tools::ToolOutcome::Passed(pass)) => (record_pass(context, state, pass, config)?, false),
        Err(error) => (error, true),
    };
    let first_line = text.lines().next().unwrap_or("");
    let marker = if is_error { "\x1b[31merror\x1b[0m " } else { "" };
    eprintln!("   {marker}{}", truncate_middle(first_line, 200));
    let content = truncate_middle(&text, 30000);
    Ok(json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}))
}

fn record_pass(context: &tools::ToolContext, state: &mut LoopState, pass: verify::Pass, config: &Config) -> Outcome<String> {
    let improved = state.best.as_ref().is_none_or(|best| pass.total_milliseconds < best.total_milliseconds);
    if improved {
        keep_build(&context.sandbox.workspace, &config.output)?;
        let (total_milliseconds, reference_milliseconds, test_count) = (pass.total_milliseconds, pass.reference_milliseconds, pass.test_count);
        state.best = Some(BestBuild { total_milliseconds, reference_milliseconds, test_count });
    }
    let kept = if improved { "This build is the new best and was saved." } else { "Slower than the best so far; the best is kept." };
    if state.optimisation_left == 0 {
        state.finished = true;
        return Ok(format!("{}\n{kept}", pass.report));
    }
    let remaining = state.optimisation_left;
    state.optimisation_left -= 1;
    Ok(format!(
        "{}\n{kept}\nYou may submit up to {remaining} more times to make it faster; the best passing build is kept. To finish, reply without calling a tool.",
        pass.report
    ))
}

fn keep_build(workspace: &Path, output: &Path) -> Outcome<()> {
    let program = workspace.join("verifier/program");
    if !fs::symlink_metadata(&program).is_ok_and(|metadata| metadata.is_file()) {
        return Err("verifier/program vanished".into());
    }
    if fs::symlink_metadata(output).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(format!("{} exists and is not a regular file", output.display()));
    }
    fs::copy(&program, output).context("copy output")?;
    fs::set_permissions(output, fs::Permissions::from_mode(0o755)).context("chmod output")?;
    let snapshot = verify::sibling_path(workspace, "best");
    if snapshot.join(SNAPSHOT_MARKER).is_file() {
        fs::remove_dir_all(&snapshot).context("clear old snapshot")?;
    }
    if snapshot.exists() {
        return Err(format!("{} exists and was not made by pringleton", snapshot.display()));
    }
    copy_tree(workspace, &snapshot)?;
    fs::write(snapshot.join(SNAPSHOT_MARKER), "").context("write snapshot marker")
}

fn copy_tree(source: &Path, destination: &Path) -> Outcome<()> {
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        fs::create_dir_all(destination.join(&relative)).context("create directory")?;
        for entry in fs::read_dir(source.join(&relative)).context("read directory")?.flatten() {
            let at_root = relative.as_os_str().is_empty();
            if at_root && SNAPSHOT_SKIP.iter().any(|skipped| entry.file_name() == *skipped) {
                continue;
            }
            let path = relative.join(entry.file_name());
            let file_type = entry.file_type().context("file type")?;
            let too_big = entry.metadata().map(|metadata| metadata.len() > MAX_SNAPSHOT_FILE_BYTES).unwrap_or(true);
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && !too_big {
                fs::copy(source.join(&path), destination.join(&path)).context("copy")?;
            }
        }
    }
    Ok(())
}

fn save_transcript(workspace: &Path, messages: &[Value]) {
    let path = verify::sibling_path(workspace, "transcript.json");
    let _ = fs::write(path, serde_json::to_string_pretty(messages).unwrap_or_default());
}

fn print_summary(config: &Config, workspace: &Path, best: Option<BestBuild>) -> Outcome<()> {
    let Some(best) = best else {
        return Err(format!("no passing build within {} turns; workspace kept at {}", config.max_turns, workspace.display()));
    };
    let ratio = |reference: f64| format!(" vs reference {reference:.2}ms ({:.2}x)", reference / best.total_milliseconds.max(0.001));
    let comparison = best.reference_milliseconds.map(ratio).unwrap_or_default();
    eprintln!("\n\x1b[32mPASS\x1b[0m {} tests, {:.2}ms{comparison}", best.test_count, best.total_milliseconds);
    eprintln!("generated compiler: {}", verify::sibling_path(workspace, "best").display());
    eprintln!("transcript: {}", verify::sibling_path(workspace, "transcript.json").display());
    println!("{}", config.output.display());
    Ok(())
}
