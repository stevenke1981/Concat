// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Pictures from the person's ChatGPT subscription, made by OpenAI's own
//! Codex command line.
//!
//! Concat never sees the person's OpenAI sign-in. The Codex CLI keeps it: the
//! person installs `@openai/codex` and runs `codex login`, choosing sign-in
//! with ChatGPT, and each request here is one non-interactive `codex exec`
//! run that asks Codex's built-in image skill for a picture. That picture is
//! counted against the subscription. A sign-in with an API key also works,
//! but [`Status::ApiKey`] says it bills the API instead, so the page can say
//! so before the person spends anything.
//!
//! The instruction goes in on stdin, not on the command line: on Windows the
//! npm install is a `.cmd` shim, and a prompt's quotes and percent signs would
//! be read by `cmd.exe` as shell syntax. Each run works in a scratch folder of
//! its own, and the pictures are looked for in two places when it ends: that
//! folder, where the instruction asks Codex to save them, and the folder Codex
//! keeps everything it draws in, `generated_images` under its home. Whatever is
//! new in either is copied into the caller's `out_dir`, and the scratch folder
//! is removed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use super::{MediaKind, Output, Progress, Request, random_hex, sniff};

/// The model the Codex CLI's image skill renders with.
const IMAGE_MODEL: &str = "gpt-image-2";

/// What to say when there is no Codex CLI to run.
const NOT_INSTALLED: &str = "ChatGPT pictures need the Codex CLI. Install it with \
     `npm i -g @openai/codex`, then run `codex login` and sign in with ChatGPT.";

/// How long `--version` and `login status` may take.
const STATUS_LIMIT: Duration = Duration::from_secs(20);

/// How long one picture may take before the run is given up.
const DRAW_LIMIT: Duration = Duration::from_secs(15 * 60);

/// How often a running Codex is checked for exit and cancellation.
const POLL: Duration = Duration::from_millis(250);

/// How many lines of Codex's output an error message keeps.
const TAIL: usize = 40;

/// How many characters of one output line are kept.
const MAX_LINE: usize = 400;

/// How deep the picture folders are searched.
const MAX_DEPTH: u32 = 4;

/// Where the Codex CLI is and where Codex keeps its files. The defaults find
/// the program on `PATH` and read `CODEX_HOME` the way Codex itself does.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The Codex program. `None` finds `codex` on `PATH`.
    pub program: Option<PathBuf>,
    /// Codex's home folder. `None` is `$CODEX_HOME`, or `~/.codex`.
    pub codex_home: Option<PathBuf>,
}

/// What the Codex CLI is ready for.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// No usable Codex CLI: it is not installed, or `--version` did not run.
    Missing,
    /// Installed, but not signed in: `codex login` is needed.
    LoggedOut {
        /// The version number, such as `0.140.0`.
        version: String,
    },
    /// Signed in with an API key. Pictures work, but they bill the API, not
    /// the ChatGPT plan.
    ApiKey {
        /// The version number, such as `0.140.0`.
        version: String,
    },
    /// Signed in with ChatGPT, so pictures count against the subscription.
    Ready {
        /// The version number, such as `0.140.0`.
        version: String,
    },
}

