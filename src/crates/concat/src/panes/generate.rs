// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The library's AI page: pictures and video made from a prompt, the
//! Assets shelf that keeps them, and Settings › AI & cloud, which holds
//! the accounts both work with.
//!
//! Everything slow here is a thread of its own, never one of the shared
//! workers: a video from OpenRouter is polled for minutes, the Codex check
//! blocks for up to forty seconds, a bridged job waits for as long as the
//! browser takes, and a Drive sync moves every file there is. Three
//! workers held by those would starve the monitor of frames. Each job has
//! its own cancel flag and reports back through `host::on_ui`; the window
//! closing sets every flag and stops the bridge.
//!
//! The asset library (`concat_host::assets`) is one index on disk, so it
//! is one `AssetLibrary` behind a mutex, and every thread that adds to it -
//! a generation, an import, a sync - takes the lock while it does. The
//! shelf shows a copy of the list, read again after each of them, with a
//! `try_lock` so the window never waits on a sync. What a generation makes
//! is written into a staging folder under the app's data folder and moved
//! into the library from there; an empty staging folder is removed after.
//! Files that could not be indexed stay there, including across launches.
//!
//! A card is laid on the timeline the way a sound from the Audio page is:
//! copied into the project's own `assets` folder and imported into the bin
//! first, so the project keeps its own copy. A drag carries
//! `asset:<id>:<name>`.
//!
//! No secret is ever in a message: what is typed into a key's field rides
//! as a [`Secret`], whose `Debug` - which the activity log reads - is a
//! placeholder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use concat_host::AppDirs;
use concat_host::assets::{self, Asset, AssetLibrary, Origin};
use concat_host::bridge::{Bridge, Event};
use concat_host::drive;
use concat_host::generate::openrouter::{self, ModelInfo};
use concat_host::generate::{MediaKind, Progress, Provider, Request, chatgpt, random_hex};
use slint::{ComponentHandle, SharedString};

use crate::host::on_ui;
use crate::i18n::{t, tf};
use crate::panes::Msg;
use crate::panes::settings::SettingsMsg;
use crate::studio::{Models, Studio, sync};
use crate::ui::{App, AssetCardData, Generate, GenerateJobData};

/// The Settings sheet's AI & cloud page, by its index in the sheet.
pub const SETTINGS_PAGE: i32 = 8;

/// The aspect ratios the form's chips offer, after the provider's own
/// choice, which is the empty one.
const ASPECTS: [&str; 4] = ["", "16:9", "9:16", "1:1"];

/// How often the bridge is asked which pages have called in.
const BRIDGE_POLL: Duration = Duration::from_secs(3);

/// How long a bridged job's thread waits for an event before it looks at
/// its cancel flag again.
const EVENT_SLICE: Duration = Duration::from_millis(250);

/// The longest a job's title runs, in characters.
const TITLE_CHARS: usize = 60;

/// The prefix a card's id carries on the source preview, so the monitor's
/// chip lights the right card on the right page.
const PREVIEW_CARD: &str = "asset-";

/// Text that must not reach a log: what is typed into a key's or a
/// secret's field.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where a job stands, as its row says it.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Sent, not started.
    Queued,
    /// Waiting for the browser extension to take it.
    Waiting,
    /// The browser took it and is driving the page.
    Taken,
    /// Under way, in the provider's own words.
    Running(String),
    /// The file is coming down.
    Downloading,
    /// This many files have come back from the browser so far.
    Received(usize),
}

impl Step {
    /// The row's status line.
    fn label(&self) -> String {
        match self {
            Step::Queued => t("generate.queued"),
            Step::Waiting => t("generate.waitingForBrowser"),
            Step::Taken => t("generate.browserWorking"),
            Step::Running(message) if message.trim().is_empty() => t("generate.working"),
            Step::Running(message) => message.clone(),
            Step::Downloading => t("generate.downloading"),
            Step::Received(files) => tf("generate.filesIn", &[files]),
        }
    }
}

/// Everything that can happen to the AI page and to Settings › AI & cloud.
#[derive(Debug)]
pub enum GenerateMsg {
    /// The preferences read on launch: the bridge started if it is on.
    Restore,
    /// Whether a library is showing the page.
    Shown(bool),
    ProviderChanged(i32),
    /// 0 a picture, 1 a video.
    KindChanged(i32),
    ModelEdited(String),
    /// A model picked from OpenRouter's list, by its row.
    ModelPicked(i32),
    /// OpenRouter's list for a kind, or why it could not be had.
    ModelsLoaded(MediaKind, Result<Vec<ModelInfo>, String>),
    AspectChanged(i32),
    SecondsChanged(i32),
    PromptEdited(String),
    /// The Generate button.
    Start,
    /// A job's cancel, by its id.
    Cancel(u64),
    /// A job's report.
    Progress(u64, Step, Option<f32>),
    /// A bridged job brought a file in; the shelf is read again.
    Took(u64, Result<String, String>),
    /// A job is over: how many assets it added, or why it failed.
    Finished(u64, Result<usize, String>),
    /// Settings, on the AI & cloud page: the fix for the page's hint.
    OpenSettings,
    /// Files picked, or dropped while the page was up.
    Import(Vec<PathBuf>),
    /// Filters the shelf by its names, prompts, models and makers.
    AssetQueryEdited(String),
    /// The import's thread is done: each file's asset id, or why not.
    Imported(Vec<Result<String, String>>),
    /// A card clicked: on the monitor.
    Preview(String),
    /// A card's plus: onto the timeline at the playhead.
    Add(String),
    Remove(String),
    /// A card renamed: its id and the new name.
    Rename(String, String),
    /// "Sync with Google Drive".
    Sync,
    /// The sync's report: a line and a fraction.
    SyncProgress(String, f32),
    /// The sync is over.
    Synced(Result<drive::SyncReport, String>),
    OpenRouterKeyEdited(Secret),
    ImageModelEdited(String),
    VideoModelEdited(String),
    /// "Check again": the Codex command line asked how it stands.
    CodexCheck,
    /// Settings turned to the AI & cloud page: Codex is asked how it
    /// stands, the first time.
    SettingsShown,
    CodexChecked(chatgpt::Status),
    BridgeEnabledChanged(bool),
    BridgePortEdited(i32),
    BridgeTokenRegenerated,
    DriveClientIdEdited(String),
    DriveClientSecretEdited(Secret),
    /// Connect: Google's sign-in in the browser.
    DriveConnect,
    DriveCancel,
    /// The sign-in is over: the account's address, or why not.
    DriveConnected(Result<String, String>),
    DriveDisconnect,
    DriveAutoSyncChanged(bool),
}

/// One generation under way.
struct Job {
    id: u64,
    provider: Provider,
    prompt: String,
    step: Step,
    fraction: Option<f32>,
    cancel: Arc<AtomicBool>,
    /// The bridge's id for a bridged job, which its cancel names.
    bridge_id: Option<String>,
}

/// The page's state, and the settings page's.
#[derive(Default)]
pub struct GeneratePane {
    /// A library is showing the page.
    pub shown: bool,
    dirs: Option<AppDirs>,
    /// The asset library, shared with the threads that add to it.
    library: Arc<Mutex<AssetLibrary>>,
    /// The library's folder, for a card's file.
    library_dir: PathBuf,
    /// What the shelf shows: the library as last read, newest first.
    shelf: Vec<Asset>,
    /// View state only: searching never changes the library or its ids.
    asset_query: String,
    /// The library was busy when the shelf was last read.
    shelf_stale: bool,
    /// Each card's picture and a video's length, once read; by asset id.
    thumbs: HashMap<String, slint::Image>,
    durations: HashMap<String, f64>,
    /// Cards whose picture is being read.
    art_pending: HashSet<String>,
    /// Files being copied in by the Import button or a drop.
    importing: usize,

