use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Outcome<T> = Result<T, String>;

pub trait Context<T> {
    fn context(self, message: &str) -> Outcome<T>;
}

impl<T, E: std::fmt::Display> Context<T> for Result<T, E> {
    fn context(self, message: &str) -> Outcome<T> {
        self.map_err(|error| format!("{message}: {error}"))
    }
}

pub struct Config {
    pub source: PathBuf,
    pub output: PathBuf,
    pub tests_directory: Option<PathBuf>,
    pub reference_command: Option<String>,
    pub model: String,
    pub effort: String,
    pub max_turns: usize,
    pub optimisation_rounds: usize,
    pub timeout: Duration,
    pub workspace: PathBuf,
    pub no_sandbox: bool,
    pub no_fallback: bool,
}

pub const HELP: &str = "usage: pringleton <source> [options]
  -o <path>            output executable (default: ./<stem>)
  --tests <dir>        stdin tests: NAME.in with optional NAME.out
  --ref '<command>'    reference build, e.g. 'cc -O3 {in} -o {out}'
  --model <id>         default claude-fable-5-2
  --effort <level>     low|medium|high|xhigh|max (default high)
  --turns <n>          max agent turns (default 40)
  --opt <n>            extra optimisation submits after first pass (default 3)
  --timeout <seconds>  per-run timeout (default 60)
  --work <dir>         workspace dir (default: $TMPDIR/pringleton/<stem>-<timestamp>)
  --no-sandbox         run generated code unsandboxed (unsafe)
  --no-fallback        disable server-side refusal fallbacks";

const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const KEY_NAMES: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_COLON_THREE"];

fn next_value(arguments: &[String], index: &mut usize) -> Outcome<String> {
    *index += 1;
    let flag = &arguments[*index - 1];
    arguments.get(*index).cloned().ok_or(format!("{flag} needs a value"))
}

fn next_number(arguments: &[String], index: &mut usize) -> Outcome<usize> {
    let text = next_value(arguments, index)?;
    text.parse().context(&format!("bad number '{text}'"))
}

pub fn parse_arguments() -> Outcome<Config> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let (mut source, mut output, mut tests_directory) = (None, None, None);
    let (mut reference_command, mut workspace) = (None, None);
    let (mut model, mut effort) = ("claude-fable-5-2".to_string(), "high".to_string());
    let (mut max_turns, mut optimisation_rounds, mut timeout_seconds) = (40, 3, 60);
    let (mut no_sandbox, mut no_fallback) = (false, false);
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "-h" | "--help" => return Err(HELP.into()),
            "-o" => output = Some(PathBuf::from(next_value(&arguments, &mut index)?)),
            "--tests" => tests_directory = Some(PathBuf::from(next_value(&arguments, &mut index)?)),
            "--ref" => reference_command = Some(next_value(&arguments, &mut index)?),
            "--model" => model = next_value(&arguments, &mut index)?,
            "--effort" => effort = next_value(&arguments, &mut index)?,
            "--turns" => max_turns = next_number(&arguments, &mut index)?,
            "--opt" => optimisation_rounds = next_number(&arguments, &mut index)?,
            "--timeout" => timeout_seconds = next_number(&arguments, &mut index)?,
            "--work" => workspace = Some(PathBuf::from(next_value(&arguments, &mut index)?)),
            "--no-sandbox" => no_sandbox = true,
            "--no-fallback" => no_fallback = true,
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag}\n{HELP}")),
            path => source = Some(PathBuf::from(path)),
        }
        index += 1;
    }
    let source = source.ok_or(HELP)?;
    if !EFFORT_LEVELS.contains(&effort.as_str()) {
        return Err(format!("--effort must be one of {}", EFFORT_LEVELS.join("|")));
    }
    let stem = source.file_stem().and_then(|stem| stem.to_str()).unwrap_or("program").to_string();
    let output = output.unwrap_or_else(|| PathBuf::from(&stem));
    let canonical_source = source.canonicalize().ok();
    if output.canonicalize().ok().is_some_and(|canonical_output| canonical_source == Some(canonical_output)) {
        return Err("output path would overwrite the source".into());
    }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0);
    let default_workspace = env::temp_dir().join("pringleton").join(format!("{stem}-{timestamp}"));
    let workspace = workspace.unwrap_or(default_workspace);
    let no_fallback = no_fallback || model.contains("haiku");
    let timeout = Duration::from_secs(timeout_seconds.max(1) as u64);
    Ok(Config {
        source,
        output,
        tests_directory,
        reference_command,
        model,
        effort,
        max_turns,
        optimisation_rounds,
        timeout,
        workspace,
        no_sandbox,
        no_fallback,
    })
}

pub fn api_key() -> Outcome<String> {
    for name in KEY_NAMES {
        let value = env::var(name).unwrap_or_default();
        if !value.trim().is_empty() {
            return Ok(value.trim().into());
        }
    }
    let dotenv = fs::read_to_string(".env").unwrap_or_default();
    for line in dotenv.lines() {
        let Some((name, value)) = line.split_once('=') else { continue };
        let name = name.trim().trim_start_matches("export ").trim();
        if KEY_NAMES.contains(&name) {
            return Ok(value.trim().trim_matches('"').trim_matches('\'').into());
        }
    }
    Err("no ANTHROPIC_API_KEY in the environment or ./.env".into())
}

pub fn machine_description() -> String {
    let mut description = format!("arch={} os={}\n", env::consts::ARCH, env::consts::OS);
    let commands: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["uname", "-a"], &["sysctl", "machdep.cpu.brand_string", "hw"]]
    } else {
        &[&["uname", "-a"], &["lscpu"]]
    };
    for command in commands {
        let Ok(result) = Command::new(command[0]).args(&command[1..]).output() else { continue };
        description += &lossy_text(&result.stdout);
    }
    truncate_middle(&description, 9000)
}

pub fn lossy_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn truncate_middle(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let head_end = floor_boundary(text, limit / 2);
    let tail_start = ceil_boundary(text, text.len() - limit / 2);
    let removed = tail_start - head_end;
    format!("{}\n...[{removed} bytes cut]...\n{}", &text[..head_end], &text[tail_start..])
}

fn floor_boundary(text: &str, mut position: usize) -> usize {
    while !text.is_char_boundary(position) {
        position -= 1;
    }
    position
}

fn ceil_boundary(text: &str, mut position: usize) -> usize {
    while !text.is_char_boundary(position) {
        position += 1;
    }
    position
}