/// The Codex program this process would run: `config.program` when it names
/// a file, otherwise the first `codex` on `PATH`. On Windows the names tried
/// follow `PATHEXT`, so the native `codex.exe` wins over the npm `codex.cmd`
/// shim, and the extensionless npm script is never picked, since Windows
/// cannot run it.
pub fn find_program(config: &Config) -> Option<PathBuf> {
    if let Some(program) = &config.program {
        return program.is_file().then(|| program.clone());
    }
    let path = std::env::var_os("PATH")?;
    let names = program_names();
    std::env::split_paths(&path).find_map(|dir| {
        names
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Whether the Codex CLI is installed and signed in, and with what. Runs
/// `codex --version` and `codex login status`, each for at most 20 seconds, so
/// it blocks for up to 40 seconds: call it off the window's thread. Never
/// panics, and never keeps the sign-in itself: an API key's text is read for
/// its kind and then dropped.
pub fn status(config: &Config) -> Status {
    let Some(program) = find_program(config) else {
        return Status::Missing;
    };
    let Some(version) = run_capture(&program, config, &["--version"], STATUS_LIMIT) else {
        return Status::Missing;
    };
    if !version.success {
        return Status::Missing;
    }
    let version = version_of(&version.text).unwrap_or_else(|| "unknown".to_string());
    let signed = run_capture(&program, config, &["login", "status"], STATUS_LIMIT)
        .map_or(SignIn::Out, |answer| sign_in(answer.success, &answer.text));
    match signed {
        SignIn::Out => Status::LoggedOut { version },
        SignIn::ApiKey => Status::ApiKey { version },
        SignIn::ChatGpt => Status::Ready { version },
    }
}

/// The folder Codex writes the pictures it makes into: `generated_images`
/// under its home. `None` when no home can be found.
pub fn generated_dir(config: &Config) -> Option<PathBuf> {
    codex_home(config).map(|home| home.join("generated_images"))
}

/// Makes one picture from the request's prompt, and copies the new pictures
/// into `out_dir`.
///
/// Only pictures are made here: a video request is refused, and so is an
/// empty prompt. The run takes at most 15 minutes, and it stops when `cancel`
/// is set, with `Err("cancelled")`; the Codex process and whatever it started
/// are killed. `progress` gets one `Running` report once Codex is under way.
/// A reference picture, when the request has one, is copied into the scratch
/// folder and attached with `-i`.
///
/// Errors are human-readable. When Codex runs but makes no picture, the error
/// carries the end of its output, with any line that mentions a token or an
/// `sk-` key left out.
pub fn generate(
    config: &Config,
    request: &Request,
    out_dir: &Path,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Vec<Output>, String> {
    if request.kind != MediaKind::Image {
        return Err("ChatGPT makes pictures only; choose OpenRouter or Flow for a video.".into());
    }
    if request.prompt.trim().is_empty() {
        return Err("Write what to draw first.".into());
    }
    let program = find_program(config).ok_or_else(|| NOT_INSTALLED.to_string())?;

    fs::create_dir_all(out_dir)
        .map_err(|error| format!("could not make {}: {error}", out_dir.display()))?;
    let work = out_dir.join(format!("{}-work", random_hex(8)));
    fs::create_dir_all(&work)
        .map_err(|error| format!("could not make {}: {error}", work.display()))?;
    // Removed however this run ends, after the pictures are copied out.
    let _scratch = Scratch(work.clone());

    if let Some(reference) = &request.reference {
        fs::copy(reference, work.join(reference_file_name(reference))).map_err(|error| {
            format!(
                "could not read the reference picture {}: {error}",
                reference.display()
            )
        })?;
    }

    // Taken after the reference is copied in, so it is never returned as output.
    let generated = generated_dir(config);
    let before_work = listing(&work);
    let before_generated = generated.as_deref().map(listing).unwrap_or_default();

    let mut child = command(&program, config)
        .args(exec_args(request, &work))
        .current_dir(&work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", program.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        let text = draw_instruction(request);
        // Written off the calling thread, so a long prompt cannot stall the
        // poll loop. Dropping the handle at the end is what ends the prompt.
        thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let stdout = tail_lines(child.stdout.take());
    let stderr = tail_lines(child.stderr.take());
    progress(Progress::Running {
        fraction: None,
        message: "ChatGPT is drawing…".to_string(),
    });

    let started = Instant::now();
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            kill_tree(&mut child);
            return Err("cancelled".to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                kill_tree(&mut child);
                return Err(format!("lost the Codex CLI while it was drawing: {error}"));
            }
        }
        if started.elapsed() >= DRAW_LIMIT {
            kill_tree(&mut child);
            return Err("ChatGPT did not finish the picture within 15 minutes.".to_string());
        }
        thread::sleep(POLL);
    };

    let mut lines = take_tail(&stdout);
    lines.extend(take_tail(&stderr));

    let mut outputs: Vec<Output> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (source, ext) in new_pictures(&work, generated.as_deref(), &before_work, &before_generated)
    {
        let Ok(bytes) = fs::read(&source) else {
            continue;
        };
        // Codex may keep a picture both in the folder it was told and in its
        // own; the same bytes are one picture.
        if !seen.insert(Sha256::digest(&bytes).to_vec()) {
            continue;
        }
        let path = out_dir.join(format!("{}.{ext}", random_hex(12)));
        if let Err(error) = fs::write(&path, &bytes) {
            for output in &outputs {
                let _ = fs::remove_file(&output.path);
            }
            return Err(format!("could not save the picture: {error}"));
        }
        outputs.push(Output {
            path,
            kind: MediaKind::Image,
            model: IMAGE_MODEL.to_string(),
        });
    }
    if !outputs.is_empty() {
        return Ok(outputs);
    }

    let said = explain(lines);
    let head = if status.success() {
        "Codex CLI finished but saved no picture".to_string()
    } else {
        format!("Codex CLI failed ({status})")
    };
    Err(if said.is_empty() {
        head
    } else {
        format!("{head}:\n{said}")
    })
}

/// What a sign-in check says.
#[derive(Debug, PartialEq)]
enum SignIn {
    /// Not signed in, or the check did not run.
    Out,
    /// Signed in with an API key.
    ApiKey,
    /// Signed in with ChatGPT.
    ChatGpt,
}

/// Reads `codex login status`: its exit code says whether there is a sign-in,
/// and its text says which kind. The text is not kept.
fn sign_in(success: bool, text: &str) -> SignIn {
    let lower = text.to_ascii_lowercase();
    if !success || lower.contains("not logged in") {
        SignIn::Out
    } else if lower.contains("api key") {
        SignIn::ApiKey
    } else {
        SignIn::ChatGpt
    }
}

/// The version number in `codex --version` output, such as `codex-cli 0.140.0`.
fn version_of(text: &str) -> Option<String> {
    text.lines()
        .filter(|line| line.to_ascii_lowercase().contains("codex"))
        .find_map(|line| {
            line.split_whitespace()
                .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
                .map(|word| {
                    word.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                        .to_string()
                })
        })
}

/// The `codex` file names to look for in each `PATH` folder, in `PATHEXT`
/// order. Windows runs only the executable extensions, so the npm script
/// without one is left out.
fn program_names() -> Vec<String> {
    #[cfg(windows)]
    {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        pathext
            .split(';')
            .map(str::trim)
            .filter(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    ".com" | ".exe" | ".bat" | ".cmd"
                )
            })
            .map(|ext| format!("codex{}", ext.to_ascii_lowercase()))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["codex".to_string()]
    }
}

/// Codex's home folder: the config's, else `$CODEX_HOME`, else `~/.codex`.
fn codex_home(config: &Config) -> Option<PathBuf> {
    if let Some(home) = &config.codex_home {
        return Some(home.clone());
    }
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|home| !home.is_empty()) {
        return Some(PathBuf::from(home));
    }
    std::env::home_dir().map(|home| home.join(".codex"))
}