    kind: MediaKind,
    prompt: String,
    /// The model typed for Flow; OpenRouter's are the preferences' own.
    flow_model: String,
    /// OpenRouter's lists, by kind, once fetched, and those being fetched.
    models: HashMap<MediaKind, Vec<ModelInfo>>,
    models_loading: HashSet<MediaKind>,
    jobs: Vec<Job>,
    next_job: u64,
    /// What the last job to end did, and whether it failed.
    status: (String, bool),

    /// How the Codex command line stands, once asked.
    codex: Option<chatgpt::Status>,
    codex_checking: bool,

    bridge: Option<Bridge>,
    /// Why the bridge is not running while its switch is on.
    bridge_error: String,
    /// Which pages have called in: Flow, ChatGPT's.
    connected: (bool, bool),
    /// Asks the bridge who has called in while it runs.
    bridge_poll: slint::Timer,

    /// The Google account signed in, by its address; `Some("")` when the
    /// address could not be read.
    drive_email: Option<String>,
    /// The sign-in running, by its stop flag.
    drive_signing: Option<Arc<AtomicBool>>,
    drive_error: String,
    /// The sync running, by its stop flag, and how it is going.
    syncing: Option<Arc<AtomicBool>>,
    sync_progress: (String, f32),
    /// Another sync is wanted once this one ends.
    sync_again: bool,
    /// What the last sync did.
    sync_status: String,

    /// Threads still running, so a closing window can give them a moment.
    live: Arc<AtomicUsize>,
}

impl GeneratePane {
    /// The page as this machine's library has it. Empty staging is cleared;
    /// files left by an interrupted or failed import remain recoverable.
    pub fn open(dirs: &AppDirs) -> GeneratePane {
        let library = AssetLibrary::open(dirs);
        cleanup_staging(&staging(dirs));
        GeneratePane {
            library_dir: library.dir.clone(),
            shelf: library.assets.clone(),
            library: Arc::new(Mutex::new(library)),
            drive_email: drive::load_tokens(dirs).map(|tokens| tokens.email),
            dirs: Some(dirs.clone()),
            next_job: 1,
            ..GeneratePane::default()
        }
    }

