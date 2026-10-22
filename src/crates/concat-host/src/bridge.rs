// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The local end of the browser bridge: a small HTTP server on
//! `127.0.0.1` that the Concat Bridge extension talks to.
//!
//! Google Flow and chatgpt.com's web page have no API, so their pictures
//! and videos are made in the person's own browser, where they are already
//! signed in. The extension long-polls this server for a job, drives the
//! page, and posts each file it downloads back here. Concat hears how the
//! job goes through the [`Event`]s on the receiver [`Bridge::submit`]
//! returns, and takes the files into the asset library from there.
//!
//! The wire protocol is spoken by a separate extension, so its routes, field
//! names and status codes are fixed here. Every route except
//! `GET /v1/hello` and the CORS preflight needs the pairing token the person
//! copies into the extension. The token, not the origin, is what keeps a web
//! page out, which is why CORS allows any origin.
//!
//! The server is written by hand on `std::net`: the protocol is six routes,
//! a request line, headers and a `Content-Length` body, and a framework would
//! be the largest dependency in the crate for that. Each connection gets a
//! short-lived thread, capped in number, and closes after one exchange. Jobs
//! live in memory. A job the browser took and then went silent on for twenty
//! minutes is failed, so a taken job always reaches an end.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use crate::generate::{Provider, Request, extension_for, random_hex, sniff};

/// The port the extension looks for Concat on unless the person chose another.
pub const DEFAULT_PORT: u16 = 47615;

/// The protocol number `GET /v1/hello` reports.
const PROTOCOL: u64 = 1;
/// Connections served at once; one more is answered with 503.
const MAX_HANDLERS: usize = 16;
/// How long a read or a write may stall before the connection is dropped.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a finished exchange waits for the client to hang up; see `serve`.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// The most unread request body that is discarded after an answer.
const DRAIN_LIMIT: u64 = 1024 * 1024;
/// How often the acceptor looks at its stop flag while nobody connects.
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// The longest a `next` poll is held open, whatever it asks for.
const MAX_WAIT_SECS: u64 = 25;
/// A site counts as connected while its last poll is this recent.
const CONNECTED_WITHIN: Duration = Duration::from_secs(40);
/// A taken job the browser has not been heard from for this long is failed.
const STALE_AFTER: Duration = Duration::from_secs(20 * 60);
/// A finished job is kept this long, so the extension's last look still finds it.
const KEEP_FINISHED: Duration = Duration::from_secs(60 * 60);
/// The largest JSON body; progress, fail and the like are a few bytes.
const MAX_JSON_BODY: u64 = 1024 * 1024;
/// The largest file a page may post: 1 GiB.
const MAX_FILE_BODY: u64 = 1024 * 1024 * 1024;
/// The longest single request or header line accepted.
const MAX_LINE: usize = 8 * 1024;
/// The most header bytes accepted in all.
const MAX_HEAD: usize = 64 * 1024;
/// The most header lines accepted.
const MAX_HEADERS: usize = 100;
/// The CORS headers every answer carries, the preflight included. The last
/// is Chrome's Private Network Access: a page on the public web may only
/// reach 127.0.0.1 when the preflight says so. The extension calls from
/// its own worker, which needs none of this, but a content script that
/// calls directly still works.
const CORS: &str = "Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Headers: Content-Type, X-Concat-Token, X-File-Name, X-Index\r\n\
Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
Access-Control-Allow-Private-Network: true\r\n";

/// A generation job as the extension receives it: what to make, and for which site.
#[derive(Clone, Debug)]
pub struct Job {
    /// The job's id, which the extension names in every later request.
    pub id: String,
    /// The site that makes it: [`Provider::Flow`] or [`Provider::ChatGptWeb`].
    pub provider: Provider,
    /// What to make.
    pub request: Request,
}

/// What happens to a job, in the order it happens, on the receiver
/// [`Bridge::submit`] returns. A cancelled job emits nothing further.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The browser took the job from the queue.
    Taken,
    /// The page is under way.
    Progress {
        /// What it is doing, in a few words.
        message: String,
        /// How far, `0..=1`, when the page says.
        fraction: Option<f32>,
    },
    /// A file was saved; its path is inside the job's [`Bridge::out_dir`].
    File(PathBuf),
    /// The job finished, and every file it made has been announced.
    Done,
    /// The job failed, for the reason given.
    Failed(String),
}

/// The running bridge: a cheap handle, so clones share one server.
#[derive(Clone)]
pub struct Bridge {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a secret and stays out of every debug line.
        f.debug_struct("Bridge")
            .field("port", &self.shared.port)
            .finish_non_exhaustive()
    }
}