/// The arguments of one `codex exec` run. The instruction is not among them:
/// it comes on stdin, which `-` asks `codex exec` to read. `-i` takes the
/// picture copied into the scratch folder by its file name, and `-C` is the
/// scratch folder itself.
fn exec_args(request: &Request, work: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "exec",
        "--skip-git-repo-check",
        "--color",
        "never",
        "--sandbox",
        "workspace-write",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(reference) = &request.reference {
        args.push("-i".into());
        args.push(reference_file_name(reference).into());
    }
    args.push("-C".into());
    args.push(work.as_os_str().to_owned());
    args.push("-".into());
    args
}

/// The instruction Codex is given: the built-in image skill, the prompt, the
/// hints the request carries, and where to save the result.
fn draw_instruction(request: &Request) -> String {
    let mut parts = vec![format!("$imagegen {}", request.prompt.trim())];
    let aspect = request.aspect_ratio.trim();
    if !aspect.is_empty() {
        parts.push(format!("Aspect ratio {aspect}."));
    }
    let count = request.count();
    if count > 1 {
        parts.push(format!("Make {count} separate images."));
    }
    parts.push(
        "Save every generated image into the current working directory as PNG files. \
         Do not ask questions; do not do anything else."
            .to_string(),
    );
    parts.join(" ")
}