    /// Applies one message. The studio is the rest of the window; while
    /// this runs the studio's copy of the pane is a blank it must not read.
    pub fn update(&mut self, msg: GenerateMsg, studio: &mut Studio) {
        if self.shelf_stale {
            self.refresh_shelf();
        }
        match msg {
            GenerateMsg::Restore => self.apply_bridge(studio),
            GenerateMsg::Shown(on) => {
                self.shown = on;
                if on {
                    self.wanted(studio);
                }
            }
            GenerateMsg::ProviderChanged(index) => {
                let Some(provider) = Provider::ALL.get(index.max(0) as usize).copied() else {
                    return;
                };
                studio.prefs.ai.provider = provider.id().to_owned();
                studio.prefs.save(&studio.host.dirs);
                if !provider.kinds().contains(&self.kind) {
                    self.kind = MediaKind::Image;
                }
                self.wanted(studio);
            }
            GenerateMsg::KindChanged(index) => {
                let kind = if index == 1 {
                    MediaKind::Video
                } else {
                    MediaKind::Image
                };
                if studio.prefs.ai.provider().kinds().contains(&kind) {
                    self.kind = kind;
                }
                self.wanted(studio);
            }
            GenerateMsg::ModelEdited(text) => self.set_model(studio, text.trim()),
            GenerateMsg::ModelPicked(row) => {
                let id = self
                    .models
                    .get(&self.kind)
                    .and_then(|models| models.get(row.max(0) as usize))
                    .map(|model| model.id.clone());
                if let Some(id) = id {
                    self.set_model(studio, &id);
                }
            }
            GenerateMsg::ModelsLoaded(kind, result) => {
                self.models_loading.remove(&kind);
                match result {
                    Ok(models) => {
                        self.models.insert(kind, models);
                    }
                    // The field still takes a name typed by hand; the list
                    // is a convenience, and asked for again next time.
                    Err(error) => log::info!("generate: OpenRouter's models: {error}"),
                }
            }
            GenerateMsg::AspectChanged(index) => {
                let aspect = ASPECTS.get(index.max(0) as usize).copied().unwrap_or("");
                studio.prefs.ai.aspect_ratio = aspect.to_owned();
                studio.prefs.save(&studio.host.dirs);
            }
            GenerateMsg::SecondsChanged(seconds) => {
                studio.prefs.ai.video_seconds = seconds.clamp(0, 600) as u32;
                studio.prefs.save(&studio.host.dirs);
            }
            GenerateMsg::PromptEdited(text) => self.prompt = text,
            GenerateMsg::Start => self.start(studio),
            GenerateMsg::Cancel(id) => {
                if let Some(job) = self.jobs.iter().find(|job| job.id == id) {
                    job.cancel.store(true, Ordering::Relaxed);
                    if let (Some(bridge), Some(bridge_id)) = (&self.bridge, &job.bridge_id) {
                        bridge.cancel(bridge_id);
                    }
                }
            }
            GenerateMsg::Progress(id, step, fraction) => {
                if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
                    job.step = step;
                    job.fraction = fraction;
                }
            }
            GenerateMsg::Took(id, result) => {
                if result.is_ok()
                    && let Some(job) = self.jobs.iter_mut().find(|job| job.id == id)
                {
                    let files = match job.step {
                        Step::Received(files) => files + 1,
                        _ => 1,
                    };
                    job.step = Step::Received(files);
                }
                self.refresh_shelf();
                if let Err(error) = result {
                    studio.notify(&tf("generate.couldNotKeep", &[&error]), true);
                }
            }
            GenerateMsg::Finished(id, result) => self.finished(id, result, studio),
            GenerateMsg::OpenSettings => open_settings(studio),
            GenerateMsg::Import(paths) => self.import(paths, studio),
            GenerateMsg::AssetQueryEdited(query) => self.asset_query = query,
            GenerateMsg::Imported(results) => {
                self.importing = self.importing.saturating_sub(1);
                self.refresh_shelf();
                let added = results.iter().filter(|result| result.is_ok()).count();
                if let Some(Err(error)) = results.iter().find(|result| result.is_err()) {
                    studio.notify(error, true);
                } else if added == 1 {
                    studio.notify(&t("generate.importedOne"), false);
                } else if added > 1 {
                    studio.notify(&tf("generate.importedMany", &[&added]), false);
                }
                self.request_art();
            }
            GenerateMsg::Preview(id) => {
                if let Some((name, path)) = self.file_of(&id) {
                    studio.preview_file(&format!("{PREVIEW_CARD}{id}"), &name, &path);
                }
            }
            GenerateMsg::Add(id) => {
                let start = f64::from(studio.playhead.max(0.0));
                if let Some((name, path)) = self.file_of(&id) {
                    studio.place_file(&name, &path, PROJECT_FOLDER, None, start);
                }
            }
            GenerateMsg::Remove(id) => self.remove(&id, studio),
            GenerateMsg::Rename(id, name) => {
                let Ok(mut library) = self.library.try_lock() else {
                    studio.notify(&t("generate.libraryBusy"), true);
                    return;
                };
                if let Err(error) = library.rename(&id, &name) {
                    studio.notify(&error, true);
                }
                drop(library);
                self.refresh_shelf();
            }
            GenerateMsg::Sync => {
                if self.drive_email.is_none() {
                    open_settings(studio);
                } else {
                    self.start_sync(studio);
                }
            }
            GenerateMsg::SyncProgress(line, fraction) => {
                if self.syncing.is_some() {
                    self.sync_progress = (line, fraction);
                }
            }
            GenerateMsg::Synced(result) => {
                self.syncing = None;
                self.refresh_shelf();
                self.sync_status = match result {
                    Ok(report) => {
                        let mut line =
                            tf("generate.synced", &[&report.uploaded, &report.downloaded]);
                        if !report.failed.is_empty() {
                            for failure in &report.failed {
                                log::warn!("drive: {failure}");
                            }
                            line.push_str(" · ");
                            line.push_str(&tf("generate.syncSomeFailed", &[&report.failed.len()]));
                        }
                        line
                    }
                    Err(error) if error == "cancelled" => t("generate.cancelled"),
                    Err(error) => {
                        log::warn!("drive: the sync failed: {error}");
                        tf("generate.syncFailed", &[&error])
                    }
                };
                self.request_art();
                if std::mem::take(&mut self.sync_again) {
                    self.start_sync(studio);
                }
            }
            GenerateMsg::OpenRouterKeyEdited(Secret(key)) => {
                studio.prefs.ai.openrouter_key = key.trim().to_owned();
                studio.prefs.save(&studio.host.dirs);
            }
            GenerateMsg::ImageModelEdited(text) => {
                studio.prefs.ai.image_model = text.trim().to_owned();
                studio.prefs.save(&studio.host.dirs);
            }
            GenerateMsg::VideoModelEdited(text) => {
                studio.prefs.ai.video_model = text.trim().to_owned();
                studio.prefs.save(&studio.host.dirs);
            }
            GenerateMsg::CodexCheck => self.check_codex(studio),
            GenerateMsg::SettingsShown => {
                if self.codex.is_none() {
                    self.check_codex(studio);
                }
            }
            GenerateMsg::CodexChecked(status) => {
                self.codex_checking = false;
                self.codex = Some(status);
            }
            GenerateMsg::BridgeEnabledChanged(on) => {
                studio.prefs.ai.bridge_enabled = on;
                studio.prefs.save(&studio.host.dirs);
                self.apply_bridge(studio);
            }
            GenerateMsg::BridgePortEdited(port) => {
                let port = port.clamp(1024, 65535) as u16;
                if port == studio.prefs.ai.bridge_port {
                    return;
                }
                studio.prefs.ai.bridge_port = port;
                studio.prefs.save(&studio.host.dirs);
                self.apply_bridge(studio);
            }
            GenerateMsg::BridgeTokenRegenerated => {
                studio.prefs.ai.bridge_token = random_hex(32);
                studio.prefs.save(&studio.host.dirs);
                self.apply_bridge(studio);
            }
            GenerateMsg::DriveClientIdEdited(text) => {
                studio.prefs.ai.drive_client_id = text.trim().to_owned();
                studio.prefs.save(&studio.host.dirs);
            }
            GenerateMsg::DriveClientSecretEdited(Secret(text)) => {
                studio.prefs.ai.drive_client_secret = text.trim().to_owned();
                studio.prefs.save(&studio.host.dirs);
            }
            GenerateMsg::DriveConnect => self.drive_connect(studio),
            GenerateMsg::DriveCancel => {
                if let Some(cancel) = &self.drive_signing {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            GenerateMsg::DriveConnected(result) => {
                self.drive_signing = None;
                match result {
                    Ok(email) => {
                        self.drive_error.clear();
                        self.drive_email = Some(email);
                    }
                    Err(error) if error == "cancelled" => {}
                    Err(error) => self.drive_error = error,
                }
            }
            GenerateMsg::DriveDisconnect => {
                if let Some(cancel) = &self.syncing {
                    cancel.store(true, Ordering::Relaxed);
                }
                match drive::forget(&studio.host.dirs) {
                    Ok(()) => {
                        self.drive_email = None;
                        self.drive_error.clear();
                        self.sync_status.clear();
                    }
                    Err(error) => studio.notify(&error, true),
                }
            }
            GenerateMsg::DriveAutoSyncChanged(on) => {
                studio.prefs.ai.drive_auto_sync = on;
                studio.prefs.save(&studio.host.dirs);
            }
        }
    }

    /// What the page needs for the provider and kind now on it, asked for
    /// once: OpenRouter's model list, the Codex command line's standing,
    /// the cards' pictures.
    fn wanted(&mut self, studio: &mut Studio) {
        if !self.shown {
            return;
        }
        match studio.prefs.ai.provider() {
            Provider::OpenRouter => self.load_models(),
            Provider::ChatGpt if self.codex.is_none() => self.check_codex(studio),
            _ => {}
        }
        self.request_art();
    }

    /// The model the form shows for the provider and kind on it.
    fn model(&self, studio: &Studio) -> String {
        match (studio.prefs.ai.provider(), self.kind) {
            (Provider::OpenRouter, MediaKind::Image) => studio.prefs.ai.image_model.clone(),
            (Provider::OpenRouter, MediaKind::Video) => studio.prefs.ai.video_model.clone(),
            (Provider::Flow, _) => self.flow_model.clone(),
            _ => String::new(),
        }
    }

    /// Writes the form's model: OpenRouter's is the preference Settings
    /// shows as the default for its kind, so the two are one choice.
    fn set_model(&mut self, studio: &mut Studio, model: &str) {
        match (studio.prefs.ai.provider(), self.kind) {
            (Provider::OpenRouter, MediaKind::Image) => {
                studio.prefs.ai.image_model = model.to_owned();
            }
            (Provider::OpenRouter, MediaKind::Video) => {
                studio.prefs.ai.video_model = model.to_owned();
            }
            (Provider::Flow, _) => {
                self.flow_model = model.to_owned();
                return;
            }
            _ => return,
        }
        studio.prefs.save(&studio.host.dirs);
    }

    /// Fetches OpenRouter's model list for the kind on the form, once.
    fn load_models(&mut self) {
        let kind = self.kind;
        if self.models.contains_key(&kind) || !self.models_loading.insert(kind) {
            return;
        }
        self.spawn_thread("concat models", move || {
            let result = openrouter::list_models(openrouter::BASE_URL, kind);
            on_ui(move |studio, _, _| {
                studio.handle(Msg::Generate(GenerateMsg::ModelsLoaded(kind, result)));
            });
        });
    }

    /// Asks the Codex command line how it stands, on a thread: the answer
    /// takes up to forty seconds.
    fn check_codex(&mut self, studio: &Studio) {
        if self.codex_checking {
            return;
        }
        self.codex_checking = true;
        let config = codex_config(studio);
        self.spawn_thread("concat codex status", move || {
            let status = chatgpt::status(&config);
            on_ui(move |studio, _, _| {
                studio.handle(Msg::Generate(GenerateMsg::CodexChecked(status)));
            });
        });
    }

    /// Why the provider on the form cannot make anything yet, and whether
    /// the fix is in Settings; `None` when it can.
    fn hint(&self, studio: &Studio) -> Option<(String, bool)> {
        let provider = studio.prefs.ai.provider();
        match provider {
            Provider::OpenRouter if studio.prefs.ai.openrouter_key.trim().is_empty() => {
                Some((t("generate.hintOpenRouterKey"), true))
            }
            Provider::OpenRouter => None,
            Provider::ChatGpt => match &self.codex {
                None if self.codex_checking => Some((t("generate.hintCodexChecking"), false)),
                None => None,
                Some(chatgpt::Status::Missing) => Some((t("generate.hintCodexMissing"), true)),
                Some(chatgpt::Status::LoggedOut { .. }) => {
                    Some((t("generate.hintCodexLogin"), true))
                }
                Some(chatgpt::Status::ApiKey { .. }) => Some((t("generate.hintCodexApiKey"), true)),
                Some(chatgpt::Status::Ready { .. }) => None,
            },
            Provider::ChatGptWeb | Provider::Flow => {
                if self.bridge.is_none() {
                    Some((t("generate.hintBridgeOff"), true))
                } else if !self.site_connected(provider) {
                    Some((t("generate.hintOpenTheSite"), true))
                } else {
                    None
                }
            }
        }
    }

    /// Whether the provider's page has called in lately.
    fn site_connected(&self, provider: Provider) -> bool {
        match provider {
            Provider::Flow => self.connected.0,
            Provider::ChatGptWeb => self.connected.1,
            _ => false,
        }
    }

    /// Whether the Generate button does anything: a prompt, a kind the
    /// provider makes, and the provider set up as far as is known. A
    /// bridged job may be queued before the page has called in: it waits.
    fn can_generate(&self, studio: &Studio) -> bool {
        let provider = studio.prefs.ai.provider();
        if self.prompt.trim().is_empty() || !provider.kinds().contains(&self.kind) {
            return false;
        }
        match provider {
            Provider::OpenRouter => !studio.prefs.ai.openrouter_key.trim().is_empty(),
            Provider::ChatGpt => !matches!(
                self.codex,
                Some(chatgpt::Status::Missing | chatgpt::Status::LoggedOut { .. })
            ),
            Provider::ChatGptWeb | Provider::Flow => self.bridge.is_some(),
        }
    }

    /// Starts a job for the form as it stands.
    fn start(&mut self, studio: &mut Studio) {
        if !self.can_generate(studio) {
            return;
        }
        let Some(dirs) = self.dirs.clone() else {
            return;
        };
        let provider = studio.prefs.ai.provider();
        let request = Request {
            kind: self.kind,
            prompt: self.prompt.trim().to_owned(),
            model: self.model(studio),
            aspect_ratio: studio.prefs.ai.aspect_ratio.clone(),
            duration: if self.kind == MediaKind::Video {
                studio.prefs.ai.video_seconds
            } else {
                0
            },
            count: 1,
            ..Request::default()
        };
        let id = self.next_job;
        self.next_job += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let origin = Origin {
            name: String::new(),
            prompt: request.prompt.clone(),
            provider: provider.id().to_owned(),
            model: request.model.clone(),
        };
        let mut job = Job {
            id,
            provider,
            prompt: request.prompt.clone(),
            step: Step::Queued,
            fraction: None,
            cancel: Arc::clone(&cancel),
            bridge_id: None,
        };
        let library = Arc::clone(&self.library);
        if provider.bridged() {
            let Some(bridge) = self.bridge.clone() else {
                return;
            };
            let (bridge_id, events) = match bridge.submit(provider, request) {
                Ok(submitted) => submitted,
                Err(error) => {
                    self.status = (tf("generate.failed", &[&error]), true);
                    return;
                }
            };
            job.step = Step::Waiting;
            job.bridge_id = Some(bridge_id.clone());
            let out_dir = bridge.out_dir(&bridge_id);
            self.spawn_thread("concat bridge job", move || {
                let post = |msg: GenerateMsg| {
                    on_ui(move |studio, _, _| studio.handle(Msg::Generate(msg)));
                };
                let result = follow_bridged(id, &events, &cancel, &library, &origin, &post);
                cleanup_staging(&out_dir);
                on_ui(move |studio, _, _| {
                    studio.handle(Msg::Generate(GenerateMsg::Finished(id, result)));
                });
            });
        } else {
            let out_dir = staging(&dirs).join(random_hex(12));
            let openrouter_config = openrouter::Config::new(studio.prefs.ai.openrouter_key.clone());
            let codex = codex_config(studio);
            self.spawn_thread("concat generate", move || {
                let report = move |progress: Progress| {
                    let (step, fraction) = match progress {
                        Progress::Queued => (Step::Queued, None),
                        Progress::Running { fraction, message } => {
                            (Step::Running(message), fraction)
                        }
                        Progress::Downloading => (Step::Downloading, None),
                    };
                    on_ui(move |studio, _, _| {
                        studio.handle(Msg::Generate(GenerateMsg::Progress(id, step, fraction)));
                    });
                };
                let made = match provider {
                    Provider::ChatGpt => {
                        chatgpt::generate(&codex, &request, &out_dir, &report, &cancel)
                    }
                    _ => openrouter::generate(
                        &openrouter_config,
                        &request,
                        &out_dir,
                        &report,
                        &cancel,
                    ),
                };
                let result = made.and_then(|outputs| {
                    let mut library = lock(&library);
                    let mut taken = 0;
                    let mut failure = None;
                    for output in outputs {
                        let origin = Origin {
                            model: if output.model.is_empty() {
                                origin.model.clone()
                            } else {
                                output.model.clone()
                            },
                            ..origin.clone()
                        };
                        match library.take(&output.path, origin, false) {
                            Ok(_) => taken += 1,
                            Err(error) => failure = Some(error),
                        }
                    }
                    match failure {
                        Some(error) if taken == 0 => Err(error),
                        _ => Ok(taken),
                    }
                });
                cleanup_staging(&out_dir);
                on_ui(move |studio, _, _| {
                    studio.handle(Msg::Generate(GenerateMsg::Finished(id, result)));
                });
            });
        }
        log::info!(
            "generate: a {} from {} started",
            self.kind.id(),
            provider.id()
        );
        self.jobs.push(job);
    }

    /// A job is over: its row goes, the shelf is read again, and with
    /// automatic sync on, Google Drive is brought up to date.
    fn finished(&mut self, id: u64, result: Result<usize, String>, studio: &mut Studio) {
        self.jobs.retain(|job| job.id != id);
        self.refresh_shelf();
        self.request_art();
        match result {
            Ok(0) => self.status = (t("generate.nothingCame"), true),
            Ok(added) => {
                self.status = if added == 1 {
                    (t("generate.addedOne"), false)
                } else {
                    (tf("generate.addedMany", &[&added]), false)
                };
                if studio.prefs.ai.drive_auto_sync && self.drive_email.is_some() {
                    self.start_sync(studio);
                }
            }
            Err(error) if error == "cancelled" => self.status = (t("generate.cancelled"), false),
            Err(error) => {
                log::warn!("generate: failed: {error}");
                self.status = (tf("generate.failed", &[&error]), true);
                studio.notify(&tf("generate.failed", &[&error]), true);
            }
        }
    }

    /// Copies picked or dropped pictures and videos into the library on a
    /// thread; anything else is refused by name.
    fn import(&mut self, paths: Vec<PathBuf>, studio: &mut Studio) {
        let (taken, refused): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|path| assets::is_asset(path));
        if let Some(path) = refused.first() {
            studio.notify(&tf("generate.notAPicture", &[&display_name(path)]), true);
        }
        if taken.is_empty() {
            return;
        }
        self.importing += 1;
        let library = Arc::clone(&self.library);
        self.spawn_thread("concat assets import", move || {
            let results: Vec<Result<String, String>> = {
                let mut library = lock(&library);
                taken
                    .iter()
                    .map(|path| {
                        library
                            .take(path, Origin::default(), true)
                            .map(|asset| asset.id)
                    })
                    .collect()
            };
            on_ui(move |studio, _, _| {
                studio.handle(Msg::Generate(GenerateMsg::Imported(results)));
            });
        });
    }

