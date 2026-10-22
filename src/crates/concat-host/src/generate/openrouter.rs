// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! OpenRouter's picture and video APIs, reached with the person's own key.
//!
//! A picture comes back in one answer: the request waits for the model, and
//! the answer carries the file as base64, or as an address to fetch. A video
//! is a job instead. The submit answers at once with an id, the job is
//! polled until it is finished, and each output is then fetched from the
//! job's content address with the key, since that address is the API's own.
//!
//! The key goes to the API's host and nowhere else. A polling address the
//! answer names on another host is not followed with it, a picture's address
//! is fetched without it, and every error string has the key taken out before
//! it leaves this module.
//!
//! Polling sleeps in short slices so that a cancel is seen within a quarter
//! of a second, and a video that is not finished after half an hour is given
//! up on rather than polled for ever.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{MediaKind, Output, Progress, Request, extension_for, random_hex, sniff};

/// The API's root. Every call goes under it, and it is the only host the
/// person's key is sent to.
pub const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// The picture model a request uses when it names none.
pub const DEFAULT_IMAGE_MODEL: &str = "google/gemini-3.1-flash-image";

/// The video model a request uses when it names none.
pub const DEFAULT_VIDEO_MODEL: &str = "google/veo-3.1-fast";

/// Sent with every call, as OpenRouter asks of the apps that use its API.
const REFERER: &str = "https://github.com/jub0t/Concat";
const TITLE: &str = "Concat";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a picture request may wait for the model's answer.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(300);
/// Every quick call: a video submit, a poll, a model list.
const QUICK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a file may take to come down.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// How long a video may take before it is given up on.
const VIDEO_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// How long to wait between two polls of a video job.
#[cfg(not(test))]
const POLL_EVERY: Duration = Duration::from_secs(5);
#[cfg(test)]
const POLL_EVERY: Duration = Duration::from_millis(20);
/// The longest sleep between two looks at the cancel flag.
const SLICE: Duration = Duration::from_millis(250);

/// The most a JSON answer from a quick call may run to.
const JSON_LIMIT: u64 = 8 * 1024 * 1024;
/// The most a picture answer may run to: a base64 picture is a third larger
/// than its file.
const ANSWER_LIMIT: u64 = 256 * 1024 * 1024;
/// The most one generated file may be.
const FILE_LIMIT: u64 = 1024 * 1024 * 1024;
/// How much of an error answer is read for its message.
const ERROR_LIMIT: u64 = 64 * 1024;

/// The error a cancelled request returns; the caller knows it by this text.
const CANCELLED: &str = "cancelled";

/// What a call to OpenRouter needs: the person's key and where the API is.
#[derive(Clone)]
pub struct Config {
    /// The person's OpenRouter key. It is sent only as the bearer token to
    /// `base_url`, and is never shown, logged or put in an error.
    pub api_key: String,
    /// The API's root: [`BASE_URL`] unless a proxy or a test says otherwise.
    pub base_url: String,
}

