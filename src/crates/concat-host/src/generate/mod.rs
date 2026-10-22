// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Pictures and video made from a prompt, by a service the person has an
//! account with.
//!
//! Three ways in, one shape out. OpenRouter is an HTTP API paid for with a
//! key ([`openrouter`]). ChatGPT is the person's subscription, reached
//! through OpenAI's own Codex command line signed in with it ([`chatgpt`]).
//! Google Flow, and ChatGPT's own web page, have no API at all: a browser
//! extension drives the page the person is signed in to and hands what it
//! made back over the bridge (see [`crate::bridge`]).
//!
//! Every way writes its files into an `out_dir` the caller names and
//! returns them as [`Output`]s; the caller takes them into the asset
//! library (see [`crate::assets`]). Nothing here touches a project or a
//! window. Long work reports through a `progress` callback and stops when
//! `cancel` is set, the way the rest of the host does.

pub mod chatgpt;
pub mod openrouter;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a request makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// A still picture.
    #[default]
    Image,
    /// A moving one.
    Video,
}

impl MediaKind {
    /// The name the index and the bridge write: "image" or "video".
    pub fn id(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
        }
    }

    /// The kind [`MediaKind::id`] names.
    pub fn from_id(id: &str) -> Option<MediaKind> {
        match id {
            "image" => Some(MediaKind::Image),
            "video" => Some(MediaKind::Video),
            _ => None,
        }
    }
}

/// Who makes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// OpenRouter's image and video APIs, with the person's key.
    #[default]
    OpenRouter,
    /// ChatGPT through the Codex command line signed in with the person's
    /// subscription: pictures only.
    ChatGpt,
    /// ChatGPT's web page, driven by the browser extension: pictures only.
    ChatGptWeb,
    /// Google Flow's web page, driven by the browser extension.
    Flow,
}

impl Provider {
    /// Every provider, in the order a picker lists them.
    pub const ALL: [Provider; 4] = [
        Provider::OpenRouter,
        Provider::ChatGpt,
        Provider::ChatGptWeb,
        Provider::Flow,
    ];

    /// The name the index, the preferences and the bridge write.
    pub fn id(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::ChatGpt => "chatgpt",
            Provider::ChatGptWeb => "chatgpt-web",
            Provider::Flow => "flow",
        }
    }

    /// The provider [`Provider::id`] names.
    pub fn from_id(id: &str) -> Option<Provider> {
        Provider::ALL
            .into_iter()
            .find(|provider| provider.id() == id)
    }

    /// What it can make.
    pub fn kinds(self) -> &'static [MediaKind] {
        match self {
            Provider::OpenRouter | Provider::Flow => &[MediaKind::Image, MediaKind::Video],
            Provider::ChatGpt | Provider::ChatGptWeb => &[MediaKind::Image],
        }
    }

    /// Whether it works through the browser extension rather than in this
    /// process.
    pub fn bridged(self) -> bool {
        matches!(self, Provider::ChatGptWeb | Provider::Flow)
    }
}

/// One thing to make.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Request {
    /// A picture or a video.
    pub kind: MediaKind,
    /// What to make, in the person's words.
    pub prompt: String,
    /// The model, as the provider names it (`google/veo-3.1`); empty is the
    /// provider's own default.
    pub model: String,
    /// "16:9", "9:16", "1:1", ...; empty is the provider's default.
    pub aspect_ratio: String,
    /// "720p", "1080p", "1K", "2K", ...; empty is the provider's default.
    pub resolution: String,
    /// Seconds of video; 0 is the provider's default. Ignored for a picture.
    pub duration: u32,
    /// How many to make; 0 is one.
    pub count: u32,
    /// A picture to start from: the first frame of a video, or the picture
    /// an edit begins with. Not every provider takes one.
    pub reference: Option<PathBuf>,
}

impl Request {
    /// How many to make, at least one.
    pub fn count(&self) -> u32 {
        self.count.max(1)
    }
}

/// One finished file.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// Where it was written, inside the `out_dir` the caller gave.
    pub path: PathBuf,
    /// A picture or a video.
    pub kind: MediaKind,
    /// The model that made it, as the provider reported it, or as asked.
    pub model: String,
}

/// How far a request has got, for the page to show.
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    /// Sent, waiting to start.
    Queued,
    /// Under way; `fraction` when the provider says how far, `0..=1`.
    Running {
        /// How far, when known.
        fraction: Option<f32>,
        /// What it is doing, in a few words, for the status line.
        message: String,
    },
    /// The file is coming down.
    Downloading,
}

/// The extension a file of this media type is written with: what the
/// providers' answers name and what the asset library keys pictures and
/// videos by.
pub fn extension_for(media_type: &str) -> Option<&'static str> {
    let essence = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    Some(match essence.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        _ => return None,
    })
}

/// The kind of file the first bytes are, for an answer that names none:
/// its extension and whether it is a picture or a video.
pub fn sniff(bytes: &[u8]) -> Option<(&'static str, MediaKind)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", MediaKind::Image))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("jpg", MediaKind::Image))
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("webp", MediaKind::Image))
    } else if bytes.starts_with(b"GIF8") {
        Some(("gif", MediaKind::Image))
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        if &bytes[8..12] == b"qt  " {
            Some(("mov", MediaKind::Video))
        } else {
            Some(("mp4", MediaKind::Video))
        }
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        Some(("webm", MediaKind::Video))
    } else {
        None
    }
}

/// A fresh random name, `len` hex digits long: a job's id, a file's stem.
pub fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len.div_ceil(2)];
    if getrandom::fill(&mut bytes).is_err() {
        // No random source is a broken platform; a clock-and-counter name
        // is still unique in this process, which is all a file name needs.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or_default();
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let seed = format!("{now:x}{n:x}{:x}", std::process::id());
        bytes = seed.into_bytes();
    }
    let mut text: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    text.truncate(len);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for provider in Provider::ALL {
            assert_eq!(Provider::from_id(provider.id()), Some(provider));
        }
        assert_eq!(MediaKind::from_id("video"), Some(MediaKind::Video));
        assert_eq!(MediaKind::from_id("audio"), None);
    }

    #[test]
    fn sniffs_the_common_formats() {
        assert_eq!(
            sniff(b"\x89PNG\r\n\x1a\nrest"),
            Some(("png", MediaKind::Image))
        );
        assert_eq!(
            sniff(b"\0\0\0\x18ftypmp42rest"),
            Some(("mp4", MediaKind::Video))
        );
        assert_eq!(sniff(b"hello"), None);
        assert_eq!(extension_for("image/jpeg; charset=x"), Some("jpg"));
    }

    #[test]
    fn random_names_are_hex_and_differ() {
        let a = random_hex(32);
        let b = random_hex(32);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
