// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The asset library, kept in a second place: a folder in the person's own
//! Google Drive.
//!
//! The local library stays the one the editor reads. This module moves
//! files between it and a folder Concat makes in the person's Drive, in
//! both directions, so a second computer, or a fresh install, finds the
//! pictures and videos it made. Every file goes up carrying its library
//! metadata as Drive `appProperties`, so the remote folder can be read
//! back into a library without an index file of its own.
//!
//! Concat ships no Google client. The person makes an OAuth client of type
//! "Desktop app" in a Google Cloud project of their own and pastes its id
//! and secret in. Sign-in is the installed-app flow: a PKCE challenge, and
//! a redirect to a port on this machine's loopback that waits for one
//! answer. The scope is `drive.file`, which sees only the files Concat
//! itself created, so nothing else in the person's Drive is readable by it.
//!
//! The calls are blocking, as the rest of the host's network work is: the
//! caller picks the thread. Errors are human-readable strings, and none of
//! them carries a token, a secret, the PKCE verifier or an upload session.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::assets::{Asset, AssetLibrary, sha256_of};
use crate::dirs::AppDirs;
use crate::generate::{MediaKind, sniff};

/// The folder Concat keeps the library in, at the top of the person's Drive.
pub const FOLDER: &str = "Concat Library";

/// The one scope asked for: the files Concat itself creates, and no others.
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
/// Google's consent page, where the browser is sent to say yes.
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's token endpoint: the code exchange, and the renewals.
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The API root; the `/drive/v3/...` paths are added to it.
const API_URL: &str = "https://www.googleapis.com";
/// The resumable upload root, `.../upload/drive/v3`.
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3";
/// The file the sign-in is saved in, beside the settings.
const TOKEN_FILE: &str = "google-drive.json";
/// How long the loopback waits for the browser to come back.
const SIGN_IN_WAIT: Duration = Duration::from_secs(5 * 60);
/// An access token this close to its end is renewed before it is used.
const RENEW_WITHIN: u64 = 60;
/// What one upload request carries: 8 MiB, a multiple of the 256 KiB Drive asks for.
const CHUNK: usize = 8 * 1024 * 1024;
/// Drive's limit on one `appProperties` entry's key and value together, in bytes.
const PROPERTY_LIMIT: usize = 124;
/// The pauses between tries when an upload chunk fails in a way worth retrying.
const UPLOAD_RETRIES: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// Upload answers in a row that keep no new bytes before the upload is given up.
const MAX_STALLS: usize = 5;
/// Pages of a folder listing read before the listing is called too long.
const MAX_PAGES: usize = 1000;
/// The name a network failure is reported under, for the messages.
const DRIVE: &str = "Google Drive";
/// The name the sign-in is reported under, for the messages.
const SIGN_IN: &str = "Google's sign-in";
/// The error text of a call that the person cancelled.
const CANCELLED: &str = "cancelled";
/// The media type a Drive folder has.
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// The `appProperties` keys that carry an asset's library metadata.
const PROP_ASSET: &str = "concatAssetId";
const PROP_SHA: &str = "concatSha256";
const PROP_KIND: &str = "concatKind";
const PROP_NAME: &str = "concatName";
const PROP_PROMPT: &str = "concatPrompt";
const PROP_PROVIDER: &str = "concatProvider";
const PROP_MODEL: &str = "concatModel";
const PROP_ADDED: &str = "concatAdded";

/// Where the calls go. The defaults are Google's; tests point them at a
/// local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// The consent page the browser is sent to.
    pub auth: String,
    /// The token endpoint, for the code exchange and for renewals.
    pub token: String,
    /// The API root; `/drive/v3/...` is added to it.
    pub api: String,
    /// The root of resumable uploads; `/files` is added to it.
    pub upload: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            auth: AUTH_URL.to_owned(),
            token: TOKEN_URL.to_owned(),
            api: API_URL.to_owned(),
            upload: UPLOAD_URL.to_owned(),
        }
    }
}

/// The OAuth client the person made in their own Google Cloud project.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Client {
    /// The client id, as Google's console shows it.
    pub client_id: String,
    /// The client secret. Google wants one even for a desktop client, where
    /// it is not secret in the strict sense; it is kept as one all the same.
    pub client_secret: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("client_id", &self.client_id)
            .field("client_secret", &Hidden)
            .finish()
    }
}

/// The sign-in: what every call to Google Drive carries, and what renews it.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    /// The bearer token every call carries. Never shown or logged.
    pub access_token: String,
    /// What renews the access token. Empty when there is none to renew with.
    pub refresh_token: String,
    /// When `access_token` stops working, in seconds since the Unix epoch.
    pub expires_at: u64,
    /// The account signed in, for the settings page to show.
    #[serde(default)]
    pub email: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &Hidden)
            .field("refresh_token", &Hidden)
            .field("expires_at", &self.expires_at)
            .field("email", &self.email)
            .finish()
    }
}

/// Stands in for a secret wherever a `Debug` output would show one.
struct Hidden;

impl std::fmt::Debug for Hidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// One file in the Drive folder, as the listing describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteFile {
    /// Drive's id for the file.
    pub id: String,
    /// The name it was uploaded under.
    pub name: String,
    /// Its media type, as Drive recorded it.
    pub mime_type: String,
    /// Its size in bytes; zero when Drive does not say.
    pub size: u64,
    /// Its MD5 as Drive computed it; empty when Drive has none.
    pub md5: String,
    /// The `appProperties` on it: for a file Concat made, the asset's
    /// library metadata; see [`sync`].
    pub app_properties: BTreeMap<String, String>,
}

/// What one [`sync`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncReport {
    /// Local assets that went up to Drive.
    pub uploaded: usize,
    /// Drive files that came down into the library.
    pub downloaded: usize,
    /// One line per item that failed, naming it. The rest of the sync went on.
    pub failed: Vec<String>,
}

/// The saved sign-in, if there is one and it reads.
pub fn load_tokens(dirs: &AppDirs) -> Option<Tokens> {
    let text = std::fs::read_to_string(token_path(dirs)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Saves the sign-in beside the settings. The file is written under another
/// name and renamed over the old one, so a crash leaves the old sign-in or
/// the new one and never half of either. On unix only the person may read it.
pub fn save_tokens(dirs: &AppDirs, tokens: &Tokens) -> Result<(), String> {
    let path = token_path(dirs);
    std::fs::create_dir_all(&dirs.config)
        .map_err(|error| format!("could not make {}: {error}", dirs.config.display()))?;
    let text = serde_json::to_vec_pretty(tokens)
        .map_err(|error| format!("could not write the sign-in: {error}"))?;
    let partial = path.with_extension("json.partial");
    write_private(&partial, &text)?;
    std::fs::rename(&partial, &path).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        format!("could not save the sign-in: {error}")
    })
}

/// Forgets the sign-in by deleting the saved tokens. Google is not asked to
/// revoke them; a person who wants that done does it from their Google
/// account's page. A sign-in that was never saved is forgotten already.
pub fn forget(dirs: &AppDirs) -> Result<(), String> {
    match std::fs::remove_file(token_path(dirs)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not forget the sign-in: {error}")),
    }
}

/// Signs the person in: opens Google's consent page through `open_url`,
/// waits up to five minutes for the browser to come back to the loopback,
/// and exchanges the code it brings for tokens.
///
/// `open_url` is given the full consent URL and is expected to hand it to
/// the browser; an `Err` from it stops the sign-in. Setting `cancel` stops
/// the wait and returns `Err("cancelled")`. A browser answer whose `state`
/// is not the one this sign-in sent is refused. The account's address is
/// looked up once, and left empty if that fails.
pub fn authorize(
    client: &Client,
    endpoints: &Endpoints,
    open_url: &dyn Fn(&str) -> Result<(), String>,
    cancel: &AtomicBool,
) -> Result<Tokens, String> {
    if client.client_id.trim().is_empty() {
        return Err("Enter the client id of your Google OAuth client first".to_owned());
    }
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| {
        format!("could not listen on this computer for Google's answer: {error}")
    })?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("could not listen for Google's answer: {error}"))?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("could not listen for Google's answer: {error}"))?;
    let redirect = format!("http://127.0.0.1:{port}");
    let (verifier, challenge) = pkce()?;
    let state = crate::generate::random_hex(32);
    let url = auth_url(endpoints, client, &redirect, &challenge, &state);
    open_url(&url)
        .map_err(|error| format!("could not open the browser for Google sign-in: {error}"))?;

    let deadline = Instant::now() + SIGN_IN_WAIT;
    let code = loop {
        check_cancel(cancel)?;
        if Instant::now() >= deadline {
            return Err("Google sign-in was not finished in five minutes; try again".to_owned());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = read_redirect(stream, &state)? {
                    break code;
                }
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("the sign-in listener failed: {error}")),
        }
    };

    let answer = post_token(
        endpoints,
        &[
            ("code", code.as_str()),
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ],
    )?;
    let mut tokens = tokens_from(answer, String::new());
    if tokens.refresh_token.is_empty() {
        return Err("Google sent no renewal token; sign in again".to_owned());
    }
    tokens.email = account_email(endpoints, &tokens).unwrap_or_default();
    Ok(tokens)
}