impl Bridge {
    /// Starts the server on `127.0.0.1:port` (0 picks a free port) for the
    /// extension that holds `token`, writing each job's files under
    /// `out_root`. An error says the port is in use when that is the cause.
    pub fn start(port: u16, token: String, out_root: PathBuf) -> Result<Bridge, String> {
        if token.is_empty() {
            return Err("the bridge needs a pairing token".to_string());
        }
        fs::create_dir_all(&out_root)
            .map_err(|error| format!("could not create {}: {error}", out_root.display()))?;
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|error| {
            if error.kind() == io::ErrorKind::AddrInUse {
                format!("port {port} is in use")
            } else {
                format!("could not listen on 127.0.0.1:{port}: {error}")
            }
        })?;
        let bound = listener
            .local_addr()
            .map_err(|error| format!("could not read the bridge's address: {error}"))?
            .port();
        // Non-blocking so the acceptor can notice `stop` between connections.
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("could not set up the bridge's listener: {error}"))?;
        let shared = Arc::new(Shared {
            token,
            out_root,
            port: bound,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            stopping: AtomicBool::new(false),
            handlers: AtomicUsize::new(0),
        });
        let acceptor = Arc::clone(&shared);
        thread::Builder::new()
            .name("concat-bridge".into())
            .spawn(move || accept(listener, &acceptor))
            .map_err(|error| format!("could not start the bridge: {error}"))?;
        Ok(Bridge { shared })
    }

    /// The port the server listens on: the one asked for, unless that was 0.
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// Stops the server. The acceptor exits within about 100 ms and releases
    /// the port; polls waiting for a job return at once.
    pub fn stop(&self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
    }

    /// Queues a job for `provider` and returns its id, with the receiver
    /// that hears how the job goes. A provider the bridge does not serve,
    /// or a kind it cannot make, is refused.
    pub fn submit(
        &self,
        provider: Provider,
        request: Request,
    ) -> Result<(String, Receiver<Event>), String> {
        if !provider.bridged() {
            return Err(format!(
                "{} is not served by the browser bridge",
                provider.id()
            ));
        }
        if !provider.kinds().contains(&request.kind) {
            return Err(format!(
                "{} cannot make a {}",
                provider.id(),
                request.kind.id()
            ));
        }
        let id = random_hex(16);
        let dir = out_dir_of(&self.shared, &id);
        fs::create_dir_all(&dir)
            .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        let (events, receiver) = mpsc::channel();
        let job = Job {
            id: id.clone(),
            provider,
            request,
        };
        {
            let mut guard = lock(&self.shared);
            guard.queue.push_back(id.clone());
            guard.jobs.insert(
                id.clone(),
                Record {
                    job,
                    phase: Phase::Queued,
                    events,
                    touched: Instant::now(),
                    saved: 0,
                },
            );
        }
        self.shared.changed.notify_all();
        Ok((id, receiver))
    }

    /// Cancels a job that is queued or taken. The extension learns of it from
    /// its state, and later posts for the job are refused. Nothing more is
    /// emitted on the job's receiver. An unknown or finished job is left be.
    pub fn cancel(&self, id: &str) {
        let mut guard = lock(&self.shared);
        let State { queue, jobs, .. } = &mut *guard;
        let live = jobs
            .get_mut(id)
            .filter(|record| matches!(record.phase, Phase::Queued | Phase::Taken));
        if let Some(record) = live {
            record.phase = Phase::Cancelled;
            record.touched = Instant::now();
        }
        queue.retain(|queued| queued.as_str() != id);
    }

    /// Whether `provider`'s page polled for a job within the last 40 seconds.
    pub fn connected(&self, provider: Provider) -> bool {
        let state = lock(&self.shared);
        state
            .polled
            .get(&provider)
            .is_some_and(|seen| seen.elapsed() <= CONNECTED_WITHIN)
    }

    /// The directory a job's files are written to. It exists once the job is
    /// submitted.
    pub fn out_dir(&self, id: &str) -> PathBuf {
        out_dir_of(&self.shared, id)
    }
}

/// Everything the acceptor and the connection threads share.
struct Shared {
    /// The pairing token. Never logged, never in an error.
    token: String,
    /// Where each job's directory goes.
    out_root: PathBuf,
    /// The port actually bound.
    port: u16,
    /// Jobs, the queue, and when each site last polled.
    state: Mutex<State>,
    /// Woken when a job is queued or cancelled, and on stop, so waiting polls
    /// re-check.
    changed: Condvar,
    /// Set by `stop`; the acceptor exits when it sees it.
    stopping: AtomicBool,
    /// Connections being served right now, capped at `MAX_HANDLERS`.
    handlers: AtomicUsize,
}

#[derive(Default)]
struct State {
    jobs: HashMap<String, Record>,
    /// Queued job ids, oldest first.
    queue: VecDeque<String>,
    /// When each provider's page last polled.
    polled: HashMap<Provider, Instant>,
}

struct Record {
    job: Job,
    phase: Phase,
    events: Sender<Event>,
    /// When the browser last spoke about this job; the stale check reads it.
    touched: Instant,
    /// Files saved so far; also the default index of the next one.
    saved: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Queued,
    Taken,
    Done,
    Failed,
    Cancelled,
}

impl Phase {
    /// The state name the protocol uses.
    fn name(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Taken => "taken",
            Phase::Done => "done",
            Phase::Failed => "failed",
            Phase::Cancelled => "cancelled",
        }
    }

    /// Whether the job has ended one way or another.
    fn finished(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::Cancelled)
    }
}

