use crate::config::{Context, Outcome};
use std::env;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CAPTURE_LIMIT: usize = 1 << 20;
const FILE_SIZE_LIMIT_BLOCKS: u64 = 4 * 1024 * 1024;

enum Mode {
    MacSandbox(String),
    Bubblewrap(Vec<String>),
    Disabled,
}

pub struct Sandbox {
    pub workspace: PathBuf,
    mode: Mode,
    environment: Vec<(String, String)>,
}

pub struct RunOutput {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub duration: Duration,
}

impl RunOutput {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

pub fn create(workspace: &Path, disabled: bool) -> Outcome<Sandbox> {
    let home = env::var("HOME").unwrap_or_default();
    let home = if home.is_empty() { "/nonexistent-home".to_string() } else { home };
    let mode = if disabled { Mode::Disabled } else { choose_mode(workspace, &home)? };
    let environment = child_environment(workspace, &home);
    Ok(Sandbox { workspace: workspace.into(), mode, environment })
}

fn choose_mode(workspace: &Path, home: &str) -> Outcome<Mode> {
    let workspace_text = workspace.to_str().ok_or("workspace path is not utf-8")?;
    let unsafe_characters = ['"', '\\', '\n'];
    if [workspace_text, home].iter().any(|path| path.contains(unsafe_characters)) {
        return Err("workspace or home path has characters unsafe for a sandbox profile".into());
    }
    if cfg!(target_os = "macos") {
        return Ok(Mode::MacSandbox(mac_profile(workspace_text, home)));
    }
    if which("bwrap").is_some() {
        return Ok(Mode::Bubblewrap(bubblewrap_arguments(workspace_text, home)));
    }
    Err("no sandbox available: install bubblewrap (bwrap), or pass --no-sandbox".into())
}

fn mac_profile(workspace: &str, home: &str) -> String {
    format!(
        r##"(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write* (subpath "{workspace}") (literal "/dev/null") (literal "/dev/zero") (literal "/dev/dtracehelper") (regex #"^/dev/fd/") (regex #"^/dev/tty"))
(deny file-read* (subpath "{home}"))
(allow file-read* (literal "{home}") (subpath "{home}/.rustup") (subpath "{home}/.cargo/bin"))
(allow file-read* (subpath "{workspace}"))
"##
    )
}

fn bubblewrap_arguments(workspace: &str, home: &str) -> Vec<String> {
    let base = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", home];
    let mut arguments: Vec<String> = base.map(String::from).to_vec();
    for toolchain_directory in [".rustup", ".cargo/bin"] {
        let path = format!("{home}/{toolchain_directory}");
        arguments.extend(["--ro-bind-try".to_string(), path.clone(), path]);
    }
    let isolation = ["--bind", workspace, workspace, "--unshare-all", "--die-with-parent", "--new-session"];
    arguments.extend(isolation.map(String::from));
    arguments.extend(["--chdir", workspace, "--"].map(String::from));
    arguments
}

fn child_environment(workspace: &Path, home: &str) -> Vec<(String, String)> {
    let workspace_text = workspace.display().to_string();
    vec![
        ("PATH".to_string(), reachable_path()),
        ("HOME".to_string(), workspace_text.clone()),
        ("TMPDIR".to_string(), format!("{workspace_text}/tmp")),
        ("LC_ALL".to_string(), "C".to_string()),
        ("RUSTUP_HOME".to_string(), format!("{home}/.rustup")),
        ("CARGO_HOME".to_string(), format!("{home}/.cargo")),
    ]
}

fn reachable_path() -> String {
    let home = env::var("HOME").unwrap_or_default();
    let cargo_bin = format!("{home}/.cargo/bin");
    let path = env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    let outside_home = |directory: &&str| home.is_empty() || !Path::new(directory).starts_with(&home);
    let reachable = |directory: &&str| directory.starts_with('/') && (outside_home(directory) || **directory == cargo_bin);
    path.split(':').filter(reachable).collect::<Vec<_>>().join(":")
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = reachable_path();
    let candidates = path.split(':').map(|directory| Path::new(directory).join(name));
    candidates.into_iter().find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn build_command(sandbox: &Sandbox, command_line: &[String], timeout: Duration) -> Command {
    let cpu_seconds = timeout.as_secs() + 5;
    let limits = format!("ulimit -t {cpu_seconds}; ulimit -f {FILE_SIZE_LIMIT_BLOCKS}; exec \"$@\"");
    let mut command = launcher(&sandbox.mode);
    command.arg("-c").arg(limits).arg("sh").args(command_line);
    command
}

fn launcher(mode: &Mode) -> Command {
    let shell = "/bin/sh".to_string();
    let (program, arguments): (&str, Vec<String>) = match mode {
        Mode::MacSandbox(profile) => ("/usr/bin/sandbox-exec", vec!["-p".into(), profile.clone(), shell]),
        Mode::Bubblewrap(options) => ("bwrap", [options.clone(), vec![shell]].concat()),
        Mode::Disabled => ("/bin/sh", vec![]),
    };
    let mut command = Command::new(program);
    command.args(arguments);
    command
}

pub fn run(sandbox: &Sandbox, command_line: &[String], input: &[u8], timeout: Duration) -> Outcome<RunOutput> {
    if command_line.is_empty() {
        return Err("empty command".into());
    }
    let mut command = build_command(sandbox, command_line, timeout);
    command.current_dir(&sandbox.workspace).env_clear().envs(sandbox.environment.iter().cloned());
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let started = Instant::now();
    let mut child = command.spawn().context("spawn")?;
    let stdin_pipe = child.stdin.take();
    let input_bytes = input.to_vec();
    let writer = thread::spawn(move || stdin_pipe.map(|mut pipe| pipe.write_all(&input_bytes)));
    let stdout_reader = spawn_reader(child.stdout.take());
    let stderr_reader = spawn_reader(child.stderr.take());
    let (status, timed_out) = wait_with_timeout(&mut child, timeout);
    let duration = started.elapsed();
    kill_process_group(child.id());
    let _ = writer.join();
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let exit_code = status.and_then(|status| status.code());
    let signal = status.and_then(|status| status.signal());
    Ok(RunOutput { exit_code, signal, stdout, stderr, timed_out, duration })
}

fn spawn_reader<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let Some(mut pipe) = pipe else { return captured };
        let mut buffer = [0u8; 16384];
        while let Ok(count) = pipe.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let kept = count.min(CAPTURE_LIMIT.saturating_sub(captured.len()));
            captured.extend_from_slice(&buffer[..kept]);
        }
        captured
    })
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> (Option<ExitStatus>, bool) {
    let started = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return (Some(status), false);
        }
        if started.elapsed() > timeout {
            kill_process_group(child.id());
            let _ = child.kill();
            return (child.wait().ok(), true);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn kill_process_group(process_id: u32) {
    let group = format!("-{process_id}");
    let mut command = Command::new("kill");
    command.args(["-s", "KILL", "--", &group]).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = command.status();
}