/// The file name a reference picture is copied under in the scratch folder:
/// `reference` and its extension, lower-cased, or `png` when it has none that
/// is safe to pass on.
fn reference_file_name(reference: &Path) -> String {
    let ext = reference
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|ext| {
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "png".to_string());
    format!("reference.{ext}")
}

/// The pictures that are new since the snapshots: files in the scratch folder
/// and in Codex's `generated_images`, whose name is new or whose modification
/// time has changed. Each comes with the extension it is saved under.
fn new_pictures(
    work: &Path,
    generated: Option<&Path>,
    before_work: &HashMap<PathBuf, SystemTime>,
    before_generated: &HashMap<PathBuf, SystemTime>,
) -> Vec<(PathBuf, &'static str)> {
    let mut candidates = changed_since(work, before_work);
    if let Some(dir) = generated {
        candidates.extend(changed_since(dir, before_generated));
    }
    candidates.sort();
    candidates
        .into_iter()
        .filter_map(|path| picture_ext(&path).map(|ext| (path, ext)))
        .collect()
}

/// The files under `dir` that are new or changed against `before`.
fn changed_since(dir: &Path, before: &HashMap<PathBuf, SystemTime>) -> Vec<PathBuf> {
    files_under(dir)
        .into_iter()
        .filter(|(path, modified)| before.get(path) != Some(modified))
        .map(|(path, _)| path)
        .collect()
}

/// A snapshot of the files under `dir`: each path with its modification time.
fn listing(dir: &Path) -> HashMap<PathBuf, SystemTime> {
    files_under(dir).into_iter().collect()
}

/// Every file under `dir`, down to [`MAX_DEPTH`] folders, with its
/// modification time. Symbolic links are not followed.
fn files_under(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut files = Vec::new();
    collect_files(dir, 0, &mut files);
    files
}

fn collect_files(dir: &Path, depth: u32, files: &mut Vec<(PathBuf, SystemTime)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let path = entry.path();
        if meta.is_dir() {
            if depth < MAX_DEPTH {
                collect_files(&path, depth + 1, files);
            }
        } else if meta.is_file()
            && let Ok(modified) = meta.modified()
        {
            files.push((path, modified));
        }
    }
}

/// The extension a file is saved under if it is a picture: what its first
/// bytes say, or else a picture extension it carries. `None` for a video or
/// anything else.
fn picture_ext(path: &Path) -> Option<&'static str> {
    let mut head = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(16).read_to_end(&mut head))
        .ok()?;
    match sniff(&head) {
        Some((ext, MediaKind::Image)) => Some(ext),
        Some((_, MediaKind::Video)) => None,
        None => match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "png" => Some("png"),
            "jpg" | "jpeg" => Some("jpg"),
            "webp" => Some("webp"),
            _ => None,
        },
    }
}

/// Runs the Codex program with `args` and waits for it, for at most `limit`.
/// `None` when it could not start or ran out of time.
struct Captured {
    success: bool,
    text: String,
}

fn run_capture(
    program: &Path,
    config: &Config,
    args: &[&str],
    limit: Duration,
) -> Option<Captured> {
    let mut child = command(program, config)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = tail_lines(child.stdout.take());
    let stderr = tail_lines(child.stderr.take());
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut lines = take_tail(&stdout);
                lines.extend(take_tail(&stderr));
                return Some(Captured {
                    success: status.success(),
                    text: lines.join("\n"),
                });
            }
            Ok(None) if started.elapsed() < limit => thread::sleep(Duration::from_millis(50)),
            _ => {
                kill_tree(&mut child);
                return None;
            }
        }
    }
}

