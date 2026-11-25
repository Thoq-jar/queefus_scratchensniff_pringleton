use crate::config::{Config, Context, Outcome, lossy_text, truncate_middle};
use crate::sandbox::{self, RunOutput, Sandbox};
use crate::tools;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_TESTS: usize = 64;
const MAX_TEST_BYTES: u64 = 4 << 20;
const MAX_SOURCE_BYTES: usize = 400_000;
const REPEATS: usize = 3;

pub struct TestCase {
    pub name: String,
    pub input: Vec<u8>,
    pub expected: Option<Vec<u8>>,
}

pub struct Verifier {
    pub extension: String,
    pub source: Vec<u8>,
    pub reference_binary: Option<PathBuf>,
    pub reference_description: String,
    pub user_tests: Vec<TestCase>,
    overhead_milliseconds: f64,
}

pub struct Pass {
    pub total_milliseconds: f64,
    pub reference_milliseconds: Option<f64>,
    pub test_count: usize,
    pub report: String,
}

struct GradedCase {
    name: String,
    input: Vec<u8>,
    expected_stdout: Vec<u8>,
    expected_exit_code: Option<i32>,
    reference_milliseconds: Option<f64>,
}

struct Timing {
    output: RunOutput,
    best_milliseconds: f64,
    consistent: bool,
}

pub fn sibling_path(workspace: &Path, suffix: &str) -> PathBuf {
    let name = workspace.file_name().and_then(|name| name.to_str()).unwrap_or("workspace");
    workspace.with_file_name(format!("{name}.{suffix}"))
}

pub fn initialise(config: &Config, sandbox: &Sandbox) -> Outcome<Verifier> {
    let source = fs::read(&config.source).context("read source")?;
    if source.len() > MAX_SOURCE_BYTES {
        return Err("source over 400KB; this tool targets single programs".into());
    }
    let extension = config.source.extension().and_then(|extension| extension.to_str()).unwrap_or("txt").to_string();
    let workspace = &sandbox.workspace;
    for directory in ["tmp", "tests", "verifier", "given"] {
        fs::create_dir_all(workspace.join(directory)).context("create directory")?;
    }
    tools::safe_write(workspace, &format!("input.{extension}"), &source)?;
    let user_tests = match &config.tests_directory {
        Some(directory) => load_tests(directory, true)?,
        None => vec![],
    };
    for test in &user_tests {
        tools::safe_write(workspace, &format!("given/{}.in", test.name), &test.input)?;
        tools::safe_write(workspace, &format!("given/{}.out", test.name), test.expected.as_deref().unwrap_or(b""))?;
    }
    let (reference_binary, reference_description) = build_reference(config, sandbox, &extension)?;
    if reference_binary.is_none() && user_tests.iter().all(|test| test.expected.is_none()) {
        return Err("no oracle: pass --tests DIR with NAME.in/NAME.out pairs, or --ref 'command {in} {out}'".into());
    }
    let overhead_milliseconds = measure_overhead(sandbox);
    Ok(Verifier { extension, source, reference_binary, reference_description, user_tests, overhead_milliseconds })
}

fn load_tests(directory: &Path, from_user: bool) -> Outcome<Vec<TestCase>> {
    let mut tests = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return if from_user { Err(format!("cannot read tests directory {}", directory.display())) } else { Ok(tests) };
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.retain(|path| path.extension().is_some_and(|extension| extension == "in"));
    paths.sort();
    for path in paths.into_iter().take(MAX_TESTS) {
        let Some(input) = read_small_file(&path) else { continue };
        let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("test").to_string();
        let expected = if from_user { read_small_file(&path.with_extension("out")) } else { None };
        tests.push(TestCase { name, input, expected });
    }
    Ok(tests)
}

fn read_small_file(path: &Path) -> Option<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_TEST_BYTES {
        return None;
    }
    fs::read(path).ok()
}