/// One connection's share of the cap; released when the connection ends.
struct Slot(Arc<Shared>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.handlers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Takes a share of the connection cap, or none when the cap is full.
fn claim(shared: &Arc<Shared>) -> Option<Slot> {
    if shared.handlers.fetch_add(1, Ordering::SeqCst) >= MAX_HANDLERS {
        shared.handlers.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    Some(Slot(Arc::clone(shared)))
}

/// Accepts connections until stopped, handing each to its own thread.
fn accept(listener: TcpListener, shared: &Arc<Shared>) {
    while !shared.stopping.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                // An accepted socket may inherit the listener's non-blocking mode.
                let _ = stream.set_nonblocking(false);
                match claim(shared) {
                    Some(slot) => {
                        // If the thread cannot start, the closure is dropped with
                        // the slot, and the share is released.
                        let _ = thread::Builder::new()
                            .name("concat-bridge-request".into())
                            .spawn(move || serve(&slot.0, stream));
                    }
                    None => refuse(stream),
                }
            }
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

/// Answers a connection that arrived when the cap was full.
fn refuse(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(DRAIN_TIMEOUT));
    let _ = write_reply(&mut stream, &Reply::error(503, "the bridge is busy"));
}

/// Serves one exchange on `stream`, then closes it.
fn serve(shared: &Shared, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut reader = BufReader::new(stream);
    let reply = match read_head(&mut reader) {
        Ok(head) => route(shared, &head, &mut reader),
        Err(reply) => reply,
    };
    let _ = write_reply(reader.get_mut(), &reply);
    // Half-close, then read off what the client still sends for a moment, so
    // closing with unread bytes does not reset the connection before the
    // client has read the answer.
    let _ = reader.get_mut().shutdown(Shutdown::Write);
    let _ = reader.get_mut().set_read_timeout(Some(DRAIN_TIMEOUT));
    let _ = io::copy(&mut (&mut reader).take(DRAIN_LIMIT), &mut io::sink());
}

/// A reply to send: a status, and a JSON body unless the status is 204.
struct Reply {
    status: u16,
    body: Option<Value>,
}

impl Reply {
    fn json(status: u16, body: Value) -> Reply {
        Reply {
            status,
            body: Some(body),
        }
    }

    fn none(status: u16) -> Reply {
        Reply { status, body: None }
    }

    fn error(status: u16, message: &str) -> Reply {
        Reply::json(status, json!({ "error": message }))
    }
}

/// The reply that says a job cannot take a post in its current state.
fn conflict(phase: Phase) -> Reply {
    Reply::error(
        409,
        match phase {
            Phase::Cancelled => "cancelled",
            Phase::Done | Phase::Failed => "finished",
            Phase::Queued => "not taken yet",
            Phase::Taken => "taken",
        },
    )
}

/// Writes `reply` with the CORS headers and `Connection: close`.
fn write_reply(stream: &mut TcpStream, reply: &Reply) -> io::Result<()> {
    let body = reply
        .body
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_default();
    let status = reply.status;
    let text = reason(status);
    let mut head = format!("HTTP/1.1 {status} {text}\r\n{CORS}Connection: close\r\n");
    if reply.body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// The reason phrase for the statuses this server answers with.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn bad_request(message: &str) -> Reply {
    Reply::error(400, message)
}

/// A request's line and headers. The body, if any, is still in the reader.
struct Head {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Head {
    /// The first header with this name, compared without regard to case.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Reads the request line and the headers, and nothing of the body.
fn read_head(reader: &mut dyn BufRead) -> Result<Head, Reply> {
    let mut line = next_line(reader)?;
    if line.is_empty() {
        // RFC 9112 lets a server skip one empty line before the request line.
        line = next_line(reader)?;
    }
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(bad_request("the request line is malformed"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(bad_request("only HTTP/1.x is spoken"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut size = line.len();
    let mut headers = Vec::new();
    loop {
        let header_line = next_line(reader)?;
        if header_line.is_empty() {
            break;
        }
        size += header_line.len();
        if size > MAX_HEAD || headers.len() >= MAX_HEADERS {
            return Err(Reply::error(431, "the request headers are too large"));
        }
        let Some((name, value)) = header_line.split_once(':') else {
            return Err(bad_request("a header line has no colon"));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(Head {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
    })
}

/// The next line of the head, without its line ending. A line that ends the
/// stream, or is too long, is a bad request.
fn next_line(reader: &mut dyn BufRead) -> Result<String, Reply> {
    read_line(reader)
        .map_err(|error| bad_request(&format!("could not read the request: {error}")))?
        .ok_or_else(|| bad_request("the request ended early"))
}

fn read_line(reader: &mut dyn BufRead) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    // One byte past the limit is read, so an overlong line is seen as one.
    let read = (&mut *reader)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_LINE || !bytes.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a request line is too long or not terminated",
        ));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Sends a request to its handler. The token is checked before any body is
/// read, so an unknown caller cannot make the server take in a large body.
fn route(shared: &Shared, head: &Head, body: &mut dyn BufRead) -> Reply {
    let method = head.method.as_str();
    if method == "OPTIONS" {
        return Reply::none(204);
    }
    if head.path == "/v1/hello" {
        return match method {
            "GET" => hello(),
            _ => Reply::error(405, "method not allowed"),
        };
    }
    if !authorized(shared, head) {
        return Reply::error(401, "unauthorized");
    }
    if head.path == "/v1/jobs/next" {
        return match method {
            "GET" => next_job(shared, &head.query),
            _ => Reply::error(405, "method not allowed"),
        };
    }
    let Some(rest) = head.path.strip_prefix("/v1/jobs/") else {
        return Reply::error(404, "no such route");
    };
    let (id, action) = match rest.split_once('/') {
        Some((id, action)) => (id, Some(action)),
        None => (rest, None),
    };
    if id.is_empty() {
        return Reply::error(404, "no such route");
    }
    match (action, method) {
        (None, "GET") => job_state(shared, id),
        (Some("progress"), "POST") => post_progress(shared, id, head, body),
        (Some("file"), "POST") => post_file(shared, id, head, body),
        (Some("done"), "POST") => post_done(shared, id),
        (Some("fail"), "POST") => post_fail(shared, id, head, body),
        (None | Some("progress" | "file" | "done" | "fail"), _) => {
            Reply::error(405, "method not allowed")
        }
        _ => Reply::error(404, "no such route"),
    }
}

fn authorized(shared: &Shared, head: &Head) -> bool {
    head.header("X-Concat-Token")
        .is_some_and(|given| same_secret(given.as_bytes(), shared.token.as_bytes()))
}

/// Compares two secrets in time that does not depend on where they differ.
fn same_secret(given: &[u8], expected: &[u8]) -> bool {
    let mut difference = u8::from(given.len() != expected.len());
    for (index, expected_byte) in expected.iter().enumerate() {
        let given_byte = given.get(index).copied().unwrap_or(0);
        difference |= given_byte ^ *expected_byte;
    }
    difference == 0
}

fn hello() -> Reply {
    Reply::json(
        200,
        json!({
            "app": "concat",
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": PROTOCOL,
        }),
    )
}

/// `GET /v1/jobs/next`: hands the oldest queued job for the site to the
/// caller, or answers 204 once `wait` seconds pass with none.
fn next_job(shared: &Shared, query: &str) -> Reply {
    let Some(provider) = query_value(query, "site")
        .and_then(Provider::from_id)
        .filter(|provider| provider.bridged())
    else {
        return bad_request("site must be flow or chatgpt-web");
    };
    let wait = match query_value(query, "wait") {
        None => 0,
        Some(text) => match text.parse::<u64>() {
            Ok(seconds) => seconds.min(MAX_WAIT_SECS),
            Err(_) => return bad_request("wait must be a whole number of seconds"),
        },
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let taken = {
        let mut guard = lock(shared);
        sweep(&mut guard, Instant::now());
        guard.polled.insert(provider, Instant::now());
        loop {
            let now = Instant::now();
            if let Some(job) = take_queued(&mut guard, provider, now) {
                break Some(job);
            }
            if now >= deadline || shared.stopping.load(Ordering::SeqCst) {
                break None;
            }
            guard = shared
                .changed
                .wait_timeout(guard, deadline - now)
                .map_or_else(|poisoned| poisoned.into_inner().0, |(guard, _)| guard);
        }
    };
    answer_taken(shared, taken)
}

/// Builds the answer for a job just taken, reading its reference picture
/// outside the lock. A reference that cannot be read fails the job, and the
/// poll is answered 204 so the extension asks again.
fn answer_taken(shared: &Shared, taken: Option<Job>) -> Reply {
    let Some(job) = taken else {
        return Reply::none(204);
    };
    match job_json(&job) {
        Ok(body) => Reply::json(200, body),
        Err(reason) => {
            fail_taken(shared, &job.id, reason);
            Reply::none(204)
        }
    }
}

/// The job as the extension's poll answer carries it.
fn job_json(job: &Job) -> Result<Value, String> {
    let request = &job.request;
    let reference = match &request.reference {
        None => Value::Null,
        Some(path) => {
            let bytes = fs::read(path).map_err(|error| {
                format!(
                    "could not read the reference picture {}: {error}",
                    path.display()
                )
            })?;
            json!({
                "name": path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "mediaType": media_type_of(path, &bytes),
                "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            })
        }
    };
    Ok(json!({
        "id": job.id,
        "site": job.provider.id(),
        "kind": request.kind.id(),
        "prompt": request.prompt,
        "model": request.model,
        "aspectRatio": request.aspect_ratio,
        "resolution": request.resolution,
        "duration": request.duration,
        "count": request.count(),
        "reference": reference,
    }))
}

/// The media type of a reference picture: by its extension when that names
/// one, else by its first bytes, else a generic type.
fn media_type_of(path: &Path, bytes: &[u8]) -> &'static str {
    let named = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .and_then(|ext| media_type_for(&ext));
    named
        .or_else(|| sniff(bytes).and_then(|(ext, _)| media_type_for(ext)))
        .unwrap_or("application/octet-stream")
}

fn media_type_for(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => return None,
    })
}

/// Moves the oldest queued job for `provider` to taken, and tells its receiver.
fn take_queued(state: &mut State, provider: Provider, now: Instant) -> Option<Job> {
    let State { queue, jobs, .. } = state;
    let position = queue.iter().position(|id| {
        jobs.get(id)
            .is_some_and(|record| record.job.provider == provider)
    })?;
    let id = queue.remove(position)?;
    let record = jobs.get_mut(&id)?;
    record.phase = Phase::Taken;
    record.touched = now;
    emit(record, Event::Taken);
    Some(record.job.clone())
}

/// `GET /v1/jobs/<id>`: the job's state, for the extension to check a cancel.
fn job_state(shared: &Shared, id: &str) -> Reply {
    let mut guard = lock(shared);
    let now = Instant::now();
    sweep(&mut guard, now);
    match guard.jobs.get_mut(id) {
        None => Reply::error(404, "no such job"),
        Some(record) => {
            record.touched = now;
            Reply::json(200, json!({ "state": record.phase.name() }))
        }
    }
}

/// `POST /v1/jobs/<id>/progress`: a progress note from the page.
fn post_progress(shared: &Shared, id: &str, head: &Head, body: &mut dyn BufRead) -> Reply {
    let value = match read_json(head, body) {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    let message = value["message"].as_str().unwrap_or_default().to_string();
    let fraction = value["fraction"]
        .as_f64()
        .filter(|fraction| fraction.is_finite())
        .map(|fraction| fraction.clamp(0.0, 1.0) as f32);
    with_taken(shared, id, |record| {
        emit(record, Event::Progress { message, fraction });
    })
}

/// `POST /v1/jobs/<id>/done`: the page has posted everything it made.
fn post_done(shared: &Shared, id: &str) -> Reply {
    with_taken(shared, id, |record| {
        if record.saved == 0 {
            record.phase = Phase::Failed;
            emit(
                record,
                Event::Failed("the page sent nothing back".to_string()),
            );
        } else {
            record.phase = Phase::Done;
            emit(record, Event::Done);
        }
    })
}

/// `POST /v1/jobs/<id>/fail`: the page could not make the job.
fn post_fail(shared: &Shared, id: &str, head: &Head, body: &mut dyn BufRead) -> Reply {
    let value = match read_json(head, body) {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    let reason = value["error"]
        .as_str()
        .filter(|text| !text.is_empty())
        .unwrap_or("the page failed")
        .to_string();
    with_taken(shared, id, |record| {
        record.phase = Phase::Failed;
        emit(record, Event::Failed(reason));
    })
}

/// `POST /v1/jobs/<id>/file`: saves the body as one of the job's files.
///
/// The body is written to a hidden part file first, so the lock is not held
/// while bytes come in, and the part is renamed to its final name, and the
/// job is told of it, under the lock once the job is seen still taken.
fn post_file(shared: &Shared, id: &str, head: &Head, body: &mut dyn BufRead) -> Reply {
    if let Err(reply) = check_taken(shared, id) {
        return reply;
    }
    let length = match body_length(head, MAX_FILE_BODY) {
        Ok(length) => length,
        Err(reply) => return reply,
    };
    if length == 0 {
        return bad_request("the file is empty");
    }
    let index = match head.header("X-Index") {
        None => None,
        Some(text) => match text.trim().parse::<u32>() {
            Ok(index) => Some(index),
            Err(_) => return bad_request("X-Index must be a whole number"),
        },
    };
    let dir = out_dir_of(shared, id);
    let part = dir.join(format!(".part-{}", random_hex(16)));
    if let Err(error) = write_part(body, length, &part) {
        let _ = fs::remove_file(&part);
        let status = if error.kind() == io::ErrorKind::UnexpectedEof {
            400
        } else {
            500
        };
        return Reply::error(status, &format!("could not save the file: {error}"));
    }
    let ext = head
        .header("Content-Type")
        .and_then(extension_for)
        .or_else(|| sniffed_extension(&part))
        .unwrap_or("bin");
    let stem = file_stem(head.header("X-File-Name"));
    let now = Instant::now();
    let mut guard = lock(shared);
    sweep(&mut guard, now);
    let Some(record) = guard.jobs.get_mut(id) else {
        let _ = fs::remove_file(&part);
        return Reply::error(404, "no such job");
    };
    if record.phase != Phase::Taken {
        let _ = fs::remove_file(&part);
        return conflict(record.phase);
    }
    let index = index.unwrap_or(record.saved + 1);
    let name = final_name(&dir, index, &stem, ext);
    let path = dir.join(&name);
    if let Err(error) = fs::rename(&part, &path) {
        let _ = fs::remove_file(&part);
        return Reply::error(500, &format!("could not save the file: {error}"));
    }
    record.saved += 1;
    record.touched = now;
    emit(record, Event::File(path));
    Reply::json(200, json!({ "saved": name }))
}

/// Refuses a post for a job that is not taken, before its body is read.
fn check_taken(shared: &Shared, id: &str) -> Result<(), Reply> {
    let mut guard = lock(shared);
    sweep(&mut guard, Instant::now());
    match guard.jobs.get(id) {
        None => Err(Reply::error(404, "no such job")),
        Some(record) if record.phase == Phase::Taken => Ok(()),
        Some(record) => Err(conflict(record.phase)),
    }
}

/// Writes exactly `length` bytes of `body` to `part`.
fn write_part(body: &mut dyn BufRead, length: u64, part: &Path) -> io::Result<()> {
    let mut file = File::create_new(part)?;
    let copied = io::copy(&mut (&mut *body).take(length), &mut file)?;
    if copied == length {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the body ended early",
        ))
    }
}

/// The extension a file's first bytes name, when they name one.
fn sniffed_extension(path: &Path) -> Option<&'static str> {
    let mut head = Vec::new();
    File::open(path)
        .ok()?
        .take(32)
        .read_to_end(&mut head)
        .ok()?;
    sniff(&head).map(|(ext, _)| ext)
}

/// The base name a posted file is saved under: its `X-File-Name` without the
/// directories and the extension, with anything a file system may refuse
/// made into `_`. A name with nothing usable left is random.
fn file_stem(name: Option<&str>) -> String {
    let base = name
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let stem = match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    };
    let clean: String = stem
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    if clean.trim_matches('_').is_empty() {
        random_hex(12)
    } else {
        clean
    }
}

/// `<index>-<stem>.<ext>`, with a random tag added if that name is taken.
fn final_name(dir: &Path, index: u32, stem: &str, ext: &str) -> String {
    let plain = format!("{index}-{stem}.{ext}");
    if dir.join(&plain).exists() {
        format!("{index}-{stem}-{}.{ext}", random_hex(6))
    } else {
        plain
    }
}

/// The body's declared length, refused when it is chunked or over `limit`.
fn body_length(head: &Head, limit: u64) -> Result<u64, Reply> {
    let chunked = head
        .header("Transfer-Encoding")
        .is_some_and(|coding| !coding.eq_ignore_ascii_case("identity"));
    if chunked {
        return Err(Reply::error(
            411,
            "send a Content-Length, not a chunked body",
        ));
    }
    let length = match head.header("Content-Length") {
        None => 0,
        Some(text) => text
            .parse::<u64>()
            .map_err(|_| bad_request("Content-Length is not a number"))?,
    };
    if length > limit {
        return Err(Reply::error(413, "the body is too large"));
    }
    Ok(length)
}

/// Reads a JSON body of up to 1 MiB. An empty body reads as `null`.
fn read_json(head: &Head, body: &mut dyn BufRead) -> Result<Value, Reply> {
    let length = body_length(head, MAX_JSON_BODY)?;
    let mut bytes = Vec::new();
    (&mut *body)
        .take(length)
        .read_to_end(&mut bytes)
        .map_err(|_| bad_request("the body ended early"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|_| bad_request("the body is not JSON"))
}

/// Runs `act` on job `id` while it is taken, and answers 204. For any other
/// state, answers the conflict or the 404.
fn with_taken(shared: &Shared, id: &str, act: impl FnOnce(&mut Record)) -> Reply {
    let mut guard = lock(shared);
    let now = Instant::now();
    sweep(&mut guard, now);
    match guard.jobs.get_mut(id) {
        None => Reply::error(404, "no such job"),
        Some(record) if record.phase == Phase::Taken => {
            record.touched = now;
            act(record);
            Reply::none(204)
        }
        Some(record) => conflict(record.phase),
    }
}

/// Fails a job the browser took but could not finish, if it is still taken.
fn fail_taken(shared: &Shared, id: &str, reason: String) {
    let mut guard = lock(shared);
    let taken = guard
        .jobs
        .get_mut(id)
        .filter(|record| record.phase == Phase::Taken);
    if let Some(record) = taken {
        record.phase = Phase::Failed;
        emit(record, Event::Failed(reason));
    }
}

/// Brings the job table up to date: a taken job silent for too long is
/// failed, and a finished one is forgotten after a while. Run on every
/// request, which is the lazy check the protocol calls for.
fn sweep(state: &mut State, now: Instant) {
    for record in state.jobs.values_mut() {
        if record.phase == Phase::Taken && now.duration_since(record.touched) > STALE_AFTER {
            record.phase = Phase::Failed;
            emit(
                record,
                Event::Failed("the browser stopped answering".to_string()),
            );
        }
    }
    state.jobs.retain(|_, record| {
        !(record.phase.finished() && now.duration_since(record.touched) > KEEP_FINISHED)
    });
}

/// Tells the job's receiver what happened. A receiver that has gone is no
/// reason to fail the request, so the send error is ignored.
fn emit(record: &Record, event: Event) {
    let _ = record.events.send(event);
}

/// The value of `key` in a query string such as `site=flow&wait=25`.
fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn out_dir_of(shared: &Shared, id: &str) -> PathBuf {
    shared.out_root.join(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::MediaKind;
    use base64::engine::general_purpose::STANDARD;
    use std::net::SocketAddr;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const AUTH: &[(&str, &str)] = &[("X-Concat-Token", TOKEN)];
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake picture";

    /// A test's view of one answer.
    struct Answer {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Answer {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).expect("a JSON body")
        }

        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    fn parse_answer(bytes: &[u8]) -> Answer {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a header block");
        let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status: u16 = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status code");
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            .collect();
        Answer {
            status,
            headers,
            body: bytes[split + 4..].to_vec(),
        }
    }

    /// Sends raw bytes and reads the whole answer. The client half-closes
    /// after sending, as a client that has nothing more to say does.
    fn exchange(port: u16, raw: &[u8]) -> Answer {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the bridge accepts");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("a read timeout");
        let _ = stream.write_all(raw);
        let _ = stream.shutdown(Shutdown::Write);
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        parse_answer(&bytes)
    }

    fn call(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
        let mut raw = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        raw.push_str("\r\n");
        let mut bytes = raw.into_bytes();
        bytes.extend_from_slice(body);
        exchange(port, &bytes)
    }

    /// Takes the oldest job for `site`, failing the test if none is there.
    fn take_job(port: u16, site: &str) -> Value {
        let answer = call(
            port,
            "GET",
            &format!("/v1/jobs/next?site={site}&wait=1"),
            AUTH,
            b"",
        );
        assert_eq!(answer.status, 200, "the browser should get a job");
        answer.json()
    }

    fn bridge_in(dir: &tempfile::TempDir) -> Bridge {
        Bridge::start(0, TOKEN.to_string(), dir.path().join("out")).expect("the bridge starts")
    }

    fn flow_image() -> Request {
        Request {
            kind: MediaKind::Image,
            prompt: "a red fox".into(),
            ..Default::default()
        }
    }

    fn flow_video() -> Request {
        Request {
            kind: MediaKind::Video,
            prompt: "a fox runs".into(),
            duration: 5,
            ..Default::default()
        }
    }

    #[test]
    fn hello_answers_without_a_token() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let answer = call(bridge.port(), "GET", "/v1/hello", &[], b"");
        assert_eq!(answer.status, 200);
        let body = answer.json();
        assert_eq!(body["app"], "concat");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["protocol"], 1);
    }

    #[test]
    fn wrong_or_missing_token_is_401() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let wrong = call(
            port,
            "GET",
            "/v1/jobs/next?site=flow&wait=0",
            &[("X-Concat-Token", "not-the-token")],
            b"",
        );
        assert_eq!(wrong.status, 401);
        assert_eq!(wrong.json()["error"], "unauthorized");
        let missing = call(port, "GET", "/v1/jobs/next?site=flow&wait=0", &[], b"");
        assert_eq!(missing.status, 401);
        assert_eq!(missing.json()["error"], "unauthorized");
    }

    #[test]
    fn options_preflight_carries_cors() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let answer = call(bridge.port(), "OPTIONS", "/v1/jobs/next", &[], b"");
        assert_eq!(answer.status, 204);
        assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
        assert_eq!(
            answer.header("access-control-allow-methods"),
            Some("GET, POST, OPTIONS")
        );
        let allowed = answer
            .header("access-control-allow-headers")
            .expect("allowed headers");
        for name in ["Content-Type", "X-Concat-Token", "X-File-Name", "X-Index"] {
            assert!(allowed.contains(name), "{name} is not allowed: {allowed}");
        }
        assert_eq!(answer.header("connection"), Some("close"));
    }

    #[test]
    fn submit_then_next_hands_over_the_job() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let request = Request {
            kind: MediaKind::Video,
            prompt: "a fox runs".into(),
            model: "google/veo-3.1".into(),
            aspect_ratio: "16:9".into(),
            resolution: "1080p".into(),
            duration: 5,
            count: 2,
            reference: None,
        };
        let (id, events) = bridge
            .submit(Provider::Flow, request)
            .expect("the job is queued");
        let job = take_job(bridge.port(), "flow");
        assert_eq!(job["id"], id.as_str());
        assert_eq!(job["site"], "flow");
        assert_eq!(job["kind"], "video");
        assert_eq!(job["prompt"], "a fox runs");
        assert_eq!(job["model"], "google/veo-3.1");
        assert_eq!(job["aspectRatio"], "16:9");
        assert_eq!(job["resolution"], "1080p");
        assert_eq!(job["duration"], 5);
        assert_eq!(job["count"], 2);
        assert!(job["reference"].is_null());
        assert_eq!(
            events.recv_timeout(Duration::from_secs(1)),
            Ok(Event::Taken)
        );
    }

    #[test]
    fn next_without_a_job_is_204_after_the_wait() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let started = Instant::now();
        let answer = call(
            bridge.port(),
            "GET",
            "/v1/jobs/next?site=flow&wait=1",
            AUTH,
            b"",
        );
        assert_eq!(answer.status, 204);
        assert!(answer.body.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(900));
    }

    #[test]
    fn next_issued_before_submit_wakes_up_on_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let waiter = thread::spawn(move || {
            let started = Instant::now();
            let answer = call(port, "GET", "/v1/jobs/next?site=flow&wait=10", AUTH, b"");
            (started.elapsed(), answer)
        });
        thread::sleep(Duration::from_millis(300));
        let (id, _events) = bridge
            .submit(Provider::Flow, flow_image())
            .expect("the job is queued");
        let (elapsed, answer) = waiter.join().expect("the waiting poll finishes");
        assert_eq!(answer.status, 200);
        assert_eq!(answer.json()["id"], id.as_str());
        assert!(elapsed < Duration::from_secs(5), "woke after {elapsed:?}");
    }

    #[test]
    fn file_is_saved_and_announced() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let (id, events) = bridge
            .submit(Provider::Flow, flow_video())
            .expect("the job is queued");
        take_job(port, "flow");
        let out = bridge.out_dir(&id);

        // The name is cleaned of directories and of characters a file system
        // may refuse; the extension comes from the Content-Type.
        let first = call(
            port,
            "POST",
            &format!("/v1/jobs/{id}/file"),
            &[
                ("X-Concat-Token", TOKEN),
                ("Content-Type", "video/mp4"),
                ("X-File-Name", "../../my clip.mp4"),
            ],
            b"mp4 bytes",
        );
        assert_eq!(
            first.status,
            200,
            "{}",
            String::from_utf8_lossy(&first.body)
        );
        assert_eq!(first.json()["saved"], "1-my_clip.mp4");
        assert_eq!(
            fs::read(out.join("1-my_clip.mp4")).expect("the file is saved"),
            b"mp4 bytes"
        );

        // An X-Index names the file; with no Content-Type the bytes are sniffed.
        let indexed = call(
            port,
            "POST",
            &format!("/v1/jobs/{id}/file"),
            &[("X-Concat-Token", TOKEN), ("X-Index", "7")],
            PNG,
        );
        assert_eq!(indexed.status, 200);
        let indexed_name = indexed.json()["saved"]
            .as_str()
            .expect("a name")
            .to_string();
        assert!(
            indexed_name.starts_with("7-") && indexed_name.ends_with(".png"),
            "{indexed_name}"
        );

        // Without an index the next one follows the count of files saved.
        let third = call(
            port,
            "POST",
            &format!("/v1/jobs/{id}/file"),
            &[("X-Concat-Token", TOKEN)],
            PNG,
        );
        assert_eq!(third.status, 200);
        let third_name = third.json()["saved"].as_str().expect("a name").to_string();
        assert!(
            third_name.starts_with("3-") && third_name.ends_with(".png"),
            "{third_name}"
        );

        let received: Vec<Event> = events.try_iter().collect();
        assert_eq!(
            received,
            [
                Event::Taken,
                Event::File(out.join("1-my_clip.mp4")),
                Event::File(out.join(&indexed_name)),
                Event::File(out.join(&third_name)),
            ]
        );
    }

    #[test]
    fn progress_and_done_are_announced() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let (id, events) = bridge
            .submit(Provider::Flow, flow_image())
            .expect("the job is queued");
        take_job(port, "flow");

        let progress = call(
            port,
            "POST",
            &format!("/v1/jobs/{id}/progress"),
            AUTH,
            br#"{"message":"rendering","fraction":0.25}"#,
        );
        assert_eq!(progress.status, 204);
        let unknown = call(
            port,
            "POST",
            &format!("/v1/jobs/{id}/progress"),
            AUTH,
            br#"{"message":"queued","fraction":null}"#,
        );
        assert_eq!(unknown.status, 204);

        let file = call(
            port,
            "POST",
            &format!("/v1/jobs/{id}/file"),
            &[("X-Concat-Token", TOKEN), ("Content-Type", "image/png")],
            PNG,
        );
        assert_eq!(file.status, 200);
        let saved = file.json()["saved"].as_str().expect("a name").to_string();

        let done = call(port, "POST", &format!("/v1/jobs/{id}/done"), AUTH, b"");
        assert_eq!(done.status, 204);
        let late = call(port, "POST", &format!("/v1/jobs/{id}/file"), AUTH, PNG);
        assert_eq!(late.status, 409);
        assert_eq!(late.json()["error"], "finished");
        let state = call(port, "GET", &format!("/v1/jobs/{id}"), AUTH, b"");
        assert_eq!(state.json()["state"], "done");

        let received: Vec<Event> = events.try_iter().collect();
        assert_eq!(
            received,
            [
                Event::Taken,
                Event::Progress {
                    message: "rendering".into(),
                    fraction: Some(0.25),
                },
                Event::Progress {
                    message: "queued".into(),
                    fraction: None,
                },
                Event::File(bridge.out_dir(&id).join(saved)),
                Event::Done,
            ]
        );
    }

    #[test]
    fn done_without_files_fails() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let (id, events) = bridge
            .submit(Provider::Flow, flow_image())
            .expect("the job is queued");
        take_job(port, "flow");
        let done = call(port, "POST", &format!("/v1/jobs/{id}/done"), AUTH, b"");
        assert_eq!(done.status, 204);
        let received: Vec<Event> = events.try_iter().collect();
        assert_eq!(
            received,
            [
                Event::Taken,
                Event::Failed("the page sent nothing back".into()),
            ]
        );
        let state = call(port, "GET", &format!("/v1/jobs/{id}"), AUTH, b"");
        assert_eq!(state.json()["state"], "failed");
    }

    #[test]
    fn fail_reports_the_reason() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let (id, events) = bridge
            .submit(Provider::Flow, flow_image())
            .expect("the job is queued");
        take_job(port, "flow");
        let fail = call(
            port,
            "POST",
            &format!("/v1/jobs/{id}/fail"),
            AUTH,
            br#"{"error":"Flow refused the prompt"}"#,
        );
        assert_eq!(fail.status, 204);
        let received: Vec<Event> = events.try_iter().collect();
        assert_eq!(
            received,
            [
                Event::Taken,
                Event::Failed("Flow refused the prompt".into())
            ]
        );
    }

    #[test]
    fn cancel_stops_the_job() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let (id, events) = bridge
            .submit(Provider::Flow, flow_image())
            .expect("the job is queued");
        take_job(port, "flow");
        bridge.cancel(&id);

        let state = call(port, "GET", &format!("/v1/jobs/{id}"), AUTH, b"");
        assert_eq!(state.json()["state"], "cancelled");
        let file = call(port, "POST", &format!("/v1/jobs/{id}/file"), AUTH, PNG);
        assert_eq!(file.status, 409);
        assert_eq!(file.json()["error"], "cancelled");

        // A queued job that is cancelled is never handed out, and says nothing.
        let (queued_id, queued_events) = bridge
            .submit(Provider::Flow, flow_image())
            .expect("the job is queued");
        bridge.cancel(&queued_id);
        let answer = call(port, "GET", "/v1/jobs/next?site=flow&wait=0", AUTH, b"");
        assert_eq!(answer.status, 204);
        assert!(queued_events.try_recv().is_err());

        let received: Vec<Event> = events.try_iter().collect();
        assert_eq!(received, [Event::Taken]);
    }

    #[test]
    fn connected_tracks_the_polls() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        assert!(!bridge.connected(Provider::Flow));
        let answer = call(
            bridge.port(),
            "GET",
            "/v1/jobs/next?site=flow&wait=0",
            AUTH,
            b"",
        );
        assert_eq!(answer.status, 204);
        assert!(bridge.connected(Provider::Flow));
        assert!(!bridge.connected(Provider::ChatGptWeb));
    }

    #[test]
    fn bridged_providers_only() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        assert!(bridge.submit(Provider::OpenRouter, flow_image()).is_err());
        assert!(bridge.submit(Provider::ChatGpt, flow_image()).is_err());
        // The web page makes pictures only.
        assert!(bridge.submit(Provider::ChatGptWeb, flow_video()).is_err());
        assert!(bridge.submit(Provider::ChatGptWeb, flow_image()).is_ok());
    }

    #[test]
    fn reference_is_embedded_as_base64() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let reference = dir.path().join("start.png");
        fs::write(&reference, PNG).expect("the reference is written");
        let request = Request {
            kind: MediaKind::Image,
            prompt: "edit it".into(),
            reference: Some(reference),
            ..Default::default()
        };
        let (id, _events) = bridge
            .submit(Provider::ChatGptWeb, request)
            .expect("the job is queued");
        let job = take_job(bridge.port(), "chatgpt-web");
        assert_eq!(job["id"], id.as_str());
        assert_eq!(job["reference"]["name"], "start.png");
        assert_eq!(job["reference"]["mediaType"], "image/png");
        let encoded = job["reference"]["base64"]
            .as_str()
            .expect("the picture is encoded");
        assert_eq!(STANDARD.decode(encoded).expect("valid base64"), PNG);
    }

    #[test]
    fn port_in_use_is_a_clear_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let holder = TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = holder.local_addr().expect("an address").port();
        let Err(message) = Bridge::start(port, TOKEN.to_string(), dir.path().join("out")) else {
            panic!("the port was free while it was held");
        };
        assert_eq!(message, format!("port {port} is in use"));
    }

    #[test]
    fn stop_ends_the_acceptor() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let address = SocketAddr::from(([127, 0, 0, 1], bridge.port()));
        bridge.stop();
        let mut closed = false;
        for _ in 0..50 {
            thread::sleep(Duration::from_millis(100));
            if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err() {
                closed = true;
                break;
            }
        }
        assert!(closed, "the acceptor kept listening after stop");
    }

    #[test]
    fn oversized_file_is_413_and_chunked_is_411() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let (id, _events) = bridge
            .submit(Provider::Flow, flow_video())
            .expect("the job is queued");
        take_job(port, "flow");

        // Only the head is sent: the length alone is refused, before any body.
        let too_large = format!(
            "POST /v1/jobs/{id}/file HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Concat-Token: {TOKEN}\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\n\r\n",
            MAX_FILE_BODY + 1
        );
        assert_eq!(exchange(port, too_large.as_bytes()).status, 413);

        let chunked = format!(
            "POST /v1/jobs/{id}/file HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Concat-Token: {TOKEN}\r\nTransfer-Encoding: chunked\r\n\r\n"
        );
        assert_eq!(exchange(port, chunked.as_bytes()).status, 411);
    }

    #[test]
    fn stale_taken_job_fails() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let (id, events) = bridge
            .submit(Provider::Flow, flow_image())
            .expect("the job is queued");
        take_job(bridge.port(), "flow");
        {
            let mut guard = lock(&bridge.shared);
            sweep(
                &mut guard,
                Instant::now() + STALE_AFTER + Duration::from_secs(1),
            );
        }
        let received: Vec<Event> = events.try_iter().collect();
        assert_eq!(
            received,
            [
                Event::Taken,
                Event::Failed("the browser stopped answering".into()),
            ]
        );
        let state = call(bridge.port(), "GET", &format!("/v1/jobs/{id}"), AUTH, b"");
        assert_eq!(state.json()["state"], "failed");
    }

    #[test]
    fn unknown_job_and_route_are_404() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let bridge = bridge_in(&dir);
        let port = bridge.port();
        let job = call(port, "GET", "/v1/jobs/deadbeef", AUTH, b"");
        assert_eq!(job.status, 404);
        let route = call(port, "GET", "/v1/nowhere", AUTH, b"");
        assert_eq!(route.status, 404);
        let post = call(port, "POST", "/v1/jobs/deadbeef/file", AUTH, PNG);
        assert_eq!(post.status, 404);
        assert_eq!(post.json()["error"], "no such job");
    }

    #[test]
    fn file_names_are_cleaned() {
        assert_eq!(file_stem(Some("../../my clip.mp4")), "my_clip");
        assert_eq!(file_stem(Some("archive.tar.gz")), "archive_tar");
        assert_eq!(file_stem(Some("..")).len(), 12);
        assert_eq!(file_stem(None).len(), 12);
    }

    #[test]
    fn secrets_compare_in_full() {
        assert!(same_secret(b"abc", b"abc"));
        assert!(!same_secret(b"abd", b"abc"));
        assert!(!same_secret(b"ab", b"abc"));
        assert!(!same_secret(b"abcd", b"abc"));
    }

    #[test]
    fn bridge_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Bridge>();
    }
}