impl Config {
    /// A config for the public API, with `api_key`.
    pub fn new(api_key: impl Into<String>) -> Config {
        Config {
            api_key: api_key.into(),
            base_url: BASE_URL.to_owned(),
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// A model OpenRouter offers, as its public list describes it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ModelInfo {
    /// The name a request sends, as in `google/veo-3.1-fast`.
    pub id: String,
    /// The name to show; the id when the list gives none.
    pub name: String,
    /// What the model is for, in the list's words.
    pub description: String,
    /// The aspect ratios it takes, such as "16:9". Empty when it takes any.
    pub aspect_ratios: Vec<String>,
    /// The resolutions it takes, such as "720p". Empty when it takes any.
    pub resolutions: Vec<String>,
    /// The durations, in seconds, it takes. Empty for pictures.
    pub durations: Vec<u32>,
}

/// The models OpenRouter offers for `kind`, read from its public list at
/// `base_url`. No key is needed, so this works before one is pasted in.
pub fn list_models(base_url: &str, kind: MediaKind) -> Result<Vec<ModelInfo>, String> {
    let path = match kind {
        MediaKind::Image => "images/models",
        MediaKind::Video => "videos/models",
    };
    let url = format!("{}/{path}", base_url.trim_end_matches('/'));
    let response = reply(
        "",
        "OpenRouter",
        identify(agent(QUICK_TIMEOUT).get(&url)).call(),
    )?;
    let bytes = read_body(response.into_reader(), JSON_LIMIT)?;
    let value: Value = parse_json("", &bytes, "model list")?;
    parse_models(&value)
}

/// Makes what `request` asks for, with the person's key, and writes each
/// finished file into `out_dir`.
///
/// A picture is one request. A video is submitted, then polled every few
/// seconds while `progress` reports how it is going, and then downloaded. An
/// empty key or an empty prompt is refused before anything is sent. A
/// cancelled call returns `Err("cancelled")`, and any file already written
/// by it is removed.
pub fn generate(
    config: &Config,
    request: &Request,
    out_dir: &Path,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Vec<Output>, String> {
    let key = config.api_key.trim();
    if key.is_empty() {
        return Err(
            "Paste an OpenRouter key in Settings first: pictures and video from OpenRouter are paid for with your own key."
                .to_owned(),
        );
    }
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err("Write what to make first: the prompt is empty.".to_owned());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.to_owned());
    }
    std::fs::create_dir_all(out_dir)
        .map_err(|error| format!("could not create {}: {error}", out_dir.display()))?;
    let model = match request.model.trim() {
        "" => default_model(request.kind),
        model => model,
    };
    let call = Call {
        key,
        base: config.base_url.trim_end_matches('/'),
        request,
        prompt,
        model,
        out_dir,
        progress,
        cancel,
    };
    let mut written = Vec::new();
    let result = match request.kind {
        MediaKind::Image => call.image(&mut written),
        MediaKind::Video => call.video(&mut written),
    };
    if result.is_err() {
        for path in &written {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

/// One generation, with what every step of it needs.
struct Call<'a> {
    key: &'a str,
    base: &'a str,
    request: &'a Request,
    prompt: &'a str,
    model: &'a str,
    out_dir: &'a Path,
    progress: &'a dyn Fn(Progress),
    cancel: &'a AtomicBool,
}

impl Call<'_> {
    /// A picture: one request, and its files from the answer.
    fn image(&self, written: &mut Vec<PathBuf>) -> Result<Vec<Output>, String> {
        let mut body = json!({ "model": self.model, "prompt": self.prompt });
        if self.request.count() > 1 {
            body["n"] = json!(self.request.count());
        }
        set_size(&mut body, self.request);
        if let Some(reference) = &self.request.reference {
            body["input_references"] = json!([{
                "type": "image_url",
                "image_url": { "url": data_url(reference)? },
            }]);
        }
        (self.progress)(Progress::Running {
            fraction: None,
            message: "Waiting for the picture".to_owned(),
        });
        let response = self.post(&format!("{}/images", self.base), IMAGE_TIMEOUT, &body)?;
        let bytes = read_body(response.into_reader(), ANSWER_LIMIT)?;
        let parsed: ImageAnswer = parse_json(self.key, &bytes, "picture answer")?;
        if parsed.data.is_empty() {
            return Err("OpenRouter answered without a picture.".to_owned());
        }
        let mut outputs = Vec::with_capacity(parsed.data.len());
        for item in parsed.data {
            self.check_cancel()?;
            if let Some(encoded) = item.b64_json.filter(|text| !text.trim().is_empty()) {
                let (inline_type, payload) = split_data_url(&encoded);
                let bytes = decode_base64(payload)?;
                let media = item.media_type.as_deref().or(inline_type);
                outputs.push(self.save(&bytes, media, written)?);
            } else if let Some(address) = item.url.filter(|text| !text.trim().is_empty()) {
                // The address is not the API's: it is fetched without the key.
                (self.progress)(Progress::Downloading);
                let fetched = reply(
                    "",
                    "The picture's host",
                    agent(DOWNLOAD_TIMEOUT).get(&address).call(),
                )?;
                let media = fetched.header("Content-Type").map(str::to_owned);
                let bytes = read_body(fetched.into_reader(), FILE_LIMIT)?;
                outputs.push(self.save(&bytes, media.as_deref(), written)?);
            } else {
                return Err(
                    "OpenRouter's picture answer had an entry with neither b64_json nor url."
                        .to_owned(),
                );
            }
        }
        Ok(outputs)
    }

    /// A video: one job, polled until it is finished, then its outputs.
    fn video(&self, written: &mut Vec<PathBuf>) -> Result<Vec<Output>, String> {
        let mut body = json!({ "model": self.model, "prompt": self.prompt });
        if self.request.duration > 0 {
            body["duration"] = json!(self.request.duration);
        }
        set_size(&mut body, self.request);
        if let Some(reference) = &self.request.reference {
            body["frame_images"] = json!([{
                "type": "image_url",
                "image_url": { "url": data_url(reference)? },
                "frame_type": "first_frame",
            }]);
        }
        let response = self.post(&format!("{}/videos", self.base), QUICK_TIMEOUT, &body)?;
        let bytes = read_body(response.into_reader(), JSON_LIMIT)?;
        let submitted: Submitted = parse_json(self.key, &bytes, "video submit answer")?;
        if !valid_job_id(&submitted.id) {
            return Err("OpenRouter answered with a job id this app will not follow.".to_owned());
        }
        let id = submitted.id;
        // The polling address is followed only when it is on the API's own
        // host; otherwise the job is polled at the address this app builds.
        let own_prefix = format!("{}/", self.base);
        let poll_url = match submitted.polling_url {
            Some(url) if url == self.base || url.starts_with(&own_prefix) => url,
            _ => format!("{}/videos/{id}", self.base),
        };

        (self.progress)(Progress::Queued);
        let started = Instant::now();
        let finished = loop {
            self.check_cancel()?;
            let job = self.poll(&poll_url)?;
            if job.status == "completed" {
                break job;
            }
            if job.status == "failed" {
                let reason = job
                    .error
                    .as_ref()
                    .and_then(error_text)
                    .unwrap_or_else(|| "no reason given".to_owned());
                return Err(scrub(
                    self.key,
                    format!("OpenRouter could not make the video: {reason}"),
                ));
            }
            if matches!(job.status.as_str(), "cancelled" | "canceled" | "expired") {
                return Err(format!(
                    "OpenRouter stopped the video job ({}).",
                    job.status
                ));
            }
            (self.progress)(Progress::Running {
                fraction: None,
                message: job.status,
            });
            if started.elapsed() >= VIDEO_DEADLINE {
                return Err(format!(
                    "The video is still not ready after 30 minutes; OpenRouter job {id} may still finish there."
                ));
            }
            pause(POLL_EVERY, self.cancel)?;
        };

        (self.progress)(Progress::Downloading);
        let count = finished.unsigned_urls.len().max(1);
        let mut outputs = Vec::with_capacity(count);
        for index in 0..count {
            self.check_cancel()?;
            let url = format!("{}/videos/{id}/content?index={index}", self.base);
            let response = reply(
                self.key,
                "OpenRouter",
                authorize(self.key, agent(DOWNLOAD_TIMEOUT).get(&url)).call(),
            )?;
            let media = response.header("Content-Type").map(str::to_owned);
            let bytes = read_body(response.into_reader(), FILE_LIMIT)?;
            outputs.push(self.save(&bytes, media.as_deref(), written)?);
        }
        Ok(outputs)
    }

    /// One POST with the key, as JSON.
    fn post(&self, url: &str, timeout: Duration, body: &Value) -> Result<ureq::Response, String> {
        let bytes = serde_json::to_vec(body)
            .map_err(|error| format!("could not encode the request: {error}"))?;
        let request = authorize(
            self.key,
            agent(timeout)
                .post(url)
                .set("Content-Type", "application/json"),
        );
        reply(self.key, "OpenRouter", request.send_bytes(&bytes))
    }

    /// One look at a video job.
    fn poll(&self, url: &str) -> Result<Job, String> {
        let response = reply(
            self.key,
            "OpenRouter",
            authorize(self.key, agent(QUICK_TIMEOUT).get(url)).call(),
        )?;
        let bytes = read_body(response.into_reader(), JSON_LIMIT)?;
        parse_json(self.key, &bytes, "video status")
    }

    /// Writes one file into `out_dir` under a random name, and records it so
    /// a failed generation can take it back out.
    fn save(
        &self,
        bytes: &[u8],
        media_type: Option<&str>,
        written: &mut Vec<PathBuf>,
    ) -> Result<Output, String> {
        if bytes.is_empty() {
            return Err("OpenRouter sent an empty file.".to_owned());
        }
        let fallback = match self.request.kind {
            MediaKind::Image => "png",
            MediaKind::Video => "mp4",
        };
        let ext = media_type
            .and_then(extension_for)
            .or_else(|| sniff(bytes).map(|(ext, _)| ext))
            .unwrap_or(fallback);
        let path = self.out_dir.join(format!("{}.{ext}", random_hex(12)));
        std::fs::write(&path, bytes)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        written.push(path.clone());
        Ok(Output {
            path,
            kind: self.request.kind,
            model: self.model.to_owned(),
        })
    }

    fn check_cancel(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(CANCELLED.to_owned())
        } else {
            Ok(())
        }
    }
}

/// The picture answer: `data` holds one entry per picture.
#[derive(Deserialize)]
struct ImageAnswer {
    #[serde(default)]
    data: Vec<ImageItem>,
}

/// One picture in the answer: inline as base64, or at an address.
#[derive(Deserialize)]
struct ImageItem {
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    media_type: Option<String>,
}

/// The answer to a video submit: the job's id, and where to poll it.
#[derive(Deserialize)]
struct Submitted {
    id: String,
    #[serde(default)]
    polling_url: Option<String>,
}

/// A video job as one poll reports it.
#[derive(Deserialize)]
struct Job {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    unsigned_urls: Vec<String>,
}

/// The agent for one call: connect and read timeouts only, since a
/// generation can be long but a stalled connection must not hold it.
fn agent(read: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(read)
        .build()
}

/// Adds the headers OpenRouter asks of every app.
fn identify(request: ureq::Request) -> ureq::Request {
    request.set("HTTP-Referer", REFERER).set("X-Title", TITLE)
}

/// Adds the headers for a call that carries the person's key.
fn authorize(key: &str, request: ureq::Request) -> ureq::Request {
    identify(request.set("Authorization", &format!("Bearer {key}")))
}

/// The response, or the error as one string: the status and the API's own
/// message for a refusal, and the transport's words otherwise. `source` says
/// who answered, for the words. The key is taken out of all of it.
fn reply(
    key: &str,
    source: &str,
    result: Result<ureq::Response, ureq::Error>,
) -> Result<ureq::Response, String> {
    match result {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(code, response)) => {
            let body = read_body(response.into_reader(), ERROR_LIMIT).unwrap_or_default();
            Err(scrub(
                key,
                format!("{source} answered {code}: {}", error_detail(&body)),
            ))
        }
        Err(error) => Err(scrub(key, format!("{source} did not answer: {error}"))),
    }
}