fn default_reference_commands(extension: &str) -> Vec<Vec<String>> {
    let native = if cfg!(target_arch = "aarch64") { "-mcpu=native" } else { "-march=native" };
    let owned = |words: &[&str]| words.iter().map(|word| word.to_string()).collect::<Vec<_>>();
    match extension {
        "c" => vec![
            owned(&["cc", "-O3", native, "{in}", "-o", "{out}", "-lm"]),
            owned(&["cc", "-O3", "{in}", "-o", "{out}", "-lm"]),
        ],
        "cc" | "cpp" | "cxx" => vec![
            owned(&["c++", "-O3", "-std=c++20", native, "{in}", "-o", "{out}"]),
            owned(&["c++", "-O3", "-std=c++20", "{in}", "-o", "{out}"]),
        ],
        "rs" => vec![
            owned(&["rustc", "-C", "opt-level=3", "-C", "target-cpu=native", "{in}", "-o", "{out}"]),
            owned(&["rustc", "-O", "{in}", "-o", "{out}"]),
        ],
        _ => vec![],
    }
}

fn build_reference(config: &Config, sandbox: &Sandbox, extension: &str) -> Outcome<(Option<PathBuf>, String)> {
    let templates = match &config.reference_command {
        Some(command) => vec![command.split_whitespace().map(String::from).collect()],
        None => default_reference_commands(extension),
    };
    if templates.is_empty() {
        return Ok((None, "none (expected outputs come from --tests)".into()));
    }
    let (input_path, output_path) = (format!("input.{extension}"), "tmp/reference_program".to_string());
    let mut last_failure = String::new();
    for template in &templates {
        let substitute = |word: &String| word.replace("{in}", &input_path).replace("{out}", &output_path);
        let command: Vec<String> = template.iter().map(substitute).collect();
        let output = sandbox::run(sandbox, &command, b"", Duration::from_secs(600))?;
        if output.succeeded() && sandbox.workspace.join(&output_path).is_file() {
            return Ok((Some(protect_reference(&sandbox.workspace, &output_path)?), template.join(" ")));
        }
        last_failure = tools::format_output(&output);
    }
    Err(format!("reference build failed; fix the source or pass --ref/--tests\n{last_failure}"))
}

fn protect_reference(workspace: &Path, built_path: &str) -> Outcome<PathBuf> {
    let directory = sibling_path(workspace, "ref");
    fs::create_dir_all(&directory).context("create reference directory")?;
    let protected = directory.join("program");
    fs::copy(workspace.join(built_path), &protected).context("copy reference")?;
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o755)).context("chmod reference")?;
    Ok(protected)
}

fn measure_overhead(sandbox: &Sandbox) -> f64 {
    let command = vec!["true".to_string()];
    let runs = (0..5).filter_map(|_| sandbox::run(sandbox, &command, b"", Duration::from_secs(10)).ok());
    let fastest = runs.map(|output| output.duration.as_secs_f64() * 1e3).fold(f64::INFINITY, f64::min);
    fastest.min(1e3)
}

fn time_runs(sandbox: &Sandbox, program: &Path, input: &[u8], timeout: Duration, overhead: f64) -> Outcome<Timing> {
    let command = vec![program.display().to_string()];
    let output = sandbox::run(sandbox, &command, input, timeout)?;
    let mut best_milliseconds = output.duration.as_secs_f64() * 1e3;
    let mut consistent = true;
    for _ in 1..REPEATS {
        if output.timed_out || output.signal.is_some() {
            break;
        }
        let repeat = sandbox::run(sandbox, &command, input, timeout)?;
        consistent &= repeat.stdout == output.stdout && repeat.exit_code == output.exit_code;
        best_milliseconds = best_milliseconds.min(repeat.duration.as_secs_f64() * 1e3);
    }
    let best_milliseconds = (best_milliseconds - overhead).max(0.0);
    Ok(Timing { output, best_milliseconds, consistent })
}

