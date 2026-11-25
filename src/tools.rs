use crate::config::{Context, Outcome, lossy_text, truncate_middle};
use crate::sandbox::{self, RunOutput, Sandbox};
use crate::verify::{self, Pass, Verifier};
use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const ALLOWED_PROGRAMS: &[&str] = &[
    "cc", "clang", "gcc", "c++", "clang++", "g++", "as", "ld", "ar", "rustc", "python3", "otool", "objdump",
    "nm", "size", "strip", "file", "diff", "cmp", "cat", "ls", "wc", "head", "tail", "od", "xxd", "sort",
    "uniq", "grep", "true",
];

const RESERVED_DIRECTORY: &str = "verifier";
const MAX_WRITE_BYTES: usize = 4 << 20;
const MAX_READ_CHARACTERS: usize = 120_000;
const MAX_BUILD_STEPS: usize = 20;
const MAX_ARGUMENTS: usize = 512;

pub struct ToolContext<'a> {
    pub sandbox: &'a Sandbox,
    pub verifier: &'a Verifier,
    pub timeout: Duration,
}

pub enum ToolOutcome {
    Text(String),
    Passed(Pass),
}

fn tool_definition(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let schema = json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false});
    json!({"name": name, "description": description, "eager_input_streaming": true, "input_schema": schema})
}

pub fn definitions() -> Value {
    let string = json!({"type": "string"});
    let command = json!({"type": "array", "items": {"type": "string"}});
    let run_description = format!(
        "Run one program in the sandboxed workspace, without a shell. command[0] is one of [{}] or a ./relative path to a file in the workspace. Arguments may not be absolute paths or contain '..'. Network is off.",
        ALLOWED_PROGRAMS.join(", ")
    );
    let submit_description = "Build your compiler, compile input.<ext> with it, and verify the executable against the reference on the given tests plus every tests/*.in. Returns PASS with timings, or the failures.";
    let build_property = json!({"type": "array", "items": command, "description": "commands run in order to build the compiler"});
    let compile_property = json!({"type": "array", "items": string, "description": "command invoking your compiler; must contain {in} and {out}"});
    let run_properties = json!({"command": command, "stdin": string, "timeout_seconds": {"type": "integer"}});
    json!([
        tool_definition("write_file", "Create or overwrite a file. path is relative to the workspace root.", json!({"path": string, "content": string}), &["path", "content"]),
        tool_definition("read_file", "Read a text file from the workspace.", json!({"path": string}), &["path"]),
        tool_definition("list_files", "List workspace files recursively with sizes.", json!({}), &[]),
        tool_definition("run", &run_description, run_properties, &["command"]),
        tool_definition("submit", submit_description, json!({"build": build_property, "compile": compile_property}), &["build", "compile"]),
    ])
}

pub fn execute(context: &ToolContext, name: &str, input: &Value) -> Outcome<ToolOutcome> {
    match name {
        "write_file" => write_file(context, input).map(ToolOutcome::Text),
        "read_file" => read_file(context, input).map(ToolOutcome::Text),
        "list_files" => Ok(ToolOutcome::Text(list_files(&context.sandbox.workspace))),
        "run" => run_tool(context, input).map(ToolOutcome::Text),
        "submit" => submit_tool(context, input).map(ToolOutcome::Passed),
        _ => Err(format!("unknown tool {name}")),
    }
}

fn submit_tool(context: &ToolContext, input: &Value) -> Outcome<Pass> {
    let steps = input["build"].as_array().ok_or("build must be an array of commands")?;
    if steps.len() > MAX_BUILD_STEPS {
        return Err(format!("at most {MAX_BUILD_STEPS} build steps"));
    }
    let build = steps.iter().map(parse_command).collect::<Outcome<Vec<_>>>()?;
    let compile = parse_command(&input["compile"])?;
    verify::submit(context.sandbox, context.verifier, &build, &compile, context.timeout)
}

fn string_field<'a>(input: &'a Value, field: &str) -> Outcome<&'a str> {
    input[field].as_str().ok_or(format!("missing string field '{field}'"))
}

pub fn parse_command(value: &Value) -> Outcome<Vec<String>> {
    let entries = value.as_array().ok_or("command must be an array of strings")?;
    if entries.is_empty() || entries.len() > MAX_ARGUMENTS {
        return Err(format!("command must have 1..{MAX_ARGUMENTS} entries"));
    }
    let to_string = |entry: &Value| entry.as_str().map(String::from).ok_or("command entries must be strings".to_string());
    entries.iter().map(to_string).collect()
}

fn relative_path(path: &str) -> Outcome<&Path> {
    let relative = Path::new(path);
    let plain = relative.components().all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if path.is_empty() || !plain {
        return Err(format!("path must be relative and inside the workspace: {path}"));
    }
    Ok(relative)
}

pub fn ensure_inside_workspace(workspace: &Path, full_path: &Path) -> Outcome<()> {
    let mut existing = full_path;
    while fs::symlink_metadata(existing).is_err() {
        existing = existing.parent().ok_or("bad path")?;
    }
    let resolved = existing.canonicalize().context("resolve path")?;
    if resolved.starts_with(workspace) {
        return Ok(());
    }
    Err("path escapes the workspace".into())
}