/// Reads a stream on its own thread, keeping its last [`TAIL`] lines. The
/// lines arrive on the returned channel once the stream ends.
fn tail_lines<R: Read + Send + 'static>(stream: Option<R>) -> Receiver<Vec<String>> {
    let (sender, receiver) = mpsc::channel();
    let Some(stream) = stream else {
        let _ = sender.send(Vec::new());
        return receiver;
    };
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut tail: VecDeque<String> = VecDeque::with_capacity(TAIL);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if tail.len() == TAIL {
                tail.pop_front();
            }
            let text = String::from_utf8_lossy(&line);
            tail.push_back(text.trim_end().chars().take(MAX_LINE).collect());
        }
        let _ = sender.send(Vec::from(tail));
    });
    receiver
}

/// The lines a [`tail_lines`] stream has kept. Waits briefly, so a stream
/// that a stray grandchild still holds open cannot stall the caller.
fn take_tail(receiver: &Receiver<Vec<String>>) -> Vec<String> {
    receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default()
}

/// The last [`TAIL`] lines, without any that mention a token or an `sk-` key.
fn explain(lines: Vec<String>) -> String {
    let kept: Vec<String> = lines
        .into_iter()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            !lower.contains("token") && !lower.contains("sk-")
        })
        .collect();
    let start = kept.len().saturating_sub(TAIL);
    kept[start..].join("\n").trim().to_string()
}

/// The command for one Codex run: the program, with `CODEX_HOME` set when the
/// config names one, in its own process group, and with no console window.
fn command(program: &Path, config: &Config) -> Command {
    let mut command = Command::new(program);
    if let Some(home) = &config.codex_home {
        command.env("CODEX_HOME", home);
    }
    own_group(&mut command);
    hide_window(&mut command);
    command
}