    /// Takes an asset out of the library, its copy with it.
    fn remove(&mut self, id: &str, studio: &mut Studio) {
        // Not showing what is about to be deleted.
        let card = format!("{PREVIEW_CARD}{id}");
        if studio
            .source
            .as_ref()
            .is_some_and(|source| source.card == card)
        {
            studio.close_source();
        }
        let Ok(mut library) = self.library.try_lock() else {
            studio.notify(&t("generate.libraryBusy"), true);
            return;
        };
        let name = library
            .get(id)
            .map(|asset| asset.name.clone())
            .unwrap_or_default();
        let result = library.remove(id);
        drop(library);
        match result {
            Ok(()) => {
                self.thumbs.remove(id);
                self.durations.remove(id);
                studio.notify(&tf("generate.removed", &[&name]), false);
            }
            Err(error) => studio.notify(&error, true),
        }
        self.refresh_shelf();
    }

    /// Reads the shelf again from the library, unless a thread holds it;
    /// then on the next message.
    fn refresh_shelf(&mut self) {
        match self.library.try_lock() {
            Ok(library) => {
                self.shelf = library.assets.clone();
                self.shelf_stale = false;
            }
            Err(_) => self.shelf_stale = true,
        }
    }

    /// The name and the file of a card.
    fn file_of(&self, id: &str) -> Option<(String, PathBuf)> {
        let asset = self.shelf.iter().find(|asset| asset.id == id)?;
        let path = self.library_dir.join(&asset.file);
        path.is_file().then(|| (asset.name.clone(), path))
    }