/// Makes the access token good for use: when it expires within a minute, or
/// is missing, it is renewed with the refresh token. Returns whether
/// `tokens` changed, so the caller knows to save them again; a sync renews
/// the tokens it is given, so the caller should save them whatever the
/// outcome of the sync.
pub fn fresh(client: &Client, endpoints: &Endpoints, tokens: &mut Tokens) -> Result<bool, String> {
    if !tokens.access_token.is_empty() && tokens.expires_at > now().saturating_add(RENEW_WITHIN) {
        return Ok(false);
    }
    if tokens.refresh_token.is_empty() {
        return Err("The Google Drive sign-in has ended; sign in again".to_owned());
    }
    let answer = post_token(
        endpoints,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens.refresh_token.as_str()),
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
        ],
    )?;
    let email = std::mem::take(&mut tokens.email);
    *tokens = tokens_from(answer, tokens.refresh_token.clone());
    tokens.email = email;
    Ok(true)
}

/// The id of the folder called `name` at the top of the Drive, made if it
/// is not there. With `drive.file` only folders Concat made are visible, so
/// the first match is the one.
pub fn ensure_folder(endpoints: &Endpoints, tokens: &Tokens, name: &str) -> Result<String, String> {
    let query = format!(
        "mimeType='{FOLDER_MIME}' and name='{}' and trashed=false",
        drive_literal(name)
    );
    let url = with_query(
        &format!("{}/drive/v3/files", endpoints.api),
        &[
            ("q", query.as_str()),
            ("fields", "files(id,name)"),
            ("spaces", "drive"),
        ],
    );
    let listing: FileList = get_json(tokens, &url)?;
    if let Some(found) = listing.files.into_iter().find(|file| !file.id.is_empty()) {
        return Ok(found.id);
    }
    let body = serde_json::json!({ "name": name, "mimeType": FOLDER_MIME });
    let created: WireFile = post_json(tokens, &format!("{}/drive/v3/files", endpoints.api), &body)?;
    if created.id.is_empty() {
        return Err("Google Drive made the folder without an id".to_owned());
    }
    Ok(created.id)
}

/// Every file directly in the folder `folder_id`, reading all the pages.
/// Files in the Drive's trash are not listed.
pub fn list(
    endpoints: &Endpoints,
    tokens: &Tokens,
    folder_id: &str,
) -> Result<Vec<RemoteFile>, String> {
    let query = format!(
        "'{}' in parents and trashed=false",
        drive_literal(folder_id)
    );
    let base = format!("{}/drive/v3/files", endpoints.api);
    let fields = "nextPageToken,files(id,name,mimeType,size,md5Checksum,appProperties)";
    let mut files = Vec::new();
    let mut page: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut params = vec![
            ("q", query.as_str()),
            ("fields", fields),
            ("spaces", "drive"),
            ("pageSize", "1000"),
        ];
        if let Some(token) = page.as_deref() {
            params.push(("pageToken", token));
        }
        let listing: FileList = get_json(tokens, &with_query(&base, &params))?;
        files.extend(
            listing
                .files
                .into_iter()
                .filter(|file| !file.id.is_empty())
                .map(remote_file),
        );
        match listing.next_page_token {
            Some(next) if !next.is_empty() => page = Some(next),
            _ => return Ok(files),
        }
    }
    Err("the Google Drive folder has more pages than Concat reads".to_owned())
}

/// Uploads the file at `path` into `folder_id` as `name`, with `props` as
/// its `appProperties`, and returns the id Drive gave it.
///
/// A resumable upload in chunks of 8 MiB. `progress` is given the fraction
/// sent, `0..=1`. A chunk that fails with a dropped connection or a busy
/// server is asked again after a short pause, after Google has said how much
/// it kept, so the file is not sent twice from the start. Setting `cancel`
/// returns `Err("cancelled")`, and the session is left for Google to expire.
pub fn upload(
    endpoints: &Endpoints,
    tokens: &Tokens,
    folder_id: &str,
    path: &Path,
    name: &str,
    mime: &str,
    props: &BTreeMap<String, String>,
    progress: &dyn Fn(f32),
    cancel: &AtomicBool,
) -> Result<String, String> {
    check_cancel(cancel)?;
    let total = std::fs::metadata(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?
        .len();
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let agent = agent();
    let metadata = serde_json::json!({
        "name": name,
        "parents": [folder_id],
        "appProperties": props,
    });
    let opened = agent
        .post(&with_query(
            &format!("{}/files", endpoints.upload),
            &[("uploadType", "resumable")],
        ))
        .set("Authorization", &bearer(tokens))
        .set("Content-Type", "application/json; charset=UTF-8")
        .set("X-Upload-Content-Type", mime)
        .set("X-Upload-Content-Length", &total.to_string())
        .send_string(&metadata.to_string())
        .map_err(|error| describe(DRIVE, error))?;
    let session = opened
        .header("Location")
        .map(str::to_owned)
        .ok_or_else(|| "Google Drive did not open an upload for the file".to_owned())?;

    let size = CHUNK.min(usize::try_from(total).unwrap_or(CHUNK));
    let mut buffer = vec![0u8; size];
    let mut offset = 0u64;
    let mut failures = 0usize;
    let mut stalls = 0usize;
    loop {
        check_cancel(cancel)?;
        match send_chunk(&agent, &session, &mut file, &mut buffer, offset, total) {
            Ok(Step::Done(id)) => {
                progress(1.0);
                return Ok(id);
            }
            Ok(Step::Resume(next)) => {
                stalls = if next > offset { 0 } else { stalls + 1 };
                if stalls > MAX_STALLS {
                    return Err("Google Drive kept none of the upload".to_owned());
                }
                offset = next;
                failures = 0;
            }
            Err(Trouble::Stop(message)) => return Err(message),
            Err(Trouble::Retry(message)) => {
                if failures == UPLOAD_RETRIES.len() {
                    return Err(message);
                }
                pause(UPLOAD_RETRIES[failures], cancel)?;
                failures += 1;
                match probe(&agent, &session, total) {
                    Ok(Step::Done(id)) => {
                        progress(1.0);
                        return Ok(id);
                    }
                    Ok(Step::Resume(next)) => offset = next,
                    Err(Trouble::Stop(message)) => return Err(message),
                    Err(Trouble::Retry(_)) => {}
                }
            }
        }
        progress(fraction(offset, total));
    }
}

/// Downloads the Drive file `file_id` to `to`. The bytes are written to a
/// `.partial` file beside it, renamed into place once they are all there,
/// and the partial file is removed when the download fails or is cancelled.
pub fn download(
    endpoints: &Endpoints,
    tokens: &Tokens,
    file_id: &str,
    to: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let url = with_query(
        &format!("{}/drive/v3/files/{}", endpoints.api, encode(file_id)),
        &[("alt", "media")],
    );
    let response = agent()
        .get(&url)
        .set("Authorization", &bearer(tokens))
        .call()
        .map_err(|error| describe(DRIVE, error))?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    }
    let partial = partial_of(to);
    let outcome = copy_to(response.into_reader(), &partial, cancel).and_then(|()| {
        std::fs::rename(&partial, to)
            .map_err(|error| format!("could not save {}: {error}", to.display()))
    });
    if outcome.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    outcome
}

/// Brings the library and the Drive folder into step, in both directions.
///
/// Renews the access token if it is due ([`fresh`]), finds or makes the
/// [`FOLDER`], and lists it. Then, for each local asset Drive does not
/// hold (no `drive_id`, or one the folder no longer has): if the folder
/// already has a file for it, by asset id or by SHA-256, its id is
/// recorded; otherwise the file goes up with the asset's metadata as
/// `appProperties` and its id is recorded. Then each Concat file in the
/// folder that the library does not hold comes down into `staging`, is
/// checked against its SHA-256, and joins the library under the id it came
/// with.
///
/// A failure on one file is listed in [`SyncReport::failed`] by its name and
/// the sync goes on. A failed sign-in, folder or listing ends the sync with
/// `Err`. Setting `cancel` ends it with `Err("cancelled")`; what was
/// recorded before then stays in `library`, so the caller saves the library
/// whatever the outcome. `progress` gets a line of text and a fraction in
/// `0..=1`.
pub fn sync(
    client: &Client,
    endpoints: &Endpoints,
    tokens: &mut Tokens,
    library: &mut AssetLibrary,
    staging: &Path,
    progress: &dyn Fn(String, f32),
    cancel: &AtomicBool,
) -> Result<SyncReport, String> {
    check_cancel(cancel)?;
    fresh(client, endpoints, tokens)?;
    progress("Finding the Concat folder in Google Drive".to_owned(), 0.0);
    let folder = ensure_folder(endpoints, tokens, FOLDER)?;
    let remote = list(endpoints, tokens, &folder)?;

    let to_send: Vec<Asset> = library
        .assets
        .iter()
        .filter(|asset| !held_remotely(&remote, asset))
        .cloned()
        .collect();
    let to_fetch: Vec<&RemoteFile> = {
        let held: &AssetLibrary = library;
        remote
            .iter()
            .filter(|file| file.app_properties.contains_key(PROP_ASSET))
            .filter(|file| !present(held, file))
            .collect()
    };
    let steps = (to_send.len() + to_fetch.len()).max(1) as f32;
    let mut report = SyncReport::default();
    let mut done = 0f32;

    for asset in to_send {
        check_cancel(cancel)?;
        fresh(client, endpoints, tokens)?;
        let label = if asset.name.is_empty() {
            asset.file.clone()
        } else {
            asset.name.clone()
        };
        if let Some(existing) = remote.iter().find(|file| matches_asset(file, &asset)) {
            if let Err(error) = library.set_drive_id(&asset.id, Some(existing.id.clone())) {
                report.failed.push(format!("{label}: {error}"));
            }
            done += 1.0;
            continue;
        }
        let path = library.path_of(&asset);
        let mime = mime_of(&asset.file);
        let props = properties(&asset);
        let base = done;
        let sending =
            |fraction: f32| progress(format!("Uploading {label}"), (base + fraction) / steps);
        let outcome = upload(
            endpoints,
            tokens,
            &folder,
            &path,
            &asset.file,
            mime,
            &props,
            &sending,
            cancel,
        );
        match outcome {
            Ok(id) => match library.set_drive_id(&asset.id, Some(id)) {
                Ok(()) => report.uploaded += 1,
                Err(error) => report.failed.push(format!("{label}: {error}")),
            },
            Err(error) if error == CANCELLED => return Err(CANCELLED.to_owned()),
            Err(error) => report.failed.push(format!("{label}: {error}")),
        }
        done += 1.0;
    }

    for file in to_fetch {
        check_cancel(cancel)?;
        fresh(client, endpoints, tokens)?;
        let label = remote_label(file);
        if present(library, file) {
            continue;
        }
        progress(format!("Downloading {label}"), done / steps);
        match fetch(endpoints, tokens, file, library, staging, cancel) {
            Ok(()) => report.downloaded += 1,
            Err(error) if error == CANCELLED => return Err(CANCELLED.to_owned()),
            Err(error) => report.failed.push(format!("{label}: {error}")),
        }
        done += 1.0;
    }
    progress("Google Drive sync finished".to_owned(), 1.0);
    Ok(report)
}