fn prepare_case(sandbox: &Sandbox, verifier: &Verifier, test: TestCase, timeout: Duration) -> Outcome<Result<GradedCase, String>> {
    let overhead = verifier.overhead_milliseconds;
    let reference = match &verifier.reference_binary {
        Some(binary) => Some(time_runs(sandbox, binary, &test.input, timeout, overhead)?),
        None => None,
    };
    let TestCase { name, input, expected } = test;
    if let Some(expected_stdout) = expected {
        let reference_milliseconds = reference.map(|timing| timing.best_milliseconds);
        let case = GradedCase { name, input, expected_stdout, expected_exit_code: None, reference_milliseconds };
        return Ok(Ok(case));
    }
    let Some(reference) = reference else { return Ok(Err(format!("{name}: skipped, no expected output"))) };
    let unreliable = reference.output.timed_out || reference.output.signal.is_some() || !reference.consistent;
    if unreliable {
        return Ok(Err(format!("{name}: skipped, reference timed out, crashed or was nondeterministic")));
    }
    let expected_exit_code = reference.output.exit_code;
    let reference_milliseconds = Some(reference.best_milliseconds);
    let expected_stdout = reference.output.stdout;
    Ok(Ok(GradedCase { name, input, expected_stdout, expected_exit_code, reference_milliseconds }))
}

fn collect_cases(sandbox: &Sandbox, verifier: &Verifier, timeout: Duration) -> Outcome<(Vec<GradedCase>, Vec<String>)> {
    let copy = |test: &TestCase| TestCase { name: test.name.clone(), input: test.input.clone(), expected: test.expected.clone() };
    let mut tests: Vec<TestCase> = verifier.user_tests.iter().map(copy).collect();
    let agent_tests = load_tests(&sandbox.workspace.join("tests"), false)?;
    tests.extend(agent_tests.into_iter().map(|test| TestCase { name: format!("tests/{}", test.name), ..test }));
    if tests.is_empty() {
        tests.push(TestCase { name: "empty-stdin".into(), input: vec![], expected: None });
    }
    let (mut cases, mut notes) = (Vec::new(), Vec::new());
    for test in tests {
        match prepare_case(sandbox, verifier, test, timeout)? {
            Ok(case) => cases.push(case),
            Err(note) => notes.push(note),
        }
    }
    Ok((cases, notes))
}

fn ensure_native_executable(path: &Path) -> Outcome<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "compiler did not produce {out}".to_string())?;
    let data = if metadata.is_file() { fs::read(path).context("read output")? } else { vec![] };
    let magic_numbers: &[&[u8]] = &[&[0xcf, 0xfa, 0xed, 0xfe], &[0xca, 0xfe, 0xba, 0xbe], &[0x7f, b'E', b'L', b'F']];
    if !magic_numbers.iter().any(|magic| data.starts_with(magic)) {
        return Err("{out} is not a native Mach-O/ELF executable".into());
    }
    Ok(())
}

fn first_differing_line(expected: &[u8], actual: &[u8]) -> usize {
    let (expected, actual) = (lossy_text(expected), lossy_text(actual));
    let shorter = expected.lines().count().min(actual.lines().count());
    let differing = expected.lines().zip(actual.lines()).position(|(left, right)| left != right);
    differing.unwrap_or(shorter) + 1
}

fn describe_failure(case: &GradedCase, timing: &Timing) -> Option<String> {
    let header = format!("test {} (stdin: {})", case.name, truncate_middle(&lossy_text(&case.input), 300));
    let output = &timing.output;
    if output.timed_out {
        return Some(format!("{header}\n  timed out"));
    }
    if let Some(signal) = output.signal {
        let stderr = truncate_middle(&lossy_text(&output.stderr), 1500);
        return Some(format!("{header}\n  killed by signal {signal}\n  stderr: {stderr}"));
    }
    if output.stdout != case.expected_stdout {
        let line = first_differing_line(&case.expected_stdout, &output.stdout);
        let expected = truncate_middle(&lossy_text(&case.expected_stdout), 1500);
        let actual = truncate_middle(&lossy_text(&output.stdout), 1500);
        return Some(format!("{header}\n  stdout differs from line {line}\n  expected:\n{expected}\n  got:\n{actual}"));
    }
    if case.expected_exit_code.is_some() && case.expected_exit_code != output.exit_code {
        return Some(format!("{header}\n  exit code {:?}, expected {:?}", output.exit_code, case.expected_exit_code));
    }
    if !timing.consistent {
        return Some(format!("{header}\n  output differs between repeated runs"));
    }
    None
}