/// The message an error answer carries: `error.message` when it is JSON
/// that says so, otherwise the start of the body, or nothing.
fn error_detail(body: &[u8]) -> String {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    if let Some(message) = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(error_text)
    {
        return message;
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        "no message given".to_owned()
    } else {
        text.chars().take(200).collect()
    }
}

/// The words of an error: a plain string, or an object's `message`.
fn error_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.as_str(),
        Value::Object(_) => value.get("message")?.as_str()?,
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// `text` with the key taken out, so no message carries it.
fn scrub(key: &str, text: String) -> String {
    if key.is_empty() {
        text
    } else {
        text.replace(key, "<redacted>")
    }
}

/// Reads a whole answer, refusing one larger than `limit`.
fn read_body(reader: impl Read, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("the answer did not arrive: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("the answer is larger than {limit} bytes"));
    }
    Ok(bytes)
}

/// Parses a JSON answer. The error names the answer, never its content, and
/// has the key taken out in case the parser ever echoes it.
fn parse_json<T: DeserializeOwned>(key: &str, bytes: &[u8], what: &str) -> Result<T, String> {
    serde_json::from_slice(bytes)
        .map_err(|error| scrub(key, format!("OpenRouter's {what} did not parse: {error}")))
}

/// Sleeps for `total`, looking at `cancel` between slices of at most a
/// quarter of a second.
fn pause(total: Duration, cancel: &AtomicBool) -> Result<(), String> {
    let until = Instant::now() + total;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_owned());
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(SLICE));
    }
}