pub fn safe_write(workspace: &Path, path: &str, data: &[u8]) -> Outcome<PathBuf> {
    let full_path = workspace.join(relative_path(path)?);
    ensure_inside_workspace(workspace, &full_path)?;
    if fs::symlink_metadata(&full_path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(format!("{path} exists and is not a regular file"));
    }
    let parent = full_path.parent().ok_or("bad path")?;
    fs::create_dir_all(parent).context("create directory")?;
    ensure_inside_workspace(workspace, parent)?;
    fs::write(&full_path, data).context("write")?;
    Ok(full_path)
}

fn is_reserved(path: &Path) -> bool {
    let first = path.components().find(|component| matches!(component, Component::Normal(_)));
    first.is_some_and(|component| component.as_os_str() == RESERVED_DIRECTORY)
}

fn write_file(context: &ToolContext, input: &Value) -> Outcome<String> {
    let (path, content) = (string_field(input, "path")?, string_field(input, "content")?);
    if is_reserved(relative_path(path)?) {
        return Err(format!("{RESERVED_DIRECTORY}/ is reserved for the verifier"));
    }
    if content.len() > MAX_WRITE_BYTES {
        return Err(format!("file too large (max {MAX_WRITE_BYTES} bytes)"));
    }
    safe_write(&context.sandbox.workspace, path, content.as_bytes())?;
    Ok(format!("wrote {path} ({} bytes)", content.len()))
}

fn read_file(context: &ToolContext, input: &Value) -> Outcome<String> {
    let path = string_field(input, "path")?;
    let full_path = context.sandbox.workspace.join(relative_path(path)?);
    ensure_inside_workspace(&context.sandbox.workspace, &full_path)?;
    let data = fs::read(&full_path).context(path)?;
    if data.contains(&0) {
        return Ok(format!("{path}: binary file, {} bytes; inspect it with run (od, otool, objdump)", data.len()));
    }
    Ok(truncate_middle(&lossy_text(&data), MAX_READ_CHARACTERS))
}

fn list_files(workspace: &Path) -> String {
    let mut pending = vec![PathBuf::new()];
    let mut lines = Vec::new();
    while let Some(relative) = pending.pop() {
        let Ok(entries) = fs::read_dir(workspace.join(&relative)) else { continue };
        for entry in entries.flatten() {
            let path = relative.join(entry.file_name());
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_dir() && path != Path::new("tmp") {
                pending.push(path.clone());
            }
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let suffix = if file_type.is_dir() { "/" } else { "" };
            lines.push(format!("{}{suffix} {size}", path.display()));
        }
    }
    lines.sort();
    lines.truncate(600);
    lines.join("\n")
}

fn check_argument(argument: &str) -> Outcome<()> {
    let absolute = argument.split(['=', ',', ':']).any(|part| part.starts_with('/') || part.starts_with('~'));
    let parent = argument.split(['/', '=', ',', ':']).any(|part| part == "..");
    let is_flag_prefix = |slash: usize| argument[..slash].trim_start_matches('-').chars().all(|character| character.is_ascii_alphanumeric());
    let joined_flag_path = argument.starts_with('-') && argument.find('/').is_some_and(is_flag_prefix);
    if absolute || parent || joined_flag_path {
        return Err(format!("argument not allowed (absolute or parent path): {argument}"));
    }
    Ok(())
}

pub fn checked_command(workspace: &Path, command: &[String]) -> Outcome<Vec<String>> {
    let (program, arguments) = command.split_first().ok_or("command is empty")?;
    for argument in arguments {
        check_argument(argument)?;
    }
    let program_path = if program.contains('/') { local_program(workspace, program)? } else { allowed_program(program)? };
    Ok(std::iter::once(program_path).chain(arguments.iter().cloned()).collect())
}

fn local_program(workspace: &Path, program: &str) -> Outcome<String> {
    let full_path = workspace.join(relative_path(program)?);
    ensure_inside_workspace(workspace, &full_path)?;
    let resolved = full_path.canonicalize().context(program)?;
    if !resolved.is_file() {
        return Err(format!("{program} is not a file"));
    }
    Ok(resolved.display().to_string())
}

fn allowed_program(program: &str) -> Outcome<String> {
    if !ALLOWED_PROGRAMS.contains(&program) {
        return Err(format!("{program} is not allowed; allowed: {} or ./path", ALLOWED_PROGRAMS.join(", ")));
    }
    let found = sandbox::which(program).map(|path| path.display().to_string());
    found.ok_or(format!("{program} not found on PATH"))
}

fn run_tool(context: &ToolContext, input: &Value) -> Outcome<String> {
    let command = checked_command(&context.sandbox.workspace, &parse_command(&input["command"])?)?;
    let stdin = input["stdin"].as_str().unwrap_or("").as_bytes();
    let requested = input["timeout_seconds"].as_u64().map(Duration::from_secs);
    let timeout = requested.unwrap_or(context.timeout).min(context.timeout * 4);
    Ok(format_output(&sandbox::run(context.sandbox, &command, stdin, timeout)?))
}

pub fn format_output(output: &RunOutput) -> String {
    let status = match (output.timed_out, output.exit_code, output.signal) {
        (true, _, _) => "TIMEOUT (killed)".to_string(),
        (_, Some(code), _) => format!("exit {code}"),
        (_, _, Some(signal)) => format!("killed by signal {signal}"),
        _ => "unknown status".to_string(),
    };
    let stdout = truncate_middle(&lossy_text(&output.stdout), 12000);
    let stderr = truncate_middle(&lossy_text(&output.stderr), 12000);
    let milliseconds = output.duration.as_millis();
    format!("{status} in {milliseconds}ms\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
}