/// Ends a run: the process and everything it started. On Windows that is the
/// whole tree, since `codex.cmd` starts `node`, which starts the real program;
/// killing only the shell would leave the drawing running and billing.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/T", "/F", "/PID"])
            .arg(child.id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_window(&mut taskkill);
        let _ = taskkill.status();
    }
    #[cfg(unix)]
    {
        // SAFETY: `kill` only sends a signal. The negative id names the process
        // group, which `own_group` made this child the leader of, so the signal
        // reaches the child and anything it started. A group that has already
        // gone is an error return, which is ignored.
        let _ = unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The scratch folder of one run, removed when the run is over.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: Concat is a GUI program, and a console program it
    // starts would otherwise open a console window of its own.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

#[cfg(unix)]
fn own_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn picture_request(prompt: &str) -> Request {
        Request {
            kind: MediaKind::Image,
            prompt: prompt.to_string(),
            ..Request::default()
        }
    }

    /// Bytes that start with the PNG signature, which is all sniffing reads.
    fn png_bytes() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(b"fixture");
        bytes
    }

    /// A fake Codex program: on Windows a `.cmd` script, elsewhere a shell
    /// script made executable. `body` is its command lines.
    #[cfg(windows)]
    fn fake_codex(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-codex.cmd");
        fs::write(&path, format!("@echo off\r\n{body}\r\n")).expect("write fake");
        path
    }

    #[cfg(not(windows))]
    fn fake_codex(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-codex");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// A shell command line that copies `from` to `to`, quoted for the script.
    #[cfg(windows)]
    fn copy_line(from: &Path, to: &str) -> String {
        format!("copy /Y \"{}\" \"{to}\" >NUL", from.display())
    }

    #[cfg(not(windows))]
    fn copy_line(from: &Path, to: &str) -> String {
        format!("cp \"{}\" \"{to}\"", from.display())
    }

    #[cfg(windows)]
    const WAIT_LINE: &str = r#""%SystemRoot%\System32\ping.exe" -n 30 127.0.0.1 >NUL"#;
    #[cfg(not(windows))]
    const WAIT_LINE: &str = "sleep 30";

    #[test]
    fn exec_args_carry_the_flags_and_read_the_prompt_from_stdin() {
        let work = Path::new("/scratch/abc-work");
        let args: Vec<String> = exec_args(&picture_request("a cat"), work)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "exec");
        for flag in [
            "--skip-git-repo-check",
            "--color",
            "never",
            "--sandbox",
            "workspace-write",
        ] {
            assert!(
                args.iter().any(|arg| arg == flag),
                "missing {flag}: {args:?}"
            );
        }
        let at = args.iter().position(|arg| arg == "-C").expect("-C");
        assert_eq!(args[at + 1], work.to_string_lossy());
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(!args.iter().any(|arg| arg == "-i"));
        assert!(!args.iter().any(|arg| arg.contains("imagegen")));
    }

    #[test]
    fn a_reference_picture_is_attached_by_its_copied_name() {
        let mut request = picture_request("make it blue");
        request.reference = Some(PathBuf::from("/photos/Cat.JPEG"));
        let args: Vec<String> = exec_args(&request, Path::new("/w"))
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let at = args.iter().position(|arg| arg == "-i").expect("-i");
        assert_eq!(args[at + 1], "reference.jpeg");
        assert_eq!(args.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn reference_names_keep_only_a_safe_extension() {
        assert_eq!(reference_file_name(Path::new("a/B.PNG")), "reference.png");
        assert_eq!(reference_file_name(Path::new("a/noext")), "reference.png");
        assert_eq!(reference_file_name(Path::new("a/x.we&rd")), "reference.png");
    }

    #[test]
    fn the_instruction_starts_with_the_skill_and_carries_the_hints() {
        let mut request = picture_request("  a red kite  ");
        request.aspect_ratio = "16:9".to_string();
        request.count = 3;
        let text = draw_instruction(&request);
        assert!(
            text.starts_with("$imagegen a red kite Aspect ratio 16:9. "),
            "{text}"
        );
        assert!(text.contains("Make 3 separate images."), "{text}");
        assert!(
            text.contains("current working directory as PNG files"),
            "{text}"
        );
        assert!(text.ends_with("do not do anything else."), "{text}");
    }

    #[test]
    fn the_instruction_without_hints_is_the_skill_and_the_save_line() {
        let text = draw_instruction(&picture_request("a cat"));
        assert!(
            text.starts_with("$imagegen a cat Save every generated image"),
            "{text}"
        );
        assert!(!text.contains("Aspect ratio"), "{text}");
        assert!(!text.contains("separate images"), "{text}");
    }

    #[test]
    fn sign_in_reads_the_kind_from_the_text() {
        assert_eq!(sign_in(true, "Logged in using ChatGPT"), SignIn::ChatGpt);
        assert_eq!(
            sign_in(true, "Logged in using an API key - sk-test-123"),
            SignIn::ApiKey
        );
        assert_eq!(sign_in(false, "Not logged in"), SignIn::Out);
        assert_eq!(sign_in(true, "Not logged in"), SignIn::Out);
    }

    #[test]
    fn version_is_the_number_after_the_name() {
        assert_eq!(
            version_of("codex-cli 0.140.0\n").as_deref(),
            Some("0.140.0")
        );
        assert_eq!(
            version_of("\ncodex-cli 0.163.0\r\n").as_deref(),
            Some("0.163.0")
        );
        assert_eq!(version_of("something else"), None);
    }

    #[test]
    fn only_new_pictures_are_collected() {
        let root = temp();
        let work = root.path().join("work");
        let generated = root.path().join("generated_images");
        fs::create_dir_all(&work).expect("work");
        fs::create_dir_all(&generated).expect("generated");
        fs::write(generated.join("old.png"), png_bytes()).expect("old");
        let before_work = listing(&work);
        let before_generated = listing(&generated);

        fs::write(work.join("drawing.png"), png_bytes()).expect("drawing");
        fs::write(work.join("notes.txt"), b"hello").expect("notes");
        fs::write(generated.join("new.png"), png_bytes()).expect("new");

        let found = new_pictures(&work, Some(&generated), &before_work, &before_generated);
        let mut names: Vec<String> = found
            .iter()
            .map(|(path, _)| {
                path.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["drawing.png", "new.png"], "{found:?}");
    }

    #[test]
    fn a_generated_picture_whose_time_changed_is_collected() {
        let root = temp();
        let generated = root.path().join("generated_images");
        fs::create_dir_all(&generated).expect("generated");
        let old = generated.join("again.png");
        fs::write(&old, png_bytes()).expect("old");
        let before = listing(&generated);

        let later = SystemTime::now() + Duration::from_secs(120);
        fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .and_then(|file| file.set_modified(later))
            .expect("set time");

        let work = root.path().join("work");
        fs::create_dir_all(&work).expect("work");
        let found = new_pictures(&work, Some(&generated), &HashMap::new(), &before);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, old);
    }

    #[test]
    fn text_and_video_are_not_pictures() {
        let root = temp();
        let notes = root.path().join("notes.txt");
        fs::write(&notes, b"plain words").expect("notes");
        let clip = root.path().join("clip.mp4");
        fs::write(&clip, b"\0\0\0\x18ftypmp42rest").expect("clip");
        assert_eq!(picture_ext(&notes), None);
        assert_eq!(picture_ext(&clip), None);
        fs::write(root.path().join("shot.webp"), b"RIFF\0\0\0\0WEBPVP8 ").expect("webp");
        assert_eq!(picture_ext(&root.path().join("shot.webp")), Some("webp"));
    }

    #[test]
    fn the_generated_folder_is_under_codex_home() {
        let root = temp();
        let config = Config {
            program: None,
            codex_home: Some(root.path().to_path_buf()),
        };
        assert_eq!(
            generated_dir(&config),
            Some(root.path().join("generated_images"))
        );
    }

    #[test]
    fn a_missing_program_is_reported_as_missing() {
        let root = temp();
        let config = Config {
            program: Some(root.path().join("no-such-codex")),
            codex_home: None,
        };
        assert_eq!(find_program(&config), None);
        assert_eq!(status(&config), Status::Missing);
    }

    #[test]
    fn a_missing_program_asks_for_the_install() {
        let root = temp();
        let config = Config {
            program: Some(root.path().join("no-such-codex")),
            codex_home: None,
        };
        let error = generate(
            &config,
            &picture_request("a cat"),
            root.path(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect_err("no program");
        assert!(error.contains("npm i -g @openai/codex"), "{error}");
        assert!(error.contains("codex login"), "{error}");
    }

    #[test]
    fn video_and_empty_prompts_are_refused() {
        let root = temp();
        let config = Config::default();
        let never = AtomicBool::new(false);
        let video = Request {
            kind: MediaKind::Video,
            prompt: "a sunset".to_string(),
            ..Request::default()
        };
        let error = generate(&config, &video, root.path(), &|_| {}, &never).expect_err("video");
        assert!(error.contains("pictures only"), "{error}");
        let blank = picture_request("   ");
        let error = generate(&config, &blank, root.path(), &|_| {}, &never).expect_err("blank");
        assert!(error.contains("Write what to draw"), "{error}");
    }

    #[test]
    fn a_fake_codex_picture_is_copied_out_and_the_scratch_folder_removed() {
        let root = temp();
        let fixture = root.path().join("fixture.png");
        fs::write(&fixture, png_bytes()).expect("fixture");
        let body = copy_line(&fixture, "drawing.png");
        let program = fake_codex(root.path(), &body);
        let out = root.path().join("out");
        let config = Config {
            program: Some(program),
            codex_home: Some(root.path().join("home")),
        };
        let reports = std::sync::Mutex::new(Vec::new());
        let outputs = generate(
            &config,
            &picture_request("a cat"),
            &out,
            &|progress| reports.lock().expect("lock").push(progress),
            &AtomicBool::new(false),
        )
        .expect("a picture");

        assert_eq!(outputs.len(), 1, "{outputs:?}");
        assert_eq!(outputs[0].kind, MediaKind::Image);
        assert_eq!(outputs[0].model, IMAGE_MODEL);
        assert_eq!(outputs[0].path.parent(), Some(out.as_path()));
        assert_eq!(
            outputs[0].path.extension().and_then(|e| e.to_str()),
            Some("png")
        );
        assert_eq!(fs::read(&outputs[0].path).expect("read"), png_bytes());
        let leftover: Vec<_> = fs::read_dir(&out)
            .expect("out")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with("-work"))
            .collect();
        assert!(leftover.is_empty(), "scratch folder kept");
        assert!(
            reports
                .lock()
                .expect("lock")
                .iter()
                .any(|p| matches!(p, Progress::Running { fraction: None, .. }))
        );
    }

    #[test]
    fn a_picture_codex_saves_in_its_own_folder_is_taken_too() {
        let root = temp();
        let fixture = root.path().join("fixture.png");
        fs::write(&fixture, png_bytes()).expect("fixture");
        let home = root.path().join("home");
        let generated = home.join("generated_images");
        fs::create_dir_all(&generated).expect("generated");
        let body = copy_line(&fixture, &format!("{}/new.png", generated.display()));
        let program = fake_codex(root.path(), &body);
        let out = root.path().join("out");
        let config = Config {
            program: Some(program),
            codex_home: Some(home),
        };
        let outputs = generate(
            &config,
            &picture_request("a dog"),
            &out,
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect("a picture");
        assert_eq!(outputs.len(), 1, "{outputs:?}");
        assert_eq!(fs::read(&outputs[0].path).expect("read"), png_bytes());
    }

    #[test]
    fn a_run_without_pictures_says_why_and_keeps_no_token() {
        let root = temp();
        #[cfg(windows)]
        let body = [
            "echo something went wrong 1>&2",
            "echo token=abc123 1>&2",
            "exit /b 3",
        ]
        .join("\r\n");
        #[cfg(not(windows))]
        let body = [
            "echo \"something went wrong\" >&2",
            "echo \"token=abc123\" >&2",
            "exit 3",
        ]
        .join("\n");
        let program = fake_codex(root.path(), &body);
        let config = Config {
            program: Some(program),
            codex_home: Some(root.path().join("home")),
        };
        let error = generate(
            &config,
            &picture_request("a cat"),
            &root.path().join("out"),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect_err("no picture");
        assert!(error.contains("went wrong"), "{error}");
        assert!(error.contains("failed"), "{error}");
        assert!(!error.contains("abc123"), "{error}");
        assert!(!error.to_ascii_lowercase().contains("token"), "{error}");
    }

    #[test]
    fn cancel_stops_the_codex_run_promptly() {
        let root = temp();
        // A batch fixture must keep waiting even when its PATH has no system
        // commands; otherwise an early shell error masquerades as cancellation.
        #[cfg(windows)]
        let wait = format!("set \"PATH=\"\r\n{WAIT_LINE}");
        #[cfg(not(windows))]
        let wait = WAIT_LINE.to_owned();
        let program = fake_codex(root.path(), &wait);
        let config = Config {
            program: Some(program),
            codex_home: Some(root.path().join("home")),
        };
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
            });
            generate(
                &config,
                &picture_request("a cat"),
                &root.path().join("out"),
                &|_| {},
                &cancel,
            )
        });
        assert_eq!(result, Err("cancelled".to_string()));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(windows)]
    #[test]
    fn awkward_characters_reach_the_batch_script_intact() {
        let root = temp();
        let script = root.path().join("echo-args.cmd");
        fs::write(&script, "@echo off\r\n> \"%~dp0args.txt\" echo %*\r\n").expect("script");
        let status = Command::new(&script)
            .args(["a&b", "50%", "x^y", "p q", "z!"])
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run script");
        assert!(status.success());
        let written = fs::read_to_string(root.path().join("args.txt")).expect("args");
        for piece in ["a&b", "x^y", "p q"] {
            assert!(written.contains(piece), "{piece} missing from {written:?}");
        }
    }
}