/// Sets the aspect ratio and resolution a request names, when it names them.
fn set_size(body: &mut Value, request: &Request) {
    let aspect = request.aspect_ratio.trim();
    if !aspect.is_empty() {
        body["aspect_ratio"] = json!(aspect);
    }
    let resolution = request.resolution.trim();
    if !resolution.is_empty() {
        body["resolution"] = json!(resolution);
    }
}

fn default_model(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => DEFAULT_IMAGE_MODEL,
        MediaKind::Video => DEFAULT_VIDEO_MODEL,
    }
}

/// A job id is put in a URL path, so only the characters an id is made of
/// are followed.
fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// The reference picture as a data URL. Its type is sniffed from the bytes,
/// not taken from its name, and only the picture types the API takes are
/// sent.
fn data_url(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| {
        format!(
            "could not read the reference picture {}: {error}",
            path.display()
        )
    })?;
    match sniff(&bytes) {
        Some((ext, MediaKind::Image)) => Ok(format!(
            "data:{};base64,{}",
            mime_for(ext),
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        )),
        _ => Err(format!(
            "the reference {} is not a PNG, JPEG, WebP or GIF picture",
            path.display()
        )),
    }
}

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

/// Splits a `data:` URL into its media type and its payload. Text that is
/// not a data URL is all payload.
fn split_data_url(text: &str) -> (Option<&str>, &str) {
    match text
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
    {
        Some((head, payload)) => (
            head.split(';').next().filter(|media| !media.is_empty()),
            payload,
        ),
        None => (None, text),
    }
}

fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|error| format!("OpenRouter's picture was not valid base64: {error}"))
}

/// The models in a model list. An entry without an id is not a model.
fn parse_models(value: &Value) -> Result<Vec<ModelInfo>, String> {
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "OpenRouter's model list has no data.".to_owned())?;
    Ok(entries.iter().filter_map(model_info).collect())
}

/// One model. Video entries name their options at the top; picture entries
/// name them under `supported_parameters`.
fn model_info(entry: &Value) -> Option<ModelInfo> {
    let id = entry.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let text = |field: &str| {
        entry
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let name = text("name");
    Some(ModelInfo {
        id: id.to_owned(),
        name: if name.is_empty() { id.to_owned() } else { name },
        description: text("description"),
        aspect_ratios: strings_at(
            entry,
            &[
                "/supported_aspect_ratios",
                "/supported_parameters/aspect_ratio/values",
            ],
        ),
        resolutions: strings_at(
            entry,
            &[
                "/supported_resolutions",
                "/supported_parameters/resolution/values",
            ],
        ),
        durations: entry
            .get("supported_durations")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_u64)
                    .filter_map(|seconds| u32::try_from(seconds).ok())
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The first non-empty list of strings among `pointers` in `entry`.
fn strings_at(entry: &Value, pointers: &[&str]) -> Vec<String> {
    pointers
        .iter()
        .filter_map(|pointer| entry.pointer(pointer)?.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<String>>()
        })
        .find(|found| !found.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    type Requests = Arc<Mutex<Vec<String>>>;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nthe picture";
    const JPEG: &[u8] = b"\xff\xd8\xffthe picture";
    const WEBP: &[u8] = b"RIFF\0\0\0\0WEBPVP8 the picture";

    /// One canned answer for the mock server.
    struct Canned {
        status: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    }

    impl Canned {
        fn json(status: &'static str, body: Value) -> Canned {
            Canned {
                status,
                content_type: "application/json",
                body: body.to_string().into_bytes(),
            }
        }

        fn file(content_type: &'static str, body: &[u8]) -> Canned {
            Canned {
                status: "200 OK",
                content_type,
                body: body.to_vec(),
            }
        }
    }

    /// Reads one request, head and body, off the connection.
    fn read_request(stream: &mut TcpStream) -> String {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(at) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break at + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break bytes.len(),
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            }
        };
        let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
        let length: usize = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        while bytes.len() < head_end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// A loopback server that answers `replies` in order, one connection
    /// each, and logs every request before it answers it. `build` gets the
    /// API root, so an answer can name an address on this server.
    fn serve(build: impl FnOnce(&str) -> Vec<Canned>) -> (String, Requests) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let base = format!(
            "http://{}/api/v1",
            listener.local_addr().expect("an address")
        );
        let replies = build(&base);
        let requests: Requests = Arc::default();
        let log = Arc::clone(&requests);
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream);
                log.lock().expect("the request log").push(request);
                let head = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reply.content_type,
                    reply.body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&reply.body);
            }
        });
        (base, requests)
    }

    fn requests_of(requests: &Requests) -> Vec<String> {
        requests.lock().expect("the request log").clone()
    }

    fn config(base: &str) -> Config {
        Config {
            api_key: "test-key".to_owned(),
            base_url: base.to_owned(),
        }
    }

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("a scratch dir")
    }

    fn video_request() -> Request {
        Request {
            kind: MediaKind::Video,
            prompt: "a fox runs".to_owned(),
            ..Request::default()
        }
    }

    #[test]
    fn image_generation_writes_the_decoded_picture() {
        let (base, requests) = serve(|_| {
            vec![Canned::json(
                "200 OK",
                json!({
                    "created": 1,
                    "data": [{
                        "b64_json": base64::engine::general_purpose::STANDARD.encode(PNG),
                        "media_type": "image/png",
                    }],
                    "usage": { "cost": 0.04 },
                }),
            )]
        });
        let dir = scratch();
        let request = Request {
            kind: MediaKind::Image,
            prompt: "  a red fox  ".to_owned(),
            model: "vendor/model".to_owned(),
            aspect_ratio: "16:9".to_owned(),
            ..Request::default()
        };
        let outputs = generate(
            &config(&base),
            &request,
            &dir.path().join("made"),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect("a picture");

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, MediaKind::Image);
        assert_eq!(outputs[0].model, "vendor/model");
        assert_eq!(
            outputs[0].path.extension().and_then(|ext| ext.to_str()),
            Some("png")
        );
        assert_eq!(std::fs::read(&outputs[0].path).expect("the file"), PNG);

        let seen = requests_of(&requests);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("POST /api/v1/images "));
        assert!(seen[0].contains("Authorization: Bearer test-key"));
        assert!(seen[0].contains("\"prompt\":\"a red fox\""));
        assert!(seen[0].contains("\"model\":\"vendor/model\""));
        assert!(seen[0].contains("\"aspect_ratio\":\"16:9\""));
        assert!(!seen[0].contains("\"n\":"), "one picture asks for no n");
    }

    #[test]
    fn image_answers_are_read_leniently() {
        let (base, requests) = serve(|base| {
            vec![
                Canned::json(
                    "200 OK",
                    json!({
                        "data": [
                            { "b64_json": format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(JPEG)) },
                            { "url": format!("{base}/files/pic.webp") },
                        ],
                    }),
                ),
                Canned::file("image/webp", WEBP),
            ]
        });
        let dir = scratch();
        let request = Request {
            kind: MediaKind::Image,
            prompt: "two things".to_owned(),
            count: 2,
            ..Request::default()
        };
        let outputs = generate(
            &config(&base),
            &request,
            dir.path(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect("two pictures");

        assert_eq!(outputs.len(), 2);
        let jpg = outputs[0].path.clone();
        assert_eq!(jpg.extension().and_then(|ext| ext.to_str()), Some("jpg"));
        assert_eq!(std::fs::read(&jpg).expect("the jpg"), JPEG);
        let webp = outputs[1].path.clone();
        assert_eq!(webp.extension().and_then(|ext| ext.to_str()), Some("webp"));
        assert_eq!(std::fs::read(&webp).expect("the webp"), WEBP);

        let seen = requests_of(&requests);
        assert_eq!(seen.len(), 2);
        assert!(seen[0].contains("\"n\":2"));
        assert!(seen[1].starts_with("GET /api/v1/files/pic.webp "));
        assert!(
            !seen[1].contains("Authorization"),
            "a picture's own address is not sent the key"
        );
    }

    #[test]
    fn a_reference_picture_is_sent_as_a_data_url() {
        let dir = scratch();
        let reference = dir.path().join("start.png");
        std::fs::write(&reference, PNG).expect("the reference");
        let (base, requests) = serve(|_| {
            vec![Canned::json(
                "200 OK",
                json!({ "data": [{ "b64_json": base64::engine::general_purpose::STANDARD.encode(PNG) }] }),
            )]
        });
        let request = Request {
            kind: MediaKind::Image,
            prompt: "make it blue".to_owned(),
            reference: Some(reference),
            ..Request::default()
        };
        generate(
            &config(&base),
            &request,
            &dir.path().join("made"),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect("a picture");

        let seen = requests_of(&requests);
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(PNG)
        );
        assert!(seen[0].contains("\"input_references\""));
        assert!(seen[0].contains(&url));
    }

    #[test]
    fn video_is_submitted_polled_then_downloaded() {
        let video = b"\0\0\0\x18ftypmp42the video".to_vec();
        let (base, requests) = serve(|_| {
            vec![
                // The submit names an address off the API's host: it must
                // not be followed with the key, so the job is polled at the
                // API's own address instead.
                Canned::json(
                    "202 Accepted",
                    json!({
                        "id": "job_1",
                        "polling_url": "https://elsewhere.invalid/videos/job_1",
                        "status": "pending",
                    }),
                ),
                Canned::json("200 OK", json!({ "id": "job_1", "status": "in_progress" })),
                Canned::json(
                    "200 OK",
                    json!({
                        "id": "job_1",
                        "status": "completed",
                        "unsigned_urls": ["https://elsewhere.invalid/unused"],
                    }),
                ),
                Canned::file("video/mp4", &video),
            ]
        });
        let dir = scratch();
        let request = Request {
            duration: 5,
            resolution: "720p".to_owned(),
            aspect_ratio: "16:9".to_owned(),
            ..video_request()
        };
        let events = std::cell::RefCell::new(Vec::new());
        let progress = |event: Progress| events.borrow_mut().push(event);
        let outputs = generate(
            &config(&base),
            &request,
            dir.path(),
            &progress,
            &AtomicBool::new(false),
        )
        .expect("a video");

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, MediaKind::Video);
        assert_eq!(outputs[0].model, DEFAULT_VIDEO_MODEL);
        assert_eq!(
            outputs[0].path.extension().and_then(|ext| ext.to_str()),
            Some("mp4")
        );
        assert_eq!(std::fs::read(&outputs[0].path).expect("the file"), video);

        let events = events.borrow();
        assert_eq!(events.first(), Some(&Progress::Queued));
        assert!(events.iter().any(|event| matches!(
            event,
            Progress::Running { message, .. } if message == "in_progress"
        )));
        assert_eq!(events.last(), Some(&Progress::Downloading));

        let seen = requests_of(&requests);
        assert_eq!(seen.len(), 4, "submit, two polls, one download");
        assert!(seen[0].starts_with("POST /api/v1/videos "));
        assert!(seen[0].contains("Authorization: Bearer test-key"));
        assert!(seen[0].contains("\"duration\":5"));
        assert!(seen[0].contains("\"resolution\":\"720p\""));
        assert!(seen[1].starts_with("GET /api/v1/videos/job_1 "));
        assert!(seen[2].starts_with("GET /api/v1/videos/job_1 "));
        assert!(seen[3].starts_with("GET /api/v1/videos/job_1/content?index=0 "));
        assert!(seen[3].contains("Authorization: Bearer test-key"));
    }

    #[test]
    fn a_failed_job_surfaces_its_error() {
        let (base, _) = serve(|_| {
            vec![
                Canned::json(
                    "202 Accepted",
                    json!({ "id": "job_2", "status": "pending" }),
                ),
                Canned::json(
                    "200 OK",
                    json!({ "id": "job_2", "status": "failed", "error": "Content policy refused the prompt" }),
                ),
            ]
        });
        let dir = scratch();
        let error = generate(
            &config(&base),
            &video_request(),
            dir.path(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect_err("the job failed");
        assert!(
            error.contains("Content policy refused the prompt"),
            "{error}"
        );
    }

    #[test]
    fn an_api_error_surfaces_its_message_without_the_key() {
        let (base, _) = serve(|_| {
            vec![Canned::json(
                "402 Payment Required",
                // The message echoes the key on purpose: it must not survive.
                json!({ "error": { "message": "Insufficient credits for test-key", "code": 402 } }),
            )]
        });
        let dir = scratch();
        let request = Request {
            kind: MediaKind::Image,
            prompt: "a fox".to_owned(),
            ..Request::default()
        };
        let error = generate(
            &config(&base),
            &request,
            dir.path(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect_err("the API refused");
        assert!(error.contains("402"), "{error}");
        assert!(error.contains("Insufficient credits"), "{error}");
        assert!(!error.contains("test-key"), "the key leaked: {error}");
    }

    #[test]
    fn cancel_stops_the_polling() {
        let (base, _) = serve(|_| {
            let mut replies = vec![Canned::json(
                "202 Accepted",
                json!({ "id": "job_3", "status": "pending" }),
            )];
            replies.extend((0..200).map(|_| {
                Canned::json("200 OK", json!({ "id": "job_3", "status": "in_progress" }))
            }));
            replies
        });
        let dir = scratch();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, Ordering::Relaxed);
            });
            generate(
                &config(&base),
                &video_request(),
                dir.path(),
                &|_| {},
                &cancel,
            )
            .expect_err("the job was cancelled")
        });
        assert_eq!(error, CANCELLED);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_request_cancelled_before_it_starts_makes_no_call() {
        let dir = scratch();
        // Nothing listens on the discard port: a call would fail with a
        // transport error rather than "cancelled".
        let error = generate(
            &config("http://127.0.0.1:9/api/v1"),
            &video_request(),
            dir.path(),
            &|_| {},
            &AtomicBool::new(true),
        )
        .expect_err("cancelled");
        assert_eq!(error, CANCELLED);
    }

    #[test]
    fn list_models_parses_both_shapes() {
        let (base, requests) = serve(|_| {
            vec![
                Canned::json(
                    "200 OK",
                    json!({
                        "data": [
                            {
                                "id": "google/gemini-3.1-flash-image",
                                "name": "Gemini 3.1 Flash Image",
                                "description": "Pictures",
                                "supported_parameters": {
                                    "aspect_ratio": { "values": ["1:1", "16:9"] },
                                    "resolution": { "values": ["1K", "2K"] },
                                },
                            },
                            { "name": "an entry without an id is skipped" },
                        ],
                    }),
                ),
                Canned::json(
                    "200 OK",
                    json!({
                        "data": [{
                            "id": "google/veo-3.1-fast",
                            "name": "Veo 3.1 Fast",
                            "description": "Video",
                            "supported_resolutions": ["720p", "1080p"],
                            "supported_aspect_ratios": ["16:9", "9:16"],
                            "supported_durations": [4, 6, 8],
                        }],
                    }),
                ),
            ]
        });

        let images = list_models(&base, MediaKind::Image).expect("the picture models");
        assert_eq!(
            images,
            vec![ModelInfo {
                id: "google/gemini-3.1-flash-image".to_owned(),
                name: "Gemini 3.1 Flash Image".to_owned(),
                description: "Pictures".to_owned(),
                aspect_ratios: vec!["1:1".to_owned(), "16:9".to_owned()],
                resolutions: vec!["1K".to_owned(), "2K".to_owned()],
                durations: Vec::new(),
            }]
        );

        let videos = list_models(&base, MediaKind::Video).expect("the video models");
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].id, "google/veo-3.1-fast");
        assert_eq!(videos[0].resolutions, vec!["720p", "1080p"]);
        assert_eq!(videos[0].aspect_ratios, vec!["16:9", "9:16"]);
        assert_eq!(videos[0].durations, vec![4, 6, 8]);

        let seen = requests_of(&requests);
        assert!(seen[0].starts_with("GET /api/v1/images/models "));
        assert!(seen[1].starts_with("GET /api/v1/videos/models "));
        assert!(
            seen.iter()
                .all(|request| !request.contains("Authorization")),
            "the public list is read without a key"
        );
    }

    #[test]
    fn debug_of_config_hides_the_key() {
        let config = Config::new("sk-or-very-secret");
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-or-very-secret"), "{debug}");
        assert!(debug.contains(BASE_URL), "{debug}");
    }

    #[test]
    fn an_empty_key_or_prompt_is_refused_before_any_call() {
        let dir = scratch();
        let cancel = AtomicBool::new(false);
        let error = generate(
            &Config::new("   "),
            &video_request(),
            dir.path(),
            &|_| {},
            &cancel,
        )
        .expect_err("no key");
        assert!(error.contains("OpenRouter key"), "{error}");

        let error = generate(
            &Config::new("sk-or-key"),
            &Request::default(),
            dir.path(),
            &|_| {},
            &cancel,
        )
        .expect_err("no prompt");
        assert!(error.contains("prompt"), "{error}");
    }
}