/// The fields of a Drive file this module reads, as the API names them.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireFile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    size: Option<serde_json::Value>,
    #[serde(default)]
    md5_checksum: String,
    #[serde(default)]
    app_properties: BTreeMap<String, String>,
}

/// One page of a Drive listing.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FileList {
    #[serde(default)]
    files: Vec<WireFile>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// Google's token answer. A renewal has no `refresh_token` of its own.
#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: String,
}

/// What a request to an upload session came back with.
enum Step {
    /// Google kept the whole file, and this is its id.
    Done(String),
    /// Google kept the bytes before this offset; the next one starts there.
    Resume(u64),
}

/// Why a request to an upload session failed.
enum Trouble {
    /// A dropped connection or a busy server: asking again may work.
    Retry(String),
    /// Anything else: asking again would not help.
    Stop(String),
}

/// The consent URL for one sign-in.
fn auth_url(
    endpoints: &Endpoints,
    client: &Client,
    redirect: &str,
    challenge: &str,
    state: &str,
) -> String {
    with_query(
        &endpoints.auth,
        &[
            ("client_id", client.client_id.as_str()),
            ("redirect_uri", redirect),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    )
}

/// A PKCE pair: the verifier, which stays here, and the challenge, which
/// goes to Google in the consent URL. The verifier is 48 random bytes in
/// base64url, which is 64 unreserved characters.
fn pkce() -> Result<(String, String), String> {
    let mut bytes = [0u8; 48];
    getrandom::fill(&mut bytes).map_err(|_| "this computer has no random source".to_owned())?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = challenge_of(&verifier);
    Ok((verifier, challenge))
}

/// The S256 challenge for `verifier`: base64url, unpadded, of its SHA-256.
fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Reads the browser's one request on the loopback and answers it.
///
/// `Ok(Some(code))` for the redirect that carries the code and the right
/// state, `Ok(None)` for anything else (a favicon, a stray probe), which is
/// answered with a 404 and ignored, and `Err` for a refusal or a state that
/// is not this sign-in's.
fn read_redirect(mut stream: TcpStream, state: &str) -> Result<Option<String>, String> {
    // An accepted socket inherits the listener's non-blocking mode on Windows.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let head = read_request_head(&mut stream);
    let target = head
        .lines()
        .next()
        .and_then(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some("GET"), Some(target)) => Some(target),
                _ => None,
            }
        })
        .unwrap_or_default();
    let params = parse_query(target.split_once('?').map_or("", |(_, query)| query));
    if find(&params, "error").is_some() {
        respond(&mut stream, 400, "Bad Request", PAGE_REFUSED);
        return Err("Google Drive sign-in was not allowed; try again".to_owned());
    }
    let Some(code) = find(&params, "code") else {
        respond(&mut stream, 404, "Not Found", PAGE_NOT_FOUND);
        return Ok(None);
    };
    if find(&params, "state").as_deref() != Some(state) {
        respond(&mut stream, 400, "Bad Request", PAGE_MISMATCH);
        return Err("Google's answer did not match this sign-in; start again".to_owned());
    }
    respond(&mut stream, 200, "OK", PAGE_CONNECTED);
    Ok(Some(code))
}

/// The page the browser shows when the sign-in worked.
const PAGE_CONNECTED: &str = "<!doctype html><meta charset=\"utf-8\"><title>Concat</title>\
<p>Concat is connected to Google Drive. You can close this tab.</p>";
/// The page when Google refused the sign-in.
const PAGE_REFUSED: &str = "<!doctype html><meta charset=\"utf-8\"><title>Concat</title>\
<p>Google Drive did not connect. You can close this tab and try again in Concat.</p>";
/// The page when the answer is not the one this sign-in asked for.
const PAGE_MISMATCH: &str = "<!doctype html><meta charset=\"utf-8\"><title>Concat</title>\
<p>This answer is not from the sign-in Concat started. You can close this tab and try again in Concat.</p>";
/// The page for a request that is not the redirect.
const PAGE_NOT_FOUND: &str = "<!doctype html><meta charset=\"utf-8\"><title>Concat</title>\
<p>Not found.</p>";

/// The head of a request, up to its blank line, read as far as it comes.
fn read_request_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") && head.len() < 16 * 1024 {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// Writes a small HTML answer and closes the connection.
fn respond(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// The account's address, from Drive's `about` call. `None` on any failure:
/// the address is for the settings page, and a sign-in does not need it.
fn account_email(endpoints: &Endpoints, tokens: &Tokens) -> Option<String> {
    let url = with_query(
        &format!("{}/drive/v3/about", endpoints.api),
        &[("fields", "user(emailAddress)")],
    );
    let text = agent()
        .get(&url)
        .set("Authorization", &bearer(tokens))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .pointer("/user/emailAddress")?
        .as_str()
        .map(str::to_owned)
}

/// Posts a form to the token endpoint and reads Google's answer.
fn post_token(endpoints: &Endpoints, form: &[(&str, &str)]) -> Result<TokenAnswer, String> {
    let response = agent()
        .post(&endpoints.token)
        .send_form(form)
        .map_err(|error| describe(SIGN_IN, error))?;
    read_json(response, SIGN_IN)
}

/// Tokens from Google's answer. A renewal that brings no new refresh token
/// keeps `keep_refresh`.
fn tokens_from(answer: TokenAnswer, keep_refresh: String) -> Tokens {
    Tokens {
        access_token: answer.access_token,
        refresh_token: if answer.refresh_token.is_empty() {
            keep_refresh
        } else {
            answer.refresh_token
        },
        expires_at: now().saturating_add(answer.expires_in.unwrap_or(3600)),
        email: String::new(),
    }
}

/// One GET to the Drive API, read as JSON.
fn get_json<T: DeserializeOwned>(tokens: &Tokens, url: &str) -> Result<T, String> {
    let response = agent()
        .get(url)
        .set("Authorization", &bearer(tokens))
        .call()
        .map_err(|error| describe(DRIVE, error))?;
    read_json(response, DRIVE)
}

/// One POST of JSON to the Drive API, read as JSON.
fn post_json<T: DeserializeOwned>(
    tokens: &Tokens,
    url: &str,
    body: &serde_json::Value,
) -> Result<T, String> {
    let response = agent()
        .post(url)
        .set("Authorization", &bearer(tokens))
        .set("Content-Type", "application/json; charset=UTF-8")
        .send_string(&body.to_string())
        .map_err(|error| describe(DRIVE, error))?;
    read_json(response, DRIVE)
}

/// Reads a whole answer and parses it. The error names the service only,
/// never the answer's text, which can echo a request.
fn read_json<T: DeserializeOwned>(response: ureq::Response, what: &str) -> Result<T, String> {
    let text = response
        .into_string()
        .map_err(|error| format!("{what} answer was cut short: {error}"))?;
    serde_json::from_str(&text).map_err(|_| format!("{what} answer was not understood"))
}

/// What a failed call says: Google's own message when the answer has one,
/// the code it answered with, and never the request. A transport failure is
/// described by its kind and message only: ureq's own text starts with the
/// URL, and an upload session's URL is a credential for that upload.
fn describe(what: &str, error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            format!("{what} answered {code}: {}", google_message(&body))
        }
        ureq::Error::Transport(transport) => match transport.message() {
            Some(message) => format!(
                "{what} could not be reached: {}: {message}",
                transport.kind()
            ),
            None => format!("{what} could not be reached: {}", transport.kind()),
        },
    }
}