    /// What a drop's plan needs of a card: its name, whether it moves, and
    /// its length when that has been read.
    pub fn summary(&self, id: &str) -> Option<(String, bool, Option<f64>)> {
        let asset = self.shelf.iter().find(|asset| asset.id == id)?;
        Some((
            asset.name.clone(),
            asset.kind == MediaKind::Video,
            self.durations.get(id).copied(),
        ))
    }

    /// Reads each card's picture that is not drawn yet, at the artwork
    /// priority the bin's thumbnails take: a still decoded small, or a
    /// video's frame from its first seconds, with its length.
    fn request_art(&mut self) {
        if !self.shown {
            return;
        }
        let wanted: Vec<(String, PathBuf, MediaKind)> = self
            .shelf
            .iter()
            .filter(|asset| {
                !self.thumbs.contains_key(&asset.id) && !self.art_pending.contains(&asset.id)
            })
            .map(|asset| {
                (
                    asset.id.clone(),
                    self.library_dir.join(&asset.file),
                    asset.kind,
                )
            })
            .collect();
        for (id, path, kind) in wanted {
            self.art_pending.insert(id.clone());
            concat_host::scheduler().submit(concat_media::Priority::Artwork, move || {
                let path = path.to_string_lossy().into_owned();
                let duration = (kind == MediaKind::Video)
                    .then(|| concat_host::media::probe(&path).ok()?.duration)
                    .flatten();
                // Within the first two seconds, as the bin's card takes it.
                let at = duration.map_or(0.0, |seconds| (seconds * 0.25).min(2.0));
                let frame = concat_host::media::still_at(&path, at, 240).ok();
                on_ui(move |studio, _, _| {
                    studio.generate.art_arrived(id, frame, duration);
                });
            });
        }
    }

    /// A card's picture and length have been read. A picture that could
    /// not be read keeps its glyph, and is not asked for again this run.
    fn art_arrived(
        &mut self,
        id: String,
        frame: Option<concat_core::frame::Frame>,
        duration: Option<f64>,
    ) {
        self.art_pending.remove(&id);
        if let Some(duration) = duration {
            self.durations.insert(id.clone(), duration);
        }
        let image = frame
            .as_ref()
            .map(crate::host::image_of)
            .unwrap_or_default();
        self.thumbs.insert(id, image);
    }

    /// Starts a Drive sync on a thread, or asks for another once the one
    /// running ends.
    fn start_sync(&mut self, studio: &Studio) {
        if self.syncing.is_some() {
            self.sync_again = true;
            return;
        }
        let Some(dirs) = self.dirs.clone() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.syncing = Some(Arc::clone(&cancel));
        self.sync_progress = (t("generate.syncing"), -1.0);
        let client = drive_client(studio);
        let library = Arc::clone(&self.library);
        self.spawn_thread("concat drive sync", move || {
            let result = match drive::load_tokens(&dirs) {
                None => Err(t("generate.driveNotConnected")),
                Some(mut tokens) => {
                    let staging = staging(&dirs).join("drive");
                    let report = |line: String, fraction: f32| {
                        on_ui(move |studio, _, _| {
                            studio.handle(Msg::Generate(GenerateMsg::SyncProgress(line, fraction)));
                        });
                    };
                    let result = {
                        let mut library = lock(&library);
                        drive::sync(
                            &client,
                            &drive::Endpoints::default(),
                            &mut tokens,
                            &mut library,
                            &staging,
                            &report,
                            &cancel,
                        )
                    };
                    // A sync renews the sign-in it was given, whatever
                    // its outcome; the renewal is kept.
                    if let Err(error) = drive::save_tokens(&dirs, &tokens) {
                        log::warn!("drive: {error}");
                    }
                    cleanup_staging(&staging);
                    result
                }
            };
            on_ui(move |studio, _, _| {
                studio.handle(Msg::Generate(GenerateMsg::Synced(result)));
            });
        });
    }

    /// Google's sign-in, in the browser, on a thread: it waits up to five
    /// minutes for the person to say yes.
    fn drive_connect(&mut self, studio: &Studio) {
        if self.drive_signing.is_some() {
            return;
        }
        let Some(dirs) = self.dirs.clone() else {
            return;
        };
        let client = drive_client(studio);
        let cancel = Arc::new(AtomicBool::new(false));
        self.drive_signing = Some(Arc::clone(&cancel));
        self.drive_error.clear();
        self.spawn_thread("concat drive sign-in", move || {
            let open = |url: &str| crate::platform::open_link(url);
            let result = drive::authorize(&client, &drive::Endpoints::default(), &open, &cancel)
                .and_then(|tokens| {
                    drive::save_tokens(&dirs, &tokens)?;
                    Ok(tokens.email)
                });
            on_ui(move |studio, _, _| {
                studio.handle(Msg::Generate(GenerateMsg::DriveConnected(result)));
            });
        });
    }