pub fn submit(sandbox: &Sandbox, verifier: &Verifier, build: &[Vec<String>], compile: &[String], timeout: Duration) -> Outcome<Pass> {
    for step in build {
        let output = sandbox::run(sandbox, &tools::checked_command(&sandbox.workspace, step)?, b"", timeout * 4)?;
        if !output.succeeded() {
            return Err(format!("BUILD FAILED: {}\n{}", step.join(" "), tools::format_output(&output)));
        }
    }
    let has_input = compile.iter().any(|word| word.contains("{in}"));
    let has_output = compile.iter().any(|word| word.contains("{out}"));
    if !has_input || !has_output {
        return Err("compile command must contain {in} and {out}".into());
    }
    let input_path = format!("verifier/input.{}", verifier.extension);
    tools::safe_write(&sandbox.workspace, &input_path, &verifier.source)?;
    let program = sandbox.workspace.join("verifier/program");
    let _ = fs::remove_file(&program);
    let substitute = |word: &String| word.replace("{in}", &input_path).replace("{out}", "verifier/program");
    let command: Vec<String> = compile.iter().map(substitute).collect();
    let output = sandbox::run(sandbox, &tools::checked_command(&sandbox.workspace, &command)?, b"", timeout * 4)?;
    if !output.succeeded() {
        return Err(format!("COMPILE FAILED: {}\n{}", command.join(" "), tools::format_output(&output)));
    }
    ensure_native_executable(&program)?;
    let copied_reference = verifier.reference_binary.as_ref().is_some_and(|reference| fs::read(reference).ok() == fs::read(&program).ok());
    if copied_reference {
        return Err("output is a copy of the reference binary".into());
    }
    grade(sandbox, verifier, &program, timeout)
}

fn grade(sandbox: &Sandbox, verifier: &Verifier, program: &Path, timeout: Duration) -> Outcome<Pass> {
    let (cases, notes) = collect_cases(sandbox, verifier, timeout)?;
    if cases.is_empty() {
        return Err(format!("no usable tests\n{}", notes.join("\n")));
    }
    let mut failures = Vec::new();
    let (mut total_milliseconds, mut reference_total, mut reference_complete) = (0.0, 0.0, true);
    for case in &cases {
        let timing = time_runs(sandbox, program, &case.input, timeout, verifier.overhead_milliseconds)?;
        if let Some(failure) = describe_failure(case, &timing) {
            failures.push(failure);
            continue;
        }
        total_milliseconds += timing.best_milliseconds;
        reference_total += case.reference_milliseconds.unwrap_or(0.0);
        reference_complete &= case.reference_milliseconds.is_some();
    }
    let notes = if notes.is_empty() { String::new() } else { format!("\nnotes:\n{}", notes.join("\n")) };
    if !failures.is_empty() {
        let shown = failures.iter().take(3).cloned().collect::<Vec<_>>().join("\n\n");
        return Err(format!("FAIL {}/{} tests\n\n{shown}{notes}", failures.len(), cases.len()));
    }
    let reference_milliseconds = reference_complete.then_some(reference_total);
    let speedup = |reference: f64| format!(", reference {reference:.2}ms, speedup {:.2}x", reference / total_milliseconds.max(0.001));
    let comparison = reference_milliseconds.map(speedup).unwrap_or_default();
    let report = format!("PASS {} tests. total min run time: yours {total_milliseconds:.2}ms{comparison}{notes}", cases.len());
    Ok(Pass { total_milliseconds, reference_milliseconds, test_count: cases.len(), report })
}