/// The human message in one of Google's error answers, or the start of the
/// answer when it has none.
fn google_message(body: &str) -> String {
    let value: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let said = value.as_ref().and_then(|value| {
        value
            .pointer("/error/message")
            .or_else(|| value.get("error_description"))
            .or_else(|| value.get("error"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    said.unwrap_or_else(|| body.chars().take(200).collect())
}

/// The agent every call uses: a connect limit, and a read and write limit
/// generous enough for a chunk of 8 MiB on a slow line.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(120))
        .timeout_write(Duration::from_secs(120))
        .build()
}

/// The bearer header for `tokens`. Only ever sent as a header.
fn bearer(tokens: &Tokens) -> String {
    format!("Bearer {}", tokens.access_token)
}

/// Sends one chunk of the file from `offset`, at most [`CHUNK`] bytes, and
/// reads Google's answer. A chunk of nothing (an empty file, or an offset
/// at the end) is sent as a bare status request.
fn send_chunk(
    agent: &ureq::Agent,
    session: &str,
    file: &mut std::fs::File,
    buffer: &mut [u8],
    offset: u64,
    total: u64,
) -> Result<Step, Trouble> {
    let read_failed =
        |error: std::io::Error| Trouble::Stop(format!("could not read the file: {error}"));
    let len = (total.saturating_sub(offset)).min(buffer.len() as u64) as usize;
    let range = if len == 0 {
        format!("bytes */{total}")
    } else {
        format!("bytes {offset}-{}/{total}", offset + len as u64 - 1)
    };
    if len > 0 {
        file.seek(SeekFrom::Start(offset)).map_err(read_failed)?;
        file.read_exact(&mut buffer[..len]).map_err(read_failed)?;
    }
    answer(
        agent
            .put(session)
            .set("Content-Range", &range)
            .send_bytes(&buffer[..len]),
    )
}

/// Asks an upload session how much it has kept, without sending anything.
fn probe(agent: &ureq::Agent, session: &str, total: u64) -> Result<Step, Trouble> {
    answer(
        agent
            .put(session)
            .set("Content-Range", &format!("bytes */{total}"))
            .send_bytes(&[]),
    )
}

/// Reads one answer from an upload session: 308 says how much was kept, 200
/// or 201 says the file is in, and a dropped connection or a 429 or 5xx is
/// worth another try.
fn answer(outcome: Result<ureq::Response, ureq::Error>) -> Result<Step, Trouble> {
    match outcome {
        Ok(response) if response.status() == 308 => Ok(Step::Resume(kept(&response))),
        Ok(response) if matches!(response.status(), 200 | 201) => {
            let text = response.into_string().map_err(|error| {
                Trouble::Stop(format!("the upload answer was cut short: {error}"))
            })?;
            let id = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| {
                    value
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            id.map(Step::Done).ok_or_else(|| {
                Trouble::Stop("Google Drive finished the upload without an id".to_owned())
            })
        }
        Ok(response) => Err(Trouble::Stop(format!(
            "Google Drive answered {} to the upload",
            response.status()
        ))),
        Err(error) => {
            let transient = match &error {
                ureq::Error::Status(code, _) => *code == 429 || *code >= 500,
                _ => true,
            };
            let message = describe(DRIVE, error);
            Err(if transient {
                Trouble::Retry(message)
            } else {
                Trouble::Stop(message)
            })
        }
    }
}

/// The offset after the last byte Google's 308 says it kept: `bytes=0-N`
/// means N+1 bytes are in. No `Range` header means none are.
fn kept(response: &ureq::Response) -> u64 {
    response
        .header("Range")
        .and_then(|range| range.strip_prefix("bytes="))
        .and_then(|span| span.split('-').nth(1))
        .and_then(|end| end.trim().parse::<u64>().ok())
        .map_or(0, |end| end + 1)
}

/// Downloads a response body into `partial`, in pieces, checking `cancel`
/// between them.
fn copy_to(mut reader: impl Read, partial: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let mut file = std::fs::File::create(partial)
        .map_err(|error| format!("could not write {}: {error}", partial.display()))?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let n = reader
            .read(&mut buffer)
            .map_err(|error| format!("the download was cut short: {error}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])
            .map_err(|error| format!("could not write {}: {error}", partial.display()))?;
    }
    file.flush()
        .map_err(|error| format!("could not write {}: {error}", partial.display()))
}

/// Downloads one Concat file from the folder into the library. The bytes
/// are checked against the SHA-256 the file was uploaded with, and the
/// copy is taken in under the asset id it carries.
fn fetch(
    endpoints: &Endpoints,
    tokens: &Tokens,
    file: &RemoteFile,
    library: &mut AssetLibrary,
    staging: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let id = file
        .app_properties
        .get(PROP_ASSET)
        .cloned()
        .unwrap_or_default();
    if !is_plain_id(&id) {
        return Err("its asset id is not one Concat wrote".to_owned());
    }
    std::fs::create_dir_all(staging)
        .map_err(|error| format!("could not make {}: {error}", staging.display()))?;
    let staged = staging.join(format!("{id}.download"));
    download(endpoints, tokens, &file.id, &staged, cancel)?;
    let outcome = import(library, file, &id, &staged, staging);
    let _ = std::fs::remove_file(&staged);
    outcome
}

/// Checks the downloaded bytes and takes them into the library.
fn import(
    library: &mut AssetLibrary,
    file: &RemoteFile,
    id: &str,
    staged: &Path,
    staging: &Path,
) -> Result<(), String> {
    let mut head = Vec::with_capacity(32);
    std::fs::File::open(staged)
        .and_then(|opened| opened.take(32).read_to_end(&mut head))
        .map_err(|error| format!("could not read the download: {error}"))?;
    let (extension, kind) =
        sniff(&head).ok_or_else(|| "it is not a picture or a video".to_owned())?;
    let sha = sha256_of(staged)?;
    let expected = file.app_properties.get(PROP_SHA).map_or("", String::as_str);
    if !expected.is_empty() && !sha.eq_ignore_ascii_case(expected) {
        return Err("the download is not the file that was uploaded".to_owned());
    }
    let typed = staging.join(format!("{id}.{extension}"));
    std::fs::rename(staged, &typed)
        .map_err(|error| format!("could not prepare the download: {error}"))?;
    let outcome = take(library, file, id, kind, &sha, &typed);
    // Moved into the library on success; this removes it only when it was not.
    let _ = std::fs::remove_file(&typed);
    outcome
}

/// Takes the file at `typed` into the library as the asset its properties
/// describe, and records the Drive id on the asset that comes back.
fn take(
    library: &mut AssetLibrary,
    file: &RemoteFile,
    id: &str,
    kind: MediaKind,
    sha: &str,
    typed: &Path,
) -> Result<(), String> {
    let props = &file.app_properties;
    let text = |key: &str| props.get(key).cloned().unwrap_or_default();
    let bytes = std::fs::metadata(typed).map(|meta| meta.len()).unwrap_or(0);
    let name = text(PROP_NAME);
    let template = Asset {
        id: id.to_owned(),
        name: if name.is_empty() {
            file.name.clone()
        } else {
            name
        },
        file: String::new(),
        kind,
        prompt: text(PROP_PROMPT),
        provider: text(PROP_PROVIDER),
        model: text(PROP_MODEL),
        added: text(PROP_ADDED).parse::<u64>().unwrap_or_else(|_| now()),
        bytes,
        sha256: sha.to_owned(),
        drive_id: Some(file.id.clone()),
    };
    let got = library.take_as(typed, template, false)?;
    if got.drive_id.as_deref() != Some(file.id.as_str()) {
        library.set_drive_id(&got.id, Some(file.id.clone()))?;
    }
    Ok(())
}

/// Whether a remote file is already held: by asset id or SHA-256, or as the
/// Drive id of a local asset.
fn present(library: &AssetLibrary, file: &RemoteFile) -> bool {
    let id = file.app_properties.get(PROP_ASSET);
    let sha = file
        .app_properties
        .get(PROP_SHA)
        .filter(|sha| !sha.is_empty());
    library.assets.iter().any(|asset| {
        id == Some(&asset.id)
            || asset.drive_id.as_deref() == Some(file.id.as_str())
            || sha == Some(&asset.sha256)
    })
}

/// Whether the Drive folder holds this local asset's Drive id.
fn held_remotely(remote: &[RemoteFile], asset: &Asset) -> bool {
    asset
        .drive_id
        .as_deref()
        .is_some_and(|id| remote.iter().any(|file| file.id == id))
}

/// Whether a remote file is this local asset: same asset id, or same SHA-256.
fn matches_asset(file: &RemoteFile, asset: &Asset) -> bool {
    file.app_properties
        .get(PROP_ASSET)
        .is_some_and(|id| *id == asset.id)
        || (!asset.sha256.is_empty()
            && file
                .app_properties
                .get(PROP_SHA)
                .is_some_and(|sha| *sha == asset.sha256))
}

/// What a remote file is called in the sync's messages.
fn remote_label(file: &RemoteFile) -> String {
    file.app_properties
        .get(PROP_NAME)
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| file.name.clone())
}

/// The `appProperties` an asset goes up with. Empty values are left out, and
/// each value is cut to fit Drive's limit.
fn properties(asset: &Asset) -> BTreeMap<String, String> {
    [
        (PROP_ASSET, asset.id.as_str()),
        (PROP_SHA, asset.sha256.as_str()),
        (PROP_KIND, asset.kind.id()),
        (PROP_NAME, asset.name.as_str()),
        (PROP_PROMPT, asset.prompt.as_str()),
        (PROP_PROVIDER, asset.provider.as_str()),
        (PROP_MODEL, asset.model.as_str()),
    ]
    .into_iter()
    .map(|(key, value)| (key, value.to_owned()))
    .chain(std::iter::once((PROP_ADDED, asset.added.to_string())))
    .filter(|(_, value)| !value.is_empty())
    .map(|(key, value)| (key.to_owned(), fit(key, &value)))
    .collect()
}

/// `value` cut short, on a character boundary, so that `key` and the value
/// together fit [`PROPERTY_LIMIT`].
fn fit(key: &str, value: &str) -> String {
    let room = PROPERTY_LIMIT.saturating_sub(key.len());
    let mut end = value.len().min(room);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// The media type a library file is uploaded as, by its extension.
fn mime_of(file: &str) -> &'static str {
    let extension = Path::new(file)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// The partial file a download is written to before it is renamed.
fn partial_of(to: &Path) -> PathBuf {
    let mut name = to.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

/// Whether an asset id can be a file name and is one this module would write.
fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Makes `text` safe inside one single-quoted Drive query literal.
fn drive_literal(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "\\'")
}

/// How far an upload has got, `0..=1`.
fn fraction(offset: u64, total: u64) -> f32 {
    if total == 0 {
        1.0
    } else {
        (offset as f32 / total as f32).clamp(0.0, 1.0)
    }
}

/// Waits `duration`, in slices, and gives up early if `cancel` is set.
fn pause(duration: Duration, cancel: &AtomicBool) -> Result<(), String> {
    let until = Instant::now() + duration;
    loop {
        check_cancel(cancel)?;
        let now = Instant::now();
        if now >= until {
            return Ok(());
        }
        std::thread::sleep((until - now).min(Duration::from_millis(100)));
    }
}

/// `Err("cancelled")` when the person has cancelled.
fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.to_owned())
    } else {
        Ok(())
    }
}

/// The Drive file of a remote listing entry, with its size read whether
/// Drive sent it as text, as it does, or as a number.
fn remote_file(wire: WireFile) -> RemoteFile {
    let size = match &wire.size {
        Some(serde_json::Value::String(text)) => text.parse::<u64>().unwrap_or(0),
        Some(serde_json::Value::Number(number)) => number.as_u64().unwrap_or(0),
        _ => 0,
    };
    RemoteFile {
        id: wire.id,
        name: wire.name,
        mime_type: wire.mime_type,
        size,
        md5: wire.md5_checksum,
        app_properties: wire.app_properties,
    }
}

/// The file the sign-in is saved in.
fn token_path(dirs: &AppDirs) -> PathBuf {
    dirs.config.join(TOKEN_FILE)
}

/// Writes `bytes` to `path` from nothing: a stale file there is removed
/// first, so the mode of a new file is the one asked for.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Percent-encodes a query value or path segment: everything but the
/// unreserved characters.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Reverses [`encode`], and reads `+` as a space, as a form does.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' => {
                let value = bytes
                    .get(at + 1..at + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok());
                match value {
                    Some(value) => {
                        out.push(value);
                        at += 3;
                    }
                    None => {
                        out.push(b'%');
                        at += 1;
                    }
                }
            }
            other => {
                out.push(other);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `base` with `params` appended as an encoded query string.
fn with_query(base: &str, params: &[(&str, &str)]) -> String {
    let query: Vec<String> = params
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect();
    format!("{base}?{}", query.join("&"))
}

/// The decoded `key=value` pairs of a query string.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// The value of the first pair named `key`.
fn find(params: &[(String, String)], key: &str) -> Option<String> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

/// Seconds since the Unix epoch, now.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::{Arc, Mutex, mpsc};

    /// One request the mock server received. Header names are lowercase.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }

        fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or_default()
        }

        fn query(&self) -> &str {
            self.target.split_once('?').map_or("", |(_, query)| query)
        }

        fn form(&self) -> Vec<(String, String)> {
            parse_query(&String::from_utf8_lossy(&self.body))
        }

        fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).expect("a JSON body")
        }
    }

    /// What the mock server answers with.
    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Reply {
        fn new(status: u16, body: &str) -> Reply {
            Reply {
                status,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
            }
        }

        fn bytes(status: u16, body: Vec<u8>) -> Reply {
            Reply {
                status,
                headers: Vec::new(),
                body,
            }
        }

        fn json(value: serde_json::Value) -> Reply {
            Reply::new(200, &value.to_string()).with("Content-Type", "application/json")
        }

        fn with(mut self, name: &str, value: &str) -> Reply {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }
    }

    /// Reads one request off a connection, or `None` when it is not one.
    fn read_request(stream: &TcpStream) -> Option<Seen> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let method = parts.next()?.to_owned();
        let target = parts.next()?.to_owned();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).ok()?;
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).ok()?;
        Some(Seen {
            method,
            target,
            headers,
            body,
        })
    }

    /// Writes one answer and closes the connection, so the next request
    /// opens a new one.
    fn write_reply(mut stream: TcpStream, reply: &Reply) {
        let reason = match reply.status {
            200 => "OK",
            201 => "Created",
            308 => "Resume Incomplete",
            400 => "Bad Request",
            404 => "Not Found",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
            reply.status,
            reply.body.len()
        );
        for (name, value) in &reply.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&reply.body);
        let _ = stream.flush();
    }

    /// A local HTTP server on 127.0.0.1:0 that answers each request with
    /// `handler`, one connection at a time, and keeps what it was asked.
    /// Returns its base URL and the log. It serves until the test ends.
    fn serve(
        mut handler: impl FnMut(&Seen) -> Reply + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock server");
        let base = format!("http://{}", listener.local_addr().expect("address"));
        let log: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let Some(request) = read_request(&stream) else {
                    continue;
                };
                let reply = handler(&request);
                seen.lock().expect("the log").push(request);
                write_reply(stream, &reply);
            }
        });
        (base, log)
    }

    /// The `q` parameter of a Drive listing request, decoded.
    fn q_of(request: &Seen) -> String {
        find(&parse_query(request.query()), "q").unwrap_or_default()
    }

    /// Endpoints that all point at one mock server.
    fn endpoints_at(base: &str) -> Endpoints {
        Endpoints {
            auth: format!("{base}/auth"),
            token: format!("{base}/token"),
            api: base.to_owned(),
            upload: format!("{base}/upload/drive/v3"),
        }
    }

    /// Tokens that will not need renewing for an hour.
    fn live_tokens() -> Tokens {
        Tokens {
            access_token: "ACCESS-live".to_owned(),
            refresh_token: "REFRESH-live".to_owned(),
            expires_at: now() + 3600,
            email: "person@example.com".to_owned(),
        }
    }

    fn client() -> Client {
        Client {
            client_id: "client-id.apps".to_owned(),
            client_secret: "client-secret-value".to_owned(),
        }
    }

    fn sha_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn png(tag: &str) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(tag.as_bytes());
        bytes
    }

    #[test]
    fn the_rfc_7636_example_challenge_is_reproduced() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_fresh_verifier_is_long_enough_and_unreserved() {
        let (verifier, challenge) = pkce().expect("random bytes");
        assert!((43..=128).contains(&verifier.len()));
        assert!(
            verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'.' | b'_' | b'~'))
        );
        assert_eq!(challenge, challenge_of(&verifier));
        assert_eq!(challenge.len(), 43);
        assert_ne!(pkce().expect("random bytes").0, verifier);
    }

    #[test]
    fn the_consent_url_carries_every_required_parameter() {
        let endpoints = Endpoints::default();
        let url = auth_url(
            &endpoints,
            &client(),
            "http://127.0.0.1:5555",
            "CHALLENGE",
            "STATE-1",
        );
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        let params = parse_query(url.split_once('?').expect("a query").1);
        let want = [
            ("client_id", "client-id.apps"),
            ("redirect_uri", "http://127.0.0.1:5555"),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", "CHALLENGE"),
            ("code_challenge_method", "S256"),
            ("state", "STATE-1"),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ];
        for (key, value) in want {
            assert_eq!(find(&params, key).as_deref(), Some(value), "{key}");
        }
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555&"));
    }

    #[test]
    fn query_values_round_trip_through_encode_and_decode() {
        assert_eq!(encode("a b&c=d/e~-._"), "a%20b%26c%3Dd%2Fe~-._");
        assert_eq!(decode("a+b%26c%3Dd%2Fe"), "a b&c=d/e");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode(&encode("héllo ✓")), "héllo ✓");
    }

    #[test]
    fn a_drive_literal_escapes_quotes_and_backslashes() {
        assert_eq!(drive_literal("it's \\ ok"), "it\\'s \\\\ ok");
        let query = format!("name='{}'", drive_literal("it's"));
        assert_eq!(
            with_query("https://x", &[("q", query.as_str())]),
            "https://x?q=name%3D%27it%5C%27s%27"
        );
    }

    #[test]
    fn a_property_is_cut_to_fit_drive_on_a_character_boundary() {
        // 13 bytes of key leave 111; each "é" is two bytes, so 110 survive.
        let cut = fit("concatAssetId", &"é".repeat(100));
        assert_eq!(cut.len(), 110);
        assert_eq!(cut.chars().count(), 55);
        assert_eq!(fit("concatName", "short"), "short");
        assert!(
            "concatPrompt".len() + fit("concatPrompt", &"x".repeat(500)).len() <= PROPERTY_LIMIT
        );
    }

    #[test]
    fn debug_output_never_shows_a_secret() {
        let tokens = Tokens {
            access_token: "ya29.SECRET-ACCESS".to_owned(),
            refresh_token: "1//SECRET-REFRESH".to_owned(),
            expires_at: 42,
            email: "person@example.com".to_owned(),
        };
        let shown = format!("{tokens:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("person@example.com"));
        let secret = Client {
            client_id: "id-visible".to_owned(),
            client_secret: "GOCSPX-SECRET".to_owned(),
        };
        let shown = format!("{secret:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("id-visible"));
    }

    #[test]
    fn tokens_are_saved_loaded_and_forgotten() {
        let root = tempfile::tempdir().expect("tempdir");
        let dirs = AppDirs::under(&root.path().join("config"));
        assert!(load_tokens(&dirs).is_none());
        let tokens = live_tokens();
        save_tokens(&dirs, &tokens).expect("save");
        assert_eq!(load_tokens(&dirs), Some(tokens.clone()));
        assert!(dirs.config.join(TOKEN_FILE).is_file());
        assert!(!dirs.config.join("google-drive.json.partial").exists());
        forget(&dirs).expect("forget");
        assert!(load_tokens(&dirs).is_none());
        forget(&dirs).expect("forgetting twice is fine");
    }

    #[cfg(unix)]
    #[test]
    fn the_saved_sign_in_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let dirs = AppDirs::under(root.path());
        save_tokens(&dirs, &live_tokens()).expect("save");
        let mode = std::fs::metadata(dirs.config.join(TOKEN_FILE))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_folder_that_exists_is_found_and_not_made_again() {
        let (base, log) = serve(|request| {
            if request.method == "GET" {
                Reply::json(serde_json::json!({"files": [{"id": "FOLDER-7", "name": FOLDER}]}))
            } else {
                Reply::new(500, "should not create")
            }
        });
        let id = ensure_folder(&endpoints_at(&base), &live_tokens(), FOLDER).expect("folder");
        assert_eq!(id, "FOLDER-7");
        let seen = log.lock().expect("log");
        assert_eq!(seen.len(), 1);
        let query = parse_query(seen[0].query());
        let q = find(&query, "q").expect("q");
        assert!(q.contains("mimeType='application/vnd.google-apps.folder'"));
        assert!(q.contains("name='Concat Library'"));
        assert!(q.contains("trashed=false"));
        assert_eq!(seen[0].header("authorization"), Some("Bearer ACCESS-live"));
    }

    #[test]
    fn a_missing_folder_is_made() {
        let (base, log) = serve(|request| {
            if request.method == "GET" {
                Reply::json(serde_json::json!({"files": []}))
            } else {
                Reply::json(serde_json::json!({"id": "FOLDER-NEW"}))
            }
        });
        let id = ensure_folder(&endpoints_at(&base), &live_tokens(), FOLDER).expect("folder");
        assert_eq!(id, "FOLDER-NEW");
        let seen = log.lock().expect("log");
        assert_eq!(seen[1].method, "POST");
        assert_eq!(seen[1].json()["name"], FOLDER);
        assert_eq!(seen[1].json()["mimeType"], FOLDER_MIME);
    }

    #[test]
    fn a_listing_reads_every_page_and_the_sizes_in_both_forms() {
        let (base, log) = serve(|request| {
            if request.query().contains("pageToken") {
                Reply::json(serde_json::json!({"files": [
                    {"id": "B", "name": "b.mp4", "mimeType": "video/mp4", "size": 7, "md5Checksum": "m2"}
                ]}))
            } else {
                Reply::json(serde_json::json!({
                    "nextPageToken": "PAGE-2",
                    "files": [{
                        "id": "A", "name": "a.png", "mimeType": "image/png", "size": "1234",
                        "md5Checksum": "m1",
                        "appProperties": {"concatAssetId": "asset-a", "concatSha256": "s1"}
                    }]
                }))
            }
        });
        let files = list(&endpoints_at(&base), &live_tokens(), "FOLDER-7").expect("list");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].id, "A");
        assert_eq!(files[0].size, 1234);
        assert_eq!(files[0].md5, "m1");
        assert_eq!(
            files[0]
                .app_properties
                .get("concatAssetId")
                .map(String::as_str),
            Some("asset-a")
        );
        assert_eq!(files[1].size, 7);
        assert!(files[1].app_properties.is_empty());
        let seen = log.lock().expect("log");
        assert_eq!(seen.len(), 2);
        assert!(
            find(&parse_query(seen[0].query()), "q")
                .expect("q")
                .contains("'FOLDER-7' in parents")
        );
        assert!(find(&parse_query(seen[1].query()), "pageToken").as_deref() == Some("PAGE-2"));
    }

    #[test]
    fn an_expiring_token_is_renewed_and_a_fresh_one_is_left_alone() {
        let (base, log) = serve(|_| {
            Reply::json(serde_json::json!({"access_token": "ACCESS-new", "expires_in": 3600}))
        });
        let endpoints = endpoints_at(&base);
        let mut tokens = Tokens {
            access_token: "ACCESS-old".to_owned(),
            refresh_token: "REFRESH-kept".to_owned(),
            expires_at: 1,
            email: "person@example.com".to_owned(),
        };
        assert!(fresh(&client(), &endpoints, &mut tokens).expect("renew"));
        assert_eq!(tokens.access_token, "ACCESS-new");
        assert_eq!(tokens.refresh_token, "REFRESH-kept");
        assert_eq!(tokens.email, "person@example.com");
        assert!(tokens.expires_at > now() + 3000);
        {
            let seen = log.lock().expect("log");
            assert_eq!(seen.len(), 1);
            let form = seen[0].form();
            assert_eq!(find(&form, "grant_type").as_deref(), Some("refresh_token"));
            assert_eq!(
                find(&form, "refresh_token").as_deref(),
                Some("REFRESH-kept")
            );
            assert_eq!(
                find(&form, "client_secret").as_deref(),
                Some("client-secret-value")
            );
        }
        assert!(!fresh(&client(), &endpoints, &mut tokens).expect("no renewal needed"));
        assert_eq!(log.lock().expect("log").len(), 1);
    }

    #[test]
    fn a_connection_that_fails_names_no_address() {
        // A port nothing listens on: bind it, then let it go.
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("address")
            .port();
        let endpoints = endpoints_at(&format!("http://127.0.0.1:{port}"));
        let mut tokens = Tokens {
            refresh_token: "REFRESH-x".to_owned(),
            ..Tokens::default()
        };
        let error = fresh(&client(), &endpoints, &mut tokens).expect_err("nothing listens");
        assert!(error.contains("could not be reached"), "{error}");
        assert!(!error.contains(&port.to_string()), "{error}");
        assert!(!error.contains("REFRESH-x"));
    }

    #[test]
    fn a_refused_renewal_says_so_without_the_secrets() {
        let (base, _) = serve(|_| {
            Reply::new(
                400,
                r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
            )
        });
        let mut tokens = Tokens {
            refresh_token: "REFRESH-revoked".to_owned(),
            ..Tokens::default()
        };
        let error = fresh(&client(), &endpoints_at(&base), &mut tokens).expect_err("refused");
        assert!(
            error.contains("Token has been expired or revoked."),
            "{error}"
        );
        assert!(!error.contains("REFRESH-revoked"));
        assert!(!error.contains("client-secret-value"));
    }

    #[test]
    fn a_resumable_upload_follows_google_through_308_and_a_busy_server() {
        // Two chunks and a tail: 8 MiB plus 1 KiB.
        let total = CHUNK as u64 + 1024;
        let content: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.mp4");
        std::fs::write(&path, &content).expect("write");

        let mut replies = std::collections::VecDeque::from(vec![
            // The first chunk: Google kept only the first 4 MiB.
            Reply::new(308, "").with("Range", "bytes=0-4194303"),
            // The second chunk's first try meets a busy server.
            Reply::new(503, "busy"),
            // Asked how much it has, it still has only 4 MiB.
            Reply::new(308, "").with("Range", "bytes=0-4194303"),
            // The retry keeps the whole second chunk.
            Reply::new(308, "").with("Range", "bytes=0-8388607"),
            // The tail completes the file.
            Reply::json(serde_json::json!({"id": "DRIVE-BIG"})),
        ]);
        let (base, log) = serve(move |request| {
            if request.method == "POST" {
                let host = request.header("host").unwrap_or_default();
                Reply::new(200, "").with("Location", &format!("http://{host}/session/9"))
            } else {
                replies
                    .pop_front()
                    .unwrap_or_else(|| Reply::new(500, "out of script"))
            }
        });
        let progress_seen = Mutex::new(Vec::new());
        let progress = |fraction: f32| progress_seen.lock().expect("progress").push(fraction);
        let cancel = AtomicBool::new(false);
        let mut props = BTreeMap::new();
        props.insert("concatAssetId".to_owned(), "big-asset".to_owned());
        let id = upload(
            &endpoints_at(&base),
            &live_tokens(),
            "FOLDER-7",
            &path,
            "big-asset.mp4",
            "video/mp4",
            &props,
            &progress,
            &cancel,
        )
        .expect("upload");
        assert_eq!(id, "DRIVE-BIG");

        let seen = log.lock().expect("log");
        assert_eq!(seen[0].method, "POST");
        assert_eq!(
            seen[0].header("x-upload-content-length"),
            Some(total.to_string().as_str())
        );
        assert_eq!(seen[0].json()["parents"][0], "FOLDER-7");
        assert_eq!(
            seen[0].json()["appProperties"]["concatAssetId"],
            "big-asset"
        );
        let puts: Vec<&Seen> = seen.iter().skip(1).collect();
        let ranges: Vec<&str> = puts
            .iter()
            .map(|put| put.header("content-range").expect("a Content-Range"))
            .collect();
        assert_eq!(
            ranges,
            vec![
                "bytes 0-8388607/8389632",
                "bytes 4194304-8389631/8389632",
                "bytes */8389632",
                "bytes 4194304-8389631/8389632",
                "bytes 8388608-8389631/8389632",
            ]
        );
        assert_eq!(puts[0].body, content[..CHUNK]);
        // Google kept 4 MiB, so the next request sends everything that is left.
        assert_eq!(puts[1].body, content[4194304..]);
        assert_eq!(puts[3].body, content[4194304..]);
        assert_eq!(puts[4].body, content[CHUNK..]);
        assert!(puts[2].body.is_empty());
        let fractions = progress_seen.lock().expect("progress");
        assert_eq!(fractions.last().copied(), Some(1.0));
        assert!(fractions.iter().all(|f| (0.0..=1.0).contains(f)));
    }

    #[test]
    fn an_upload_that_is_cancelled_stops_without_a_session_chunk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.png");
        std::fs::write(&path, png("x")).expect("write");
        let (base, log) = serve(|request| {
            if request.method == "POST" {
                let host = request.header("host").unwrap_or_default();
                Reply::new(200, "").with("Location", &format!("http://{host}/session/1"))
            } else {
                Reply::new(500, "must not be reached")
            }
        });
        let cancel = AtomicBool::new(true);
        let error = upload(
            &endpoints_at(&base),
            &live_tokens(),
            "FOLDER-7",
            &path,
            "a.png",
            "image/png",
            &BTreeMap::new(),
            &|_| {},
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(error, "cancelled");
        assert_eq!(log.lock().expect("log").len(), 0);
    }

    #[test]
    fn a_download_lands_whole_and_leaves_no_partial_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let to = root.path().join("out").join("file.png");
        let (base, log) = serve(|request| {
            if request.path() == "/drive/v3/files/F-9" && request.query().contains("alt=media") {
                Reply::bytes(200, b"PAYLOAD-BYTES".to_vec())
            } else {
                Reply::new(404, "")
            }
        });
        let cancel = AtomicBool::new(false);
        download(&endpoints_at(&base), &live_tokens(), "F-9", &to, &cancel).expect("download");
        assert_eq!(std::fs::read(&to).expect("read"), b"PAYLOAD-BYTES");
        assert!(!partial_of(&to).exists());
        assert_eq!(log.lock().expect("log").len(), 1);
    }

    #[test]
    fn a_refused_download_names_google_s_message_and_leaves_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let to = root.path().join("file.png");
        let (base, _) = serve(|_| Reply::new(500, r#"{"error":{"message":"backend boom"}}"#));
        let cancel = AtomicBool::new(false);
        let error = download(&endpoints_at(&base), &live_tokens(), "F-9", &to, &cancel)
            .expect_err("refused");
        assert!(
            error.contains("500") && error.contains("backend boom"),
            "{error}"
        );
        assert!(!to.exists() && !partial_of(&to).exists());
    }

    #[test]
    fn a_sign_in_runs_through_the_loopback_and_stores_the_tokens() {
        let (base, log) = serve(|request| match request.path() {
            "/token" => Reply::json(serde_json::json!({
                "access_token": "ACCESS-signed-in",
                "refresh_token": "REFRESH-signed-in",
                "expires_in": 3599,
            })),
            "/drive/v3/about" => {
                Reply::json(serde_json::json!({"user": {"emailAddress": "person@example.com"}}))
            }
            _ => Reply::new(404, ""),
        });
        let endpoints = endpoints_at(&base);
        let cancel = AtomicBool::new(false);
        let (answers, received) = mpsc::channel::<String>();
        let consent_url = Mutex::new(String::new());
        let open = |url: &str| -> Result<(), String> {
            *consent_url.lock().expect("url") = url.to_owned();
            let params = parse_query(url.split_once('?').map_or("", |(_, query)| query));
            let redirect = find(&params, "redirect_uri").unwrap_or_default();
            let state = find(&params, "state").unwrap_or_default();
            let address = redirect.trim_start_matches("http://").to_owned();
            let answers = answers.clone();
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(&address).expect("connect to the loopback");
                let request =
                    format!("GET /?code=CODE-1&state={state} HTTP/1.1\r\nHost: {address}\r\n\r\n");
                stream.write_all(request.as_bytes()).expect("send");
                let mut page = String::new();
                let _ = stream.read_to_string(&mut page);
                let _ = answers.send(page);
            });
            Ok(())
        };

        let tokens = authorize(&client(), &endpoints, &open, &cancel).expect("sign in");
        assert_eq!(tokens.access_token, "ACCESS-signed-in");
        assert_eq!(tokens.refresh_token, "REFRESH-signed-in");
        assert_eq!(tokens.email, "person@example.com");
        assert!(tokens.expires_at > now() + 3000);

        let page = received.recv().expect("the browser's page");
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("connected to Google Drive"));

        let url = consent_url.lock().expect("url").clone();
        let url_params = parse_query(url.split_once('?').expect("query").1);
        assert_eq!(
            find(&url_params, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(find(&url_params, "access_type").as_deref(), Some("offline"));
        assert_eq!(find(&url_params, "prompt").as_deref(), Some("consent"));
        let challenge = find(&url_params, "code_challenge").expect("challenge");

        let seen = log.lock().expect("log");
        let token_request = seen
            .iter()
            .find(|request| request.path() == "/token")
            .expect("token call");
        let form = token_request.form();
        assert_eq!(find(&form, "code").as_deref(), Some("CODE-1"));
        assert_eq!(
            find(&form, "grant_type").as_deref(),
            Some("authorization_code")
        );
        assert_eq!(find(&form, "client_id").as_deref(), Some("client-id.apps"));
        assert_eq!(
            find(&form, "client_secret").as_deref(),
            Some("client-secret-value")
        );
        let verifier = find(&form, "code_verifier").expect("verifier");
        assert_eq!(
            challenge_of(&verifier),
            challenge,
            "PKCE: the verifier matches the challenge sent"
        );
        let redirect = find(&form, "redirect_uri").expect("redirect");
        assert!(redirect.starts_with("http://127.0.0.1:"));
        assert!(
            seen.iter()
                .any(|request| request.path() == "/drive/v3/about")
        );
    }

    #[test]
    fn a_browser_answer_with_another_state_is_refused() {
        let (base, log) = serve(|_| Reply::new(500, "must not be reached"));
        let cancel = AtomicBool::new(false);
        let (answers, received) = mpsc::channel::<String>();
        let open = |url: &str| -> Result<(), String> {
            let params = parse_query(url.split_once('?').map_or("", |(_, query)| query));
            let address = find(&params, "redirect_uri")
                .unwrap_or_default()
                .trim_start_matches("http://")
                .to_owned();
            let answers = answers.clone();
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(&address).expect("connect");
                let request =
                    format!("GET /?code=CODE-1&state=NOT-OURS HTTP/1.1\r\nHost: {address}\r\n\r\n");
                stream.write_all(request.as_bytes()).expect("send");
                let mut page = String::new();
                let _ = stream.read_to_string(&mut page);
                let _ = answers.send(page);
            });
            Ok(())
        };
        let error =
            authorize(&client(), &endpoints_at(&base), &open, &cancel).expect_err("refused");
        assert!(error.contains("did not match"), "{error}");
        assert!(received.recv().expect("page").starts_with("HTTP/1.1 400"));
        assert!(
            log.lock().expect("log").is_empty(),
            "no token call after a mismatch"
        );
    }

    #[test]
    fn a_refusal_from_google_ends_the_sign_in_with_its_reason() {
        let (base, _) = serve(|_| Reply::new(500, "must not be reached"));
        let cancel = AtomicBool::new(false);
        let open = |url: &str| -> Result<(), String> {
            let params = parse_query(url.split_once('?').map_or("", |(_, query)| query));
            let address = find(&params, "redirect_uri")
                .unwrap_or_default()
                .trim_start_matches("http://")
                .to_owned();
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(&address).expect("connect");
                let request =
                    format!("GET /?error=access_denied HTTP/1.1\r\nHost: {address}\r\n\r\n");
                stream.write_all(request.as_bytes()).expect("send");
                let mut page = String::new();
                let _ = stream.read_to_string(&mut page);
            });
            Ok(())
        };
        let error =
            authorize(&client(), &endpoints_at(&base), &open, &cancel).expect_err("refused");
        assert!(error.contains("not allowed"), "{error}");
    }

    #[test]
    fn a_cancelled_sign_in_stops_at_once() {
        let cancel = AtomicBool::new(true);
        let open = |_: &str| -> Result<(), String> { Ok(()) };
        let error =
            authorize(&client(), &Endpoints::default(), &open, &cancel).expect_err("cancelled");
        assert_eq!(error, "cancelled");
    }

    #[test]
    fn a_sign_in_without_a_client_id_is_not_started() {
        let cancel = AtomicBool::new(false);
        let open = |_: &str| -> Result<(), String> { panic!("the browser must not open") };
        let error = authorize(&Client::default(), &Endpoints::default(), &open, &cancel)
            .expect_err("no client id");
        assert!(error.contains("client id"));
    }

    #[test]
    fn sync_uploads_what_drive_lacks_and_fetches_what_the_library_lacks() {
        let local_one = png("local-one");
        let local_two = png("local-two");
        let remote_only = png("remote-only");
        let remote_sha = sha_hex(&remote_only);

        let root = tempfile::tempdir().expect("tempdir");
        let library_dir = root.path().join("library");
        std::fs::create_dir_all(&library_dir).expect("library dir");
        let mut library = AssetLibrary::at(library_dir.clone());
        for (id, bytes) in [("local-asset-1", &local_one), ("local-asset-2", &local_two)] {
            std::fs::write(library_dir.join(format!("{id}.png")), bytes).expect("write");
            library.assets.push(Asset {
                id: id.to_owned(),
                name: format!("Local {id}"),
                file: format!("{id}.png"),
                kind: MediaKind::Image,
                prompt: "a prompt".to_owned(),
                provider: "openrouter".to_owned(),
                model: "model-x".to_owned(),
                added: 1_700_000_000,
                bytes: bytes.len() as u64,
                sha256: sha_hex(bytes),
                drive_id: None,
            });
        }

        let remote_listing = serde_json::json!({"files": [
            {
                "id": "REMOTE1", "name": "remote-only-asset.png", "mimeType": "image/png",
                "size": remote_only.len().to_string(),
                "appProperties": {
                    "concatAssetId": "remote-only-asset", "concatSha256": remote_sha,
                    "concatKind": "image", "concatName": "Remote picture", "concatPrompt": "a cat",
                    "concatProvider": "flow", "concatModel": "m", "concatAdded": "1600000000"
                }
            },
            {
                "id": "REMOTE2", "name": "shared.png", "mimeType": "image/png", "size": "9",
                "appProperties": {"concatAssetId": "shared-asset", "concatSha256": sha_hex(&local_two)}
            },
            {
                "id": "REMOTE3", "name": "bad.png", "mimeType": "image/png", "size": "1",
                "appProperties": {"concatAssetId": "../escape", "concatSha256": "x", "concatName": "Bad name"}
            }
        ]});
        let download_bytes = remote_only.clone();
        let (base, log) = serve(
            move |request| match (request.method.as_str(), request.path()) {
                ("GET", "/drive/v3/files") if q_of(request).contains("mimeType") => {
                    Reply::json(serde_json::json!({"files": [{"id": "FOLDER-7", "name": FOLDER}]}))
                }
                ("GET", "/drive/v3/files") if q_of(request).contains("in parents") => {
                    Reply::json(remote_listing.clone())
                }
                ("GET", "/drive/v3/files/REMOTE1") => Reply::bytes(200, download_bytes.clone()),
                ("POST", "/upload/drive/v3/files") => {
                    let host = request.header("host").unwrap_or_default();
                    Reply::new(200, "").with("Location", &format!("http://{host}/session/1"))
                }
                ("PUT", "/session/1") => Reply::json(serde_json::json!({"id": "UP-1"})),
                _ => Reply::new(404, "unexpected"),
            },
        );

        let mut tokens = live_tokens();
        let staging = root.path().join("staging");
        let cancel = AtomicBool::new(false);
        let lines = Mutex::new(Vec::new());
        let progress = |message: String, fraction: f32| {
            assert!((0.0..=1.0).contains(&fraction));
            lines.lock().expect("lines").push(message);
        };
        let report = sync(
            &client(),
            &endpoints_at(&base),
            &mut tokens,
            &mut library,
            &staging,
            &progress,
            &cancel,
        )
        .expect("sync");

        assert_eq!(report.uploaded, 1, "{report:?}");
        assert_eq!(report.downloaded, 1, "{report:?}");
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(report.failed[0].starts_with("Bad name: "), "{report:?}");

        // Local asset 1 went up and was recorded; asset 2 matched REMOTE2 by digest.
        assert_eq!(
            library
                .get("local-asset-1")
                .and_then(|a| a.drive_id.clone())
                .as_deref(),
            Some("UP-1")
        );
        assert_eq!(
            library
                .get("local-asset-2")
                .and_then(|a| a.drive_id.clone())
                .as_deref(),
            Some("REMOTE2")
        );
        // The remote-only file came down under the id it was uploaded with.
        let fetched = library
            .get("remote-only-asset")
            .expect("fetched into the library")
            .clone();
        assert_eq!(fetched.drive_id.as_deref(), Some("REMOTE1"));
        assert_eq!(fetched.name, "Remote picture");
        assert_eq!(fetched.prompt, "a cat");
        assert_eq!(fetched.provider, "flow");
        assert_eq!(fetched.added, 1_600_000_000);
        assert_eq!(fetched.kind, MediaKind::Image);
        assert_eq!(
            std::fs::read(library.path_of(&fetched)).expect("copy"),
            remote_only
        );
        assert!(!staging.join("remote-only-asset.download").exists());

        let seen = log.lock().expect("log");
        let post = seen
            .iter()
            .find(|request| request.method == "POST")
            .expect("one upload");
        let metadata = post.json();
        assert_eq!(metadata["parents"][0], "FOLDER-7");
        assert_eq!(metadata["name"], "local-asset-1.png");
        assert_eq!(metadata["appProperties"]["concatAssetId"], "local-asset-1");
        assert_eq!(
            metadata["appProperties"]["concatSha256"],
            sha_hex(&local_one).as_str()
        );
        assert_eq!(
            metadata["appProperties"]["concatName"],
            "Local local-asset-1"
        );
        assert_eq!(
            seen.iter()
                .filter(|request| request.method == "POST")
                .count(),
            1
        );
        let put = seen
            .iter()
            .find(|request| request.method == "PUT")
            .expect("the bytes");
        assert_eq!(put.body, local_one);
        assert!(
            lines
                .lock()
                .expect("lines")
                .iter()
                .any(|line| line.starts_with("Uploading"))
        );
    }

    #[test]
    fn a_failed_download_is_reported_and_the_sync_goes_on() {
        let good = png("good");
        let good_sha = sha_hex(&good);
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("library")).expect("library dir");
        let mut library = AssetLibrary::at(root.path().join("library"));
        let listing = serde_json::json!({"files": [
            {"id": "BROKEN", "name": "a.png", "mimeType": "image/png", "size": "3",
             "appProperties": {"concatAssetId": "a-asset", "concatName": "A pic"}},
            {"id": "GOOD", "name": "b.png", "mimeType": "image/png", "size": "3",
             "appProperties": {"concatAssetId": "b-asset", "concatSha256": good_sha, "concatName": "B pic"}}
        ]});
        let (base, _) = serve(move |request| match request.path() {
            "/drive/v3/files" if q_of(request).contains("mimeType") => {
                Reply::json(serde_json::json!({"files": [{"id": "FOLDER-7"}]}))
            }
            "/drive/v3/files" => Reply::json(listing.clone()),
            "/drive/v3/files/BROKEN" => Reply::new(500, r#"{"error":{"message":"backend boom"}}"#),
            "/drive/v3/files/GOOD" => Reply::bytes(200, good.clone()),
            _ => Reply::new(404, ""),
        });
        let mut tokens = live_tokens();
        let cancel = AtomicBool::new(false);
        let report = sync(
            &client(),
            &endpoints_at(&base),
            &mut tokens,
            &mut library,
            &root.path().join("staging"),
            &|_, _| {},
            &cancel,
        )
        .expect("sync goes on past one failure");
        assert_eq!(report.downloaded, 1, "{report:?}");
        assert_eq!(report.uploaded, 0);
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(report.failed[0].starts_with("A pic: "), "{report:?}");
        assert!(report.failed[0].contains("backend boom"), "{report:?}");
        assert!(library.get("b-asset").is_some());
        assert!(library.get("a-asset").is_none());
    }

    #[test]
    fn a_cancelled_sync_asks_drive_for_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut library = AssetLibrary::at(root.path().join("library"));
        let (base, log) = serve(|_| Reply::new(500, "must not be reached"));
        let mut tokens = live_tokens();
        let cancel = AtomicBool::new(true);
        let error = sync(
            &client(),
            &endpoints_at(&base),
            &mut tokens,
            &mut library,
            &root.path().join("staging"),
            &|_, _| {},
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(error, "cancelled");
        assert!(log.lock().expect("log").is_empty());
    }
}