    /// Starts or stops the browser bridge to match the preferences. A
    /// bridge already running is stopped first, so a new port or token
    /// takes effect, and the jobs it was holding are cancelled: the new one
    /// knows nothing of them. A port in use leaves the switch on and says
    /// why in Settings.
    fn apply_bridge(&mut self, studio: &mut Studio) {
        if let Some(bridge) = self.bridge.take() {
            bridge.stop();
            for job in self.jobs.iter().filter(|job| job.bridge_id.is_some()) {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        self.bridge_poll.stop();
        self.connected = (false, false);
        self.bridge_error.clear();
        let Some(dirs) = self.dirs.clone() else {
            return;
        };
        if !studio.prefs.ai.bridge_enabled {
            return;
        }
        if studio.prefs.ai.ensure_bridge_token() {
            studio.prefs.save(&studio.host.dirs);
        }
        let started = Bridge::start(
            studio.prefs.ai.bridge_port,
            studio.prefs.ai.bridge_token.clone(),
            staging(&dirs).join("bridge"),
        );
        match started {
            Ok(bridge) => {
                log::info!("bridge: listening on 127.0.0.1:{}", bridge.port());
                self.bridge = Some(bridge);
                self.bridge_poll
                    .start(slint::TimerMode::Repeated, BRIDGE_POLL, poll_bridge);
            }
            Err(error) => {
                log::warn!("bridge: could not start: {error}");
                self.bridge_error = error;
            }
        }
    }

    /// Asks the bridge which pages have called in; whether that changed.
    fn poll_connected(&mut self) -> bool {
        let now = self.bridge.as_ref().map_or((false, false), |bridge| {
            (
                bridge.connected(Provider::Flow),
                bridge.connected(Provider::ChatGptWeb),
            )
        });
        std::mem::replace(&mut self.connected, now) != now
    }

    /// Runs `work` on a thread of its own, counted while it runs.
    fn spawn_thread(&self, name: &str, work: impl FnOnce() + Send + 'static) {
        let live = Arc::clone(&self.live);
        live.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                work();
                live.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(error) = spawned {
            self.live.fetch_sub(1, Ordering::SeqCst);
            log::warn!("generate: could not start {name}: {error}");
        }
    }

    /// The window is closing: every job, sign-in and sync is told to stop,
    /// and the bridge lets go of its port. A Codex run is a process of its
    /// own, killed when its job sees the flag, so the threads get a moment
    /// to see it before the process goes.
    pub fn shutdown(&mut self) {
        for job in &self.jobs {
            job.cancel.store(true, Ordering::Relaxed);
        }
        for flag in [&self.syncing, &self.drive_signing].into_iter().flatten() {
            flag.store(true, Ordering::Relaxed);
        }
        self.bridge_poll.stop();
        if let Some(bridge) = self.bridge.take() {
            bridge.stop();
        }
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while self.live.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The page, its shelf and the settings page, into the window.
    pub fn publish(&self, studio: &Studio, app: &App, models: &Models) {
        let page = app.global::<Generate>();
        let provider = studio.prefs.ai.provider();
        sync(
            &models.generate_providers,
            Provider::ALL
                .iter()
                .map(|provider| SharedString::from(provider_name(*provider)))
                .collect(),
        );
        page.set_provider(
            Provider::ALL
                .iter()
                .position(|candidate| *candidate == provider)
                .unwrap_or(0) as i32,
        );
        page.set_video_allowed(provider.kinds().contains(&MediaKind::Video));
        page.set_kind(i32::from(self.kind == MediaKind::Video));
        page.set_model_shown(matches!(provider, Provider::OpenRouter | Provider::Flow));
        let model = self.model(studio);
        page.set_model(model.as_str().into());
        page.set_model_placeholder(
            match (provider, self.kind) {
                (Provider::OpenRouter, MediaKind::Image) => {
                    openrouter::DEFAULT_IMAGE_MODEL.to_owned()
                }
                (Provider::OpenRouter, MediaKind::Video) => {
                    openrouter::DEFAULT_VIDEO_MODEL.to_owned()
                }
                _ => t("generate.defaultModel"),
            }
            .into(),
        );
        let listed: &[ModelInfo] = match provider {
            Provider::OpenRouter => self.models.get(&self.kind).map_or(&[], Vec::as_slice),
            _ => &[],
        };
        sync(
            &models.generate_models,
            listed
                .iter()
                .map(|info| SharedString::from(info.name.as_str()))
                .collect(),
        );
        sync(
            &models.generate_model_ids,
            listed
                .iter()
                .map(|info| SharedString::from(info.id.as_str()))
                .collect(),
        );
        page.set_model_choice(
            listed
                .iter()
                .position(|info| info.id == model)
                .map_or(-1, |row| row as i32),
        );
        page.set_models_loading(
            provider == Provider::OpenRouter && self.models_loading.contains(&self.kind),
        );
        page.set_aspect(
            ASPECTS
                .iter()
                .position(|aspect| *aspect == studio.prefs.ai.aspect_ratio)
                .unwrap_or(0) as i32,
        );
        page.set_seconds(studio.prefs.ai.video_seconds as i32);
        page.set_prompt(self.prompt.as_str().into());
        let hint = self.hint(studio);
        page.set_hint(hint.as_ref().map_or("", |(text, _)| text.as_str()).into());
        page.set_hint_settings(hint.is_some_and(|(_, settings)| settings));
        page.set_can_generate(self.can_generate(studio));
        sync(
            &models.generate_jobs,
            self.jobs
                .iter()
                .map(|job| GenerateJobData {
                    id: job.id as i32,
                    title: job_title(&job.prompt, job.provider).into(),
                    status: job.step.label().into(),
                    progress: job
                        .fraction
                        .map_or(-1.0, |fraction| fraction.clamp(0.0, 1.0)),
                })
                .collect(),
        );
        page.set_status(self.status.0.as_str().into());
        page.set_status_failed(self.status.1);

        sync(&models.assets, self.cards());
        page.set_asset_query(self.asset_query.as_str().into());
        page.set_asset_count(self.shelf.len() as i32);
        page.set_importing(self.importing > 0);
        page.set_sync_state(if self.syncing.is_some() {
            2
        } else if self.drive_email.is_some() {
            1
        } else {
            0
        });
        page.set_sync_status(
            if self.syncing.is_some() {
                self.sync_progress.0.as_str()
            } else {
                self.sync_status.as_str()
            }
            .into(),
        );
        page.set_sync_progress(self.sync_progress.1);

        page.set_openrouter_key(studio.prefs.ai.openrouter_key.as_str().into());
        page.set_image_model(studio.prefs.ai.image_model.as_str().into());
        page.set_video_model(studio.prefs.ai.video_model.as_str().into());
        let (codex_state, codex_status) = codex_line(self.codex.as_ref(), self.codex_checking);
        page.set_codex_state(codex_state);
        page.set_codex_status(codex_status.into());
        page.set_codex_checking(self.codex_checking);
        page.set_bridge_enabled(studio.prefs.ai.bridge_enabled);
        page.set_bridge_port(i32::from(studio.prefs.ai.bridge_port));
        page.set_bridge_token(studio.prefs.ai.bridge_token.as_str().into());
        let (bridge_state, bridge_status) = match &self.bridge {
            Some(bridge) => (
                1,
                tf(
                    "settings.bridgeListening",
                    &[&format!("127.0.0.1:{}", bridge.port())],
                ),
            ),
            None if !self.bridge_error.is_empty() => {
                (2, tf("settings.bridgeCouldNotStart", &[&self.bridge_error]))
            }
            None => (0, t("settings.off")),
        };
        page.set_bridge_state(bridge_state);
        page.set_bridge_status(bridge_status.into());
        page.set_flow_connected(self.connected.0);
        page.set_chatgpt_connected(self.connected.1);
        page.set_drive_client_id(studio.prefs.ai.drive_client_id.as_str().into());
        page.set_drive_client_secret(studio.prefs.ai.drive_client_secret.as_str().into());
        page.set_drive_state(if self.drive_signing.is_some() {
            1
        } else if self.drive_email.is_some() {
            2
        } else {
            0
        });
        page.set_drive_email(self.drive_email.as_deref().unwrap_or_default().into());
        page.set_drive_error(self.drive_error.as_str().into());
        page.set_drive_auto_sync(studio.prefs.ai.drive_auto_sync);
    }

    /// The shelf's cards.
    fn cards(&self) -> Vec<AssetCardData> {
        let terms: Vec<String> = self
            .asset_query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.shelf
            .iter()
            .filter(|asset| asset_matches(asset, &terms))
            .map(|asset| AssetCardData {
                id: asset.id.as_str().into(),
                name: asset.name.as_str().into(),
                video: asset.kind == MediaKind::Video,
                thumb: self.thumbs.get(&asset.id).cloned().unwrap_or_default(),
                badge: badge(&asset.provider).into(),
                duration: self.durations.get(&asset.id).copied().unwrap_or(0.0) as f32,
            })
            .collect()
    }
}

/// Every search word may match a different field. Match the displayed badge
/// too, so a person can search for the same provider name the card shows.
fn asset_matches(asset: &Asset, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let fields = [
        asset.name.to_lowercase(),
        asset.prompt.to_lowercase(),
        asset.model.to_lowercase(),
        asset.provider.to_lowercase(),
        badge(&asset.provider).to_lowercase(),
    ];
    terms
        .iter()
        .all(|term| fields.iter().any(|field| field.contains(term)))
}

/// What the Import button's picker offers: every picture and video the
/// library takes (`assets::IMAGE_EXTENSIONS` and `VIDEO_EXTENSIONS`).
pub const PICK_EXTENSIONS: [&str; 14] = [
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "mp4", "mov", "webm", "mkv", "m4v",
    "avi",
];

/// The project folder a card is copied into before it reaches the bin.
pub const PROJECT_FOLDER: &str = "assets";

/// Where generations, the bridge's jobs and a sync's downloads are written
/// before they are taken into the library.
fn staging(dirs: &AppDirs) -> PathBuf {
    dirs.data.join("generate")
}

/// Only an empty job folder is disposable: a file left here may be the only
/// copy of a generation or download whose library index could not be saved.
fn cleanup_staging(dir: &Path) {
    if let Err(error) = std::fs::remove_dir(dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::info!("generate: kept staging at {}: {error}", dir.display());
    }
}

/// The library's lock, even after a thread holding it panicked: the index
/// on disk is written whole or not at all, so what is in memory is still a
/// library.
fn lock(library: &Mutex<AssetLibrary>) -> MutexGuard<'_, AssetLibrary> {
    library.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The Codex command line as the preferences name it.
fn codex_config(studio: &Studio) -> chatgpt::Config {
    chatgpt::Config {
        program: studio
            .prefs
            .ai
            .codex_program
            .as_deref()
            .map(str::trim)
            .filter(|program| !program.is_empty())
            .map(PathBuf::from),
        codex_home: None,
    }
}

/// The person's Google OAuth client, from the preferences.
fn drive_client(studio: &Studio) -> drive::Client {
    drive::Client {
        client_id: studio.prefs.ai.drive_client_id.clone(),
        client_secret: studio.prefs.ai.drive_client_secret.clone(),
    }
}

/// Settings, on the AI & cloud page.
fn open_settings(studio: &mut Studio) {
    studio.handle(Msg::Settings(SettingsMsg::Open));
    studio.handle(Msg::Settings(SettingsMsg::PageChanged(SETTINGS_PAGE)));
}

/// The bridge timer's tick: the connection dots, published when they move.
fn poll_bridge() {
    crate::host::Shell::with(|shell, app| {
        // A native dialog pumps timers while a handler holds the studio;
        // the dots can wait for the next tick.
        let Ok(mut studio) = shell.studio.try_borrow_mut() else {
            return;
        };
        let changed = studio.generate.poll_connected();
        drop(studio);
        if changed {
            shell.studio.borrow().publish(&app, &shell.models);
        }
    });
}

/// Follows a bridged job's events until it ends, taking each file it
/// brings into the library as it comes, and posting how it goes through
/// `post`. Returns how many were taken, or why the job failed; a cancel,
/// or the bridge stopping, is "cancelled".
fn follow_bridged(
    id: u64,
    events: &std::sync::mpsc::Receiver<Event>,
    cancel: &AtomicBool,
    library: &Mutex<AssetLibrary>,
    origin: &Origin,
    post: &dyn Fn(GenerateMsg),
) -> Result<usize, String> {
    let mut taken = 0;
    let report = |step: Step, fraction: Option<f32>| {
        post(GenerateMsg::Progress(id, step, fraction));
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_owned());
        }
        match events.recv_timeout(EVENT_SLICE) {
            Ok(Event::Taken) => report(Step::Taken, None),
            Ok(Event::Progress { message, fraction }) => report(Step::Running(message), fraction),
            Ok(Event::File(path)) => {
                let result = lock(library)
                    .take(&path, origin.clone(), false)
                    .map(|asset| asset.id);
                if result.is_ok() {
                    taken += 1;
                }
                post(GenerateMsg::Took(id, result));
            }
            Ok(Event::Done) => return Ok(taken),
            Ok(Event::Failed(error)) if taken > 0 => {
                log::warn!("bridge: the job failed after {taken} file(s): {error}");
                return Ok(taken);
            }
            Ok(Event::Failed(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("cancelled".to_owned()),
        }
    }
}

/// A provider as the picker and a job's row name it.
fn provider_name(provider: Provider) -> String {
    match provider {
        Provider::OpenRouter => t("generate.providerOpenRouter"),
        Provider::ChatGpt => t("generate.providerChatGpt"),
        Provider::ChatGptWeb => t("generate.providerChatGptWeb"),
        Provider::Flow => t("generate.providerFlow"),
    }
}

/// Who made an asset, as its card says it: the provider's short name, or
/// that it was imported.
fn badge(provider: &str) -> String {
    match provider {
        "openrouter" => "OpenRouter".to_owned(),
        "chatgpt" => "ChatGPT".to_owned(),
        "chatgpt-web" => "ChatGPT web".to_owned(),
        "flow" => "Google Flow".to_owned(),
        "import" | "" => t("generate.imported"),
        other => other.to_owned(),
    }
}

/// A job's row title: the first words of its prompt, and who is making it.
fn job_title(prompt: &str, provider: Provider) -> String {
    let words: String = prompt.chars().take(TITLE_CHARS).collect();
    let words = if prompt.chars().count() > TITLE_CHARS {
        format!("{}…", words.trim_end())
    } else {
        words
    };
    format!("{words} · {}", provider_name(provider))
}

/// The Codex line in Settings: its dot - 0 unknown, 1 ready, 2 an API
/// key, 3 signed out, 4 not installed - and its words.
fn codex_line(status: Option<&chatgpt::Status>, checking: bool) -> (i32, String) {
    match status {
        _ if checking => (0, t("settings.checking")),
        None => (0, t("settings.codexNotChecked")),
        Some(chatgpt::Status::Ready { version }) => (1, tf("settings.codexReady", &[version])),
        Some(chatgpt::Status::ApiKey { version }) => (2, tf("settings.codexApiKey", &[version])),
        Some(chatgpt::Status::LoggedOut { version }) => {
            (3, tf("settings.codexLoggedOut", &[version]))
        }
        Some(chatgpt::Status::Missing) => (4, t("settings.codexMissing")),
    }
}

/// A file's name, for a notice.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

impl Studio {
    /// What an `asset:` payload's card is, for the drop's plan: its name,
    /// whether it is a video, and its length when read.
    pub fn asset_summary(&self, id: &str) -> Option<(String, bool, Option<f64>)> {
        self.generate.summary(id)
    }

    /// A card dropped on a lane, or added at the playhead: copied into the
    /// project's `assets` folder and imported, then cut onto `track_id`
    /// at `start`, or at `start` on the first lane with room.
    pub fn place_asset(&mut self, id: &str, track_id: Option<String>, start: f64) {
        if let Some((name, path)) = self.generate.file_of(id) {
            self.place_file(&name, &path, PROJECT_FOLDER, track_id, start);
        }
    }

    /// Lets the window's close stop every generation, sign-in and sync,
    /// and the bridge.
    pub fn stop_generation(&mut self) {
        self.generate.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unindexed_generation_survives_cleanup_and_reopening() {
        let root = std::env::temp_dir().join(format!(
            "concat-generate-recovery-{}-{}",
            std::process::id(),
            random_hex(6)
        ));
        let dirs = AppDirs::under(&root);
        let out_dir = staging(&dirs).join("job");
        std::fs::create_dir_all(&out_dir).unwrap();
        let source = out_dir.join("made.png");
        let picture = b"\x89PNG\r\n\x1a\nrecoverable picture";
        std::fs::write(&source, picture).unwrap();

        let mut library = AssetLibrary::open(&dirs);
        std::fs::create_dir_all(library.dir.join("assets.json.partial")).unwrap();
        assert!(library.take(&source, Origin::default(), false).is_err());
        cleanup_staging(&out_dir);
        let reopened = GeneratePane::open(&dirs);

        assert!(
            reopened.shelf.is_empty(),
            "the failed import was not indexed"
        );
        assert_eq!(std::fs::read(&source).unwrap(), picture);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn empty_job_staging_is_removed() {
        let root = std::env::temp_dir().join(format!(
            "concat-generate-cleanup-{}-{}",
            std::process::id(),
            random_hex(6)
        ));
        let dirs = AppDirs::under(&root);
        let out_dir = staging(&dirs).join("job");
        std::fs::create_dir_all(&out_dir).unwrap();
        cleanup_staging(&out_dir);
        assert!(!out_dir.exists());
        cleanup_staging(&staging(&dirs));
        assert!(!staging(&dirs).exists());
        std::fs::remove_dir(&root).unwrap();
    }

    fn searchable_asset(id: &str, name: &str, prompt: &str, provider: &str) -> Asset {
        Asset {
            id: id.into(),
            name: name.into(),
            file: format!("{id}.png"),
            kind: MediaKind::Image,
            prompt: prompt.into(),
            provider: provider.into(),
            model: "Gemini".into(),
            added: 0,
            bytes: 0,
            sha256: String::new(),
            drive_id: None,
        }
    }

    #[test]
    fn searching_the_shelf_keeps_ids_order_and_the_full_library() {
        let mut pane = GeneratePane {
            shelf: vec![
                searchable_asset("new", "海邊日落", "A Quiet Ocean", "flow"),
                searchable_asset("old", "Forest", "Morning mist", "openrouter"),
            ],
            asset_query: "  OCEAN 海邊 google GEMINI  ".into(),
            ..GeneratePane::default()
        };
        let cards = pane.cards();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, "new");
        assert_eq!(cards[0].name, "海邊日落");
        assert_eq!(pane.shelf.len(), 2, "searching is only a view projection");

        pane.asset_query = "OpenRouter".into();
        assert_eq!(pane.cards()[0].id, "old");
        pane.asset_query = "  \t  ".into();
        let ids: Vec<String> = pane
            .cards()
            .iter()
            .map(|card| card.id.to_string())
            .collect();
        assert_eq!(ids, ["new", "old"]);
    }

    #[test]
    fn an_unmatched_shelf_search_does_not_remove_an_asset() {
        let pane = GeneratePane {
            shelf: vec![searchable_asset("held", "Forest", "Morning mist", "flow")],
            asset_query: "forest night".into(),
            ..GeneratePane::default()
        };
        assert!(pane.cards().is_empty(), "every search word must match");
        assert_eq!(pane.shelf[0].id, "held");
    }

    #[test]
    fn a_secret_never_reaches_a_debug_line() {
        let msg = GenerateMsg::OpenRouterKeyEdited(Secret("sk-or-v1-secret".into()));
        let text = format!("{msg:?}");
        assert!(!text.contains("sk-or"), "{text}");
        assert!(text.contains("OpenRouterKeyEdited"), "{text}");
        let msg = GenerateMsg::DriveClientSecretEdited(Secret("GOCSPX-x".into()));
        assert!(!format!("{msg:?}").contains("GOCSPX"));
    }

    #[test]
    fn the_picker_offers_what_the_library_takes() {
        let library: Vec<&str> = assets::IMAGE_EXTENSIONS
            .iter()
            .chain(assets::VIDEO_EXTENSIONS.iter())
            .copied()
            .collect();
        assert_eq!(PICK_EXTENSIONS.to_vec(), library);
    }

    #[test]
    fn a_job_title_is_the_start_of_its_prompt() {
        let short = job_title("a red fox", Provider::Flow);
        assert!(short.starts_with("a red fox · "), "{short}");
        let long = "word ".repeat(40);
        let title = job_title(&long, Provider::OpenRouter);
        let words = title.split(" · ").next().unwrap();
        assert!(words.ends_with('…'), "{title}");
        assert!(words.chars().count() <= TITLE_CHARS + 1, "{title}");
    }

    #[test]
    fn an_imported_asset_says_so_and_a_known_maker_by_name() {
        assert_eq!(badge("openrouter"), "OpenRouter");
        assert_eq!(badge("flow"), "Google Flow");
        assert_eq!(badge("someone-else"), "someone-else");
        assert_eq!(badge("import"), t("generate.imported"));
    }

    #[test]
    fn the_aspect_chips_start_with_the_providers_own_choice() {
        assert_eq!(ASPECTS[0], "");
        assert!(ASPECTS.contains(&"16:9") && ASPECTS.contains(&"9:16"));
    }

    #[test]
    fn the_codex_line_follows_the_status() {
        assert_eq!(codex_line(None, false).0, 0);
        assert_eq!(codex_line(None, true).0, 0);
        let ready = chatgpt::Status::Ready {
            version: "0.140.0".into(),
        };
        let (state, line) = codex_line(Some(&ready), false);
        assert_eq!(state, 1);
        assert!(line.contains("0.140.0"), "{line}");
        assert_eq!(codex_line(Some(&chatgpt::Status::Missing), false).0, 4);
        let out = chatgpt::Status::LoggedOut {
            version: "1".into(),
        };
        assert_eq!(codex_line(Some(&out), false).0, 3);
    }

    /// A bridged job's thread takes each file into the library as it
    /// comes, ends on Done with the count, and on Failed with nothing
    /// taken reports the reason; a cancel ends it as cancelled.
    #[test]
    fn a_bridged_job_takes_its_files_and_ends() {
        let root = std::env::temp_dir().join(format!(
            "concat-generate-bridged-{}-{}",
            std::process::id(),
            random_hex(6)
        ));
        let library = Mutex::new(AssetLibrary::at(root.join("library")));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("made.png");
        std::fs::write(&file, b"\x89PNG\r\n\x1a\nnot really a picture").unwrap();
        let origin = Origin {
            prompt: "a lighthouse at dusk".into(),
            provider: "flow".into(),
            ..Origin::default()
        };

        let (send, events) = std::sync::mpsc::channel();
        send.send(Event::Taken).unwrap();
        send.send(Event::File(file.clone())).unwrap();
        send.send(Event::Done).unwrap();
        let cancel = AtomicBool::new(false);
        let posted = std::cell::RefCell::new(Vec::new());
        let post = |msg: GenerateMsg| posted.borrow_mut().push(format!("{msg:?}"));
        assert_eq!(
            follow_bridged(1, &events, &cancel, &library, &origin, &post),
            Ok(1)
        );
        let posted = posted.into_inner();
        assert!(posted[0].starts_with("Progress(1, Taken"), "{posted:?}");
        assert!(posted[1].starts_with("Took(1, Ok("), "{posted:?}");
        let held = lock(&library).assets.clone();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].provider, "flow");
        assert_eq!(held[0].prompt, "a lighthouse at dusk");
        assert!(!file.exists(), "the file is moved, not copied");

        let (send, events) = std::sync::mpsc::channel();
        send.send(Event::Failed("the page said no".into())).unwrap();
        assert_eq!(
            follow_bridged(2, &events, &cancel, &library, &origin, &|_| {}),
            Err("the page said no".to_owned())
        );

        let (_send, events) = std::sync::mpsc::channel::<Event>();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            follow_bridged(3, &events, &cancel, &library, &origin, &|_| {}),
            Err("cancelled".to_owned())
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
