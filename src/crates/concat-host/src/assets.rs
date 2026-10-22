// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The user's own pictures and videos, kept across projects: the asset
//! library.
//!
//! What a generator makes, and what the person imports, is copied into
//! `<library>/assets/` (see [`AppDirs::library`]) under a random id rather
//! than the name it came with, so two projects that use the same picture
//! share one copy and no two files can ever clash. Beside the copies is
//! `assets.json`, the index: each asset's name, what made it, its size and
//! a SHA-256 of its bytes. The digest is what keeps the library from holding
//! the same picture twice, and what Google Drive sync (see `drive.rs`)
//! matches a file from another machine against.
//!
//! Nothing here touches a project; what a project keeps is the window's
//! business. The index is written through a temporary file and a rename,
//! the way the sound library writes its own.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AppDirs;
use crate::generate::{MediaKind, random_hex, sniff};

/// The still-picture extensions the library takes, by name.
pub const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff"];

/// The moving-picture extensions the library takes, by name.
pub const VIDEO_EXTENSIONS: [&str; 6] = ["mp4", "mov", "webm", "mkv", "m4v", "avi"];

/// The index's file name inside the library folder.
const INDEX: &str = "assets.json";

/// The longest name a prompt is turned into, in characters.
const NAME_CHARS: usize = 40;

/// The provider an asset is recorded with when nothing says who made it.
const IMPORT: &str = "import";

/// The kind of picture or video a path names, by its extension, in any case;
/// `None` for anything else.
pub fn kind_of(path: &Path) -> Option<MediaKind> {
    let extension = path.extension()?.to_str()?;
    if IMAGE_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
    {
        Some(MediaKind::Image)
    } else if VIDEO_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
    {
        Some(MediaKind::Video)
    } else {
        None
    }
}

/// Whether the library takes the file at `path`, by its extension.
pub fn is_asset(path: &Path) -> bool {
    kind_of(path).is_some()
}

/// One picture or video in the library.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    /// Stable for as long as the asset is in the library: random hex for
    /// one the library made, or the id a Drive copy came with.
    pub id: String,
    /// What the card says. The person may rename it.
    pub name: String,
    /// The copy's file name inside the library folder: `<id>.<extension>`.
    pub file: String,
    /// A picture or a video.
    pub kind: MediaKind,
    /// The prompt it was made from; empty for an import.
    #[serde(default)]
    pub prompt: String,
    /// `Provider::id()` of the maker, or `"import"` for a file brought in.
    #[serde(default)]
    pub provider: String,
    /// The model that made it, as the provider names it; empty when unknown.
    #[serde(default)]
    pub model: String,
    /// When it came in, as Unix seconds: the library's order, newest first.
    #[serde(default)]
    pub added: u64,
    /// The size of the copy, in bytes.
    #[serde(default)]
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the copy's bytes; empty when not known.
    #[serde(default)]
    pub sha256: String,
    /// The copy's id in Google Drive, once the sync has put it there.
    #[serde(default)]
    pub drive_id: Option<String>,
}

/// What is known about a file coming in: how the person named it and what
/// made it. Every field may be empty.
#[derive(Clone, Debug, Default)]
pub struct Origin {
    /// The card's name. Empty derives it: from the prompt's first words,
    /// else from the file's own name.
    pub name: String,
    /// The prompt it was made from; empty for an import.
    pub prompt: String,
    /// `Provider::id()` of the maker; empty records `"import"`.
    pub provider: String,
    /// The model that made it, as the provider names it.
    pub model: String,
}

/// The index on disk.
#[derive(Serialize, Deserialize, Default)]
struct Index {
    #[serde(default)]
    assets: Vec<Asset>,
}

/// The library: its folder and what is in it.
#[derive(Clone, Debug, Default)]
pub struct AssetLibrary {
    /// The folder: the copies and the index beside them.
    pub dir: PathBuf,
    /// Newest first.
    pub assets: Vec<Asset>,
}

impl AssetLibrary {
    /// Where the library lives on this machine.
    pub fn dir_in(dirs: &AppDirs) -> PathBuf {
        dirs.library().join("assets")
    }

    /// The library as the index has it. A missing or unreadable index is an
    /// empty library, and an asset whose copy has gone is left out.
    pub fn open(dirs: &AppDirs) -> AssetLibrary {
        AssetLibrary::at(Self::dir_in(dirs))
    }

    /// The library in `dir`; see [`AssetLibrary::open`].
    pub fn at(dir: PathBuf) -> AssetLibrary {
        let index: Index = std::fs::read(dir.join(INDEX))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let assets = index
            .assets
            .into_iter()
            .filter(|asset| is_plain_name(&asset.file) && dir.join(&asset.file).is_file())
            .collect();
        AssetLibrary { dir, assets }
    }

    /// The copy of `asset` on disk.
    pub fn path_of(&self, asset: &Asset) -> PathBuf {
        self.dir.join(&asset.file)
    }

    /// The asset by its id.
    pub fn get(&self, id: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.id == id)
    }

    /// The asset whose bytes have this SHA-256 (any case), if one is held.
    pub fn by_sha256(&self, sha: &str) -> Option<&Asset> {
        self.position_of_sha(sha).map(|at| &self.assets[at])
    }

    /// Takes in the file at `source` as a new asset made or imported as
    /// `origin` says.
    ///
    /// The kind and the extension come from the file's name, or, for a name
    /// that is not a picture or video format, from its first bytes; a file
    /// that is neither is refused before anything is copied. When the library
    /// already holds the same bytes, that asset is returned unchanged and
    /// nothing is stored twice. With `keep_source` false the file is moved
    /// rather than copied.
    pub fn take(
        &mut self,
        source: &Path,
        origin: Origin,
        keep_source: bool,
    ) -> Result<Asset, String> {
        let (extension, kind) = classify(source)?;
        let (sha256, bytes) = hash_file(source)?;
        if let Some(at) = self.position_of_sha(&sha256) {
            let held = self.assets[at].clone();
            self.discard_duplicate(source, keep_source, &held)?;
            return Ok(held);
        }
        let id = random_hex(16);
        let file = format!("{id}.{extension}");
        let name = derive_name(&origin.name, &origin.prompt, source);
        let asset = Asset {
            id,
            name,
            file,
            kind,
            prompt: origin.prompt,
            provider: provider_or_import(&origin.provider),
            model: origin.model,
            added: now(),
            bytes,
            sha256,
            drive_id: None,
        };
        self.index_new(source, asset, keep_source)
    }

    /// Takes in the file at `source` as `template` says it is: the template's
    /// id, name, prompt, provider, model, added time and Drive id are kept,
    /// and its kind is the template's. This is how a copy brought back from
    /// Google Drive, made on another machine, joins the library under the
    /// id it already has.
    ///
    /// Idempotent. A template whose id is held is answered with that asset.
    /// A file whose bytes are held under another id is answered with that
    /// asset, which takes the template's Drive id when it has none. The
    /// template's `file`, `bytes` and `sha256` are not trusted: the file on
    /// disk decides them.
    pub fn take_as(
        &mut self,
        source: &Path,
        template: Asset,
        keep_source: bool,
    ) -> Result<Asset, String> {
        if !is_plain_id(&template.id) {
            return Err(format!("{:?} is not an asset id", template.id));
        }
        let Asset {
            id,
            name,
            kind,
            prompt,
            provider,
            model,
            added,
            drive_id,
            ..
        } = template;
        let drive_id = normalise_drive_id(drive_id);
        if let Some(held) = self.get(&id).cloned() {
            self.discard_duplicate(source, keep_source, &held)?;
            return Ok(held);
        }
        let (extension, _) = classify(source)?;
        let (sha256, bytes) = hash_file(source)?;
        if let Some(at) = self.position_of_sha(&sha256) {
            if drive_id.is_some() && self.assets[at].drive_id.is_none() {
                self.assets[at].drive_id = drive_id;
                if let Err(error) = self.save() {
                    self.assets[at].drive_id = None;
                    return Err(error);
                }
            }
            let held = self.assets[at].clone();
            self.discard_duplicate(source, keep_source, &held)?;
            return Ok(held);
        }
        let file = format!("{id}.{extension}");
        let name = derive_name(&name, &prompt, source);
        let asset = Asset {
            id,
            name,
            file,
            kind,
            prompt,
            provider: provider_or_import(&provider),
            model,
            added,
            bytes,
            sha256,
            drive_id,
        };
        self.index_new(source, asset, keep_source)
    }

    /// Deletes the copy and forgets the asset. A copy already gone is not an
    /// error: what the person asked for is that it be gone.
    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        let Some(at) = self.position_of(id) else {
            return Ok(());
        };
        let asset = self.assets.remove(at);
        let path = self.path_of(&asset);
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            self.assets.insert(at, asset);
            return Err(format!("could not delete {}: {error}", path.display()));
        }
        self.save()
    }

    /// Gives the asset a new name, trimmed. An empty name is refused, and an
    /// id the library does not hold is an error: the card the person is
    /// renaming is not there.
    pub fn rename(&mut self, id: &str, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("an asset needs a name".to_owned());
        }
        let at = self.require(id)?;
        let previous = std::mem::replace(&mut self.assets[at].name, name.to_owned());
        if let Err(error) = self.save() {
            self.assets[at].name = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Records the asset's id in Google Drive, or forgets it with `None`. A
    /// blank id counts as none. An id the library does not hold is an error.
    pub fn set_drive_id(&mut self, id: &str, drive_id: Option<String>) -> Result<(), String> {
        let at = self.require(id)?;
        let drive_id = normalise_drive_id(drive_id);
        let previous = std::mem::replace(&mut self.assets[at].drive_id, drive_id);
        if let Err(error) = self.save() {
            self.assets[at].drive_id = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Where the asset with this id sits in the list.
    fn position_of(&self, id: &str) -> Option<usize> {
        self.assets.iter().position(|asset| asset.id == id)
    }

    /// Where the asset with this id sits in the list, or an error naming it.
    fn require(&self, id: &str) -> Result<usize, String> {
        self.position_of(id)
            .ok_or_else(|| format!("there is no asset {id} in the library"))
    }

    /// Where the asset with these bytes sits in the list, if held.
    fn position_of_sha(&self, sha: &str) -> Option<usize> {
        if sha.is_empty() {
            return None;
        }
        self.assets
            .iter()
            .position(|asset| asset.sha256.eq_ignore_ascii_case(sha))
    }

    /// Makes the copy of `asset` from `source`, puts it at the front of the
    /// list and writes the index. If the index cannot be written, the list is
    /// left as it was; a copy is removed, while a moved file stays where it
    /// went, so nothing the person had is lost.
    fn index_new(
        &mut self,
        source: &Path,
        asset: Asset,
        keep_source: bool,
    ) -> Result<Asset, String> {
        self.ensure_dir()?;
        let target = self.path_of(&asset);
        place(source, &target, keep_source)?;
        self.assets.insert(0, asset.clone());
        if let Err(error) = self.save() {
            self.assets.remove(0);
            if keep_source {
                let _ = std::fs::remove_file(&target);
            }
            return Err(error);
        }
        Ok(asset)
    }

    /// Deletes a duplicate's source when the caller asked for a move. The
    /// source is left alone when it is the held copy itself, so importing a
    /// file out of the library cannot delete it.
    fn discard_duplicate(
        &self,
        source: &Path,
        keep_source: bool,
        held: &Asset,
    ) -> Result<(), String> {
        if keep_source || is_same_file(source, &self.path_of(held)) {
            return Ok(());
        }
        discard(source)
    }

    /// Creates the library folder, refusing a library that has none.
    fn ensure_dir(&self) -> Result<(), String> {
        if self.dir.as_os_str().is_empty() {
            return Err("the asset library has no folder".to_owned());
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("could not create {}: {error}", self.dir.display()))
    }

    /// Writes the index, through a temporary file and a rename, so a crash
    /// mid-write leaves the last one whole.
    fn save(&self) -> Result<(), String> {
        self.ensure_dir()?;
        let index = Index {
            assets: self.assets.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&index).map_err(|error| error.to_string())?;
        let path = self.dir.join(INDEX);
        let partial = self.dir.join(format!("{INDEX}.partial"));
        std::fs::write(&partial, bytes)
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|error| format!("could not write {}: {error}", path.display()))
    }
}

/// The SHA-256 of the file at `path`, as lowercase hex. Streamed, so a long
/// video is never read into memory whole.
pub fn sha256_of(path: &Path) -> Result<String, String> {
    hash_file(path).map(|(sha256, _)| sha256)
}

/// The SHA-256 of the file at `path` and its size: one pass over the bytes.
fn hash_file(path: &Path) -> Result<(String, u64), String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut handle = std::fs::File::open(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let read = handle
            .read(&mut buffer)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((digest, total))
}

/// The extension a file is kept under, lowercase, and its kind: from its
/// name when that is a picture or video format, else from its first bytes.
fn classify(source: &Path) -> Result<(String, MediaKind), String> {
    if let (Some(kind), Some(extension)) = (
        kind_of(source),
        source.extension().and_then(|extension| extension.to_str()),
    ) {
        return Ok((extension.to_ascii_lowercase(), kind));
    }
    match sniff(&read_head(source)?) {
        Some((extension, kind)) => Ok((extension.to_owned(), kind)),
        None => Err(format!("{} is not a picture or a video", source.display())),
    }
}

/// The first bytes of a file: enough for [`sniff`] to know its format.
fn read_head(source: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let file = std::fs::File::open(source)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?;
    let mut head = Vec::with_capacity(32);
    file.take(32)
        .read_to_end(&mut head)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?;
    Ok(head)
}

/// Puts the file at `source` at `target`: a copy when the source stays, else
/// a move. A move that a rename cannot make, across volumes, is a copy and a
/// delete of the source.
fn place(source: &Path, target: &Path, keep_source: bool) -> Result<(), String> {
    if keep_source {
        return copy(source, target);
    }
    if std::fs::rename(source, target).is_ok() {
        return Ok(());
    }
    copy(source, target)?;
    if let Err(error) = std::fs::remove_file(source) {
        let _ = std::fs::remove_file(target);
        return Err(format!("could not move {}: {error}", source.display()));
    }
    Ok(())
}

/// Copies `source` to `target`, removing a part-written `target` on failure.
fn copy(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::copy(source, target).map(|_| ()).map_err(|error| {
        let _ = std::fs::remove_file(target);
        format!("could not copy {}: {error}", source.display())
    })
}

/// Deletes `source`. A file already gone is not an error.
fn discard(source: &Path) -> Result<(), String> {
    match std::fs::remove_file(source) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not delete {}: {error}", source.display())),
    }
}

/// Whether two paths name the same file, once resolved. A path that cannot
/// be resolved names nothing.
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The name a card shows: the given name when there is one, else the first
/// words of the prompt, else the file's own name without its extension.
fn derive_name(name: &str, prompt: &str, source: &Path) -> String {
    let name = name.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    let from_prompt = prompt_name(prompt);
    if !from_prompt.is_empty() {
        return from_prompt;
    }
    source
        .file_stem()
        .map(|stem| stem.to_string_lossy().trim().to_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "Asset".to_owned())
}

/// The first words of a prompt, as many as fit in [`NAME_CHARS`] characters,
/// cut on a word boundary. A first word too long to fit is cut on its
/// characters instead.
fn prompt_name(prompt: &str) -> String {
    let mut name = String::new();
    for word in prompt.split_whitespace() {
        let next = if name.is_empty() {
            word.to_owned()
        } else {
            format!("{name} {word}")
        };
        if next.chars().count() > NAME_CHARS {
            break;
        }
        name = next;
    }
    if name.is_empty() {
        name = prompt
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .chars()
            .take(NAME_CHARS)
            .collect();
    }
    name
}

/// The provider to record: the one given, trimmed, or `"import"` for none.
fn provider_or_import(provider: &str) -> String {
    let provider = provider.trim();
    if provider.is_empty() {
        IMPORT.to_owned()
    } else {
        provider.to_owned()
    }
}

/// A Drive id worth keeping: a blank one is none.
fn normalise_drive_id(drive_id: Option<String>) -> Option<String> {
    drive_id
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// A file name with nothing in it that could walk out of a folder.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && Path::new(name).file_name().is_some_and(|file| file == name)
}

/// An id that is safe to build a file name from: letters, digits, `-`, `_`.
fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Just enough of a PNG to be sniffed as one.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake picture";
    /// Just enough of an MP4 to be sniffed as one.
    const MP4: &[u8] = b"\0\0\0\x18ftypmp42fake video";

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp dir")
    }

    /// Writes `bytes` to `path`, making its folder, and returns `path`.
    fn write(path: &Path, bytes: &[u8]) -> PathBuf {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("a folder");
        }
        std::fs::write(path, bytes).expect("a file");
        path.to_path_buf()
    }

    /// The library of a test's scratch folder: `assets/` in it.
    fn library_in(root: &tempfile::TempDir) -> AssetLibrary {
        AssetLibrary::at(root.path().join("assets"))
    }

    fn origin(prompt: &str) -> Origin {
        Origin {
            prompt: prompt.to_owned(),
            provider: "openrouter".to_owned(),
            model: "some/model".to_owned(),
            ..Origin::default()
        }
    }

    /// A template as a Drive copy from another machine would bring it.
    fn remote(id: &str, kind: MediaKind) -> Asset {
        Asset {
            id: id.to_owned(),
            name: "Remote clip".to_owned(),
            file: String::new(),
            kind,
            prompt: "a fox".to_owned(),
            provider: "flow".to_owned(),
            model: "veo".to_owned(),
            added: 1_700_000_000,
            bytes: 0,
            sha256: String::new(),
            drive_id: Some("drive-1".to_owned()),
        }
    }

    #[test]
    fn take_copies_indexes_and_reopens_from_disk() {
        let root = scratch();
        let source = write(&root.path().join("inbox/fox.png"), PNG);
        let mut library = library_in(&root);

        let asset = library
            .take(&source, origin("a red fox"), true)
            .expect("taken");

        assert_eq!(asset.name, "a red fox");
        assert_eq!(asset.kind, MediaKind::Image);
        assert_eq!(asset.provider, "openrouter");
        assert_eq!(asset.model, "some/model");
        assert_eq!(asset.bytes, PNG.len() as u64);
        assert_eq!(asset.sha256, sha256_of(&source).expect("a digest"));
        assert!(asset.file.ends_with(".png"), "{}", asset.file);
        assert!(source.is_file(), "a copy keeps the source");
        assert_eq!(
            std::fs::read(library.path_of(&asset)).expect("the copy"),
            PNG
        );
        assert_eq!(library.get(&asset.id), Some(&asset));
        assert_eq!(library.by_sha256(&asset.sha256), Some(&asset));

        let reopened = library_in(&root);
        assert_eq!(reopened.assets, vec![asset]);
    }

    #[test]
    fn a_duplicate_is_not_stored_twice() {
        let root = scratch();
        let first = write(&root.path().join("inbox/one.png"), PNG);
        let second = write(&root.path().join("inbox/two.PNG"), PNG);
        let mut library = library_in(&root);
        let kept = library.take(&first, origin("fox"), true).expect("taken");

        let again = library
            .take(&second, origin("another name"), false)
            .expect("the held asset");

        assert_eq!(again, kept);
        assert_eq!(library.assets.len(), 1);
        assert!(!second.exists(), "a duplicate moved in is removed");
        assert!(first.is_file());
        assert!(library.path_of(&kept).is_file());
    }

    #[test]
    fn a_file_already_in_the_library_is_not_deleted_as_a_duplicate() {
        let root = scratch();
        let source = write(&root.path().join("inbox/fox.png"), PNG);
        let mut library = library_in(&root);
        let kept = library.take(&source, origin("fox"), true).expect("taken");

        let copy = library.path_of(&kept);
        let same = library
            .take(&copy, Origin::default(), false)
            .expect("the held asset");

        assert_eq!(same, kept);
        assert!(copy.is_file(), "the held copy is not its own duplicate");
    }

    #[test]
    fn a_move_takes_the_source_away() {
        let root = scratch();
        let source = write(&root.path().join("inbox/clip.mp4"), MP4);
        let mut library = library_in(&root);

        let asset = library
            .take(&source, Origin::default(), false)
            .expect("taken");

        assert!(!source.exists(), "a move leaves nothing behind");
        assert_eq!(
            std::fs::read(library.path_of(&asset)).expect("the moved file"),
            MP4
        );
        assert_eq!(asset.kind, MediaKind::Video);
        assert_eq!(asset.provider, "import");
        assert_eq!(asset.name, "clip", "no prompt and no name: the file's own");
    }

    #[test]
    fn name_comes_from_the_given_name_the_prompt_or_the_file() {
        let root = scratch();
        let mut library = library_in(&root);
        let first = write(
            &root.path().join("inbox/Golden Hour.jpg"),
            b"\xff\xd8\xff\xe0one",
        );
        let second = write(&root.path().join("inbox/Other.jpg"), b"\xff\xd8\xff\xe0two");
        let third = write(
            &root.path().join("inbox/Third.jpg"),
            b"\xff\xd8\xff\xe0three",
        );

        let from_file = library
            .take(&first, Origin::default(), true)
            .expect("taken");
        assert_eq!(from_file.name, "Golden Hour");

        let from_prompt = library
            .take(&second, origin("  a   sunset over  the sea "), true)
            .expect("taken");
        assert_eq!(from_prompt.name, "a sunset over the sea");

        let given = library
            .take(
                &third,
                Origin {
                    name: "  Hero shot  ".to_owned(),
                    prompt: "ignored".to_owned(),
                    ..Origin::default()
                },
                true,
            )
            .expect("taken");
        assert_eq!(given.name, "Hero shot");
    }

    #[test]
    fn prompt_names_stop_on_a_word_boundary() {
        let long = "one two three four five six seven eight nine ten eleven twelve";
        let name = prompt_name(long);
        assert_eq!(name, "one two three four five six seven eight");
        assert!(name.chars().count() <= NAME_CHARS);
        assert!(long[name.len()..].starts_with(' '), "cut between two words");

        let single = prompt_name(&"x".repeat(60));
        assert_eq!(single.chars().count(), NAME_CHARS);
        assert_eq!(prompt_name("   "), "");
    }

    #[test]
    fn an_unknown_file_is_refused_before_anything_is_copied() {
        let root = scratch();
        let source = write(&root.path().join("inbox/notes.txt"), b"hello");
        let mut library = library_in(&root);

        let error = library
            .take(&source, origin("x"), false)
            .expect_err("refused");

        assert!(error.contains("not a picture or a video"), "{error}");
        assert!(source.is_file());
        assert!(library.assets.is_empty());
        assert!(!root.path().join("assets").exists());
    }

    #[test]
    fn an_unknown_extension_is_sniffed_from_its_bytes() {
        let root = scratch();
        let source = write(&root.path().join("inbox/blob"), MP4);
        let mut library = library_in(&root);

        let asset = library
            .take(&source, Origin::default(), true)
            .expect("taken");

        assert_eq!(asset.kind, MediaKind::Video);
        assert!(asset.file.ends_with(".mp4"), "{}", asset.file);
    }

    #[test]
    fn take_as_keeps_the_id_and_is_idempotent() {
        let root = scratch();
        let source = write(&root.path().join("inbox/remote.mp4"), MP4);
        let mut library = library_in(&root);
        let template = remote("a1b2c3", MediaKind::Video);

        let first = library
            .take_as(&source, template.clone(), true)
            .expect("taken");

        assert_eq!(first.id, "a1b2c3");
        assert_eq!(first.file, "a1b2c3.mp4");
        assert_eq!(first.name, "Remote clip");
        assert_eq!(first.kind, MediaKind::Video);
        assert_eq!(first.added, 1_700_000_000);
        assert_eq!(first.drive_id.as_deref(), Some("drive-1"));
        assert_eq!(first.sha256, sha256_of(&source).expect("a digest"));

        let again = library
            .take_as(&source, template, true)
            .expect("the same asset");
        assert_eq!(again, first);
        assert_eq!(library.assets.len(), 1);
        assert_eq!(library_in(&root).assets, vec![first]);

        assert!(
            library
                .take_as(&source, remote("../escape", MediaKind::Video), true)
                .is_err()
        );
    }

    #[test]
    fn take_as_marks_a_held_duplicate_with_its_drive_id() {
        let root = scratch();
        let local = write(&root.path().join("inbox/local.png"), PNG);
        let download = write(&root.path().join("inbox/download.png"), PNG);
        let mut library = library_in(&root);
        let held = library.take(&local, origin("fox"), true).expect("taken");
        assert_eq!(held.drive_id, None);

        let got = library
            .take_as(&download, remote("zz99", MediaKind::Image), false)
            .expect("the held asset");

        assert_eq!(got.id, held.id);
        assert_eq!(got.drive_id.as_deref(), Some("drive-1"));
        assert!(!download.exists());
        let reopened = library_in(&root);
        assert_eq!(
            reopened
                .get(&held.id)
                .and_then(|asset| asset.drive_id.as_deref()),
            Some("drive-1")
        );

        library
            .set_drive_id(&held.id, Some("mine".to_owned()))
            .expect("set");
        let other = write(&root.path().join("inbox/again.png"), PNG);
        let got = library
            .take_as(&other, remote("yy88", MediaKind::Image), true)
            .expect("the held asset");
        assert_eq!(
            got.drive_id.as_deref(),
            Some("mine"),
            "a Drive id already held is kept"
        );
        assert_eq!(library.assets.len(), 1);
    }

    #[test]
    fn rename_remove_and_drive_id_persist() {
        let root = scratch();
        let source = write(&root.path().join("inbox/pic.png"), PNG);
        let mut library = library_in(&root);
        let asset = library.take(&source, origin("fox"), true).expect("taken");

        library.rename(&asset.id, "  Hero shot ").expect("renamed");
        assert_eq!(
            library_in(&root).get(&asset.id).expect("held").name,
            "Hero shot"
        );
        assert!(library.rename(&asset.id, "   ").is_err());
        assert_eq!(library.get(&asset.id).expect("held").name, "Hero shot");
        assert!(library.rename("nope", "x").is_err());

        library
            .set_drive_id(&asset.id, Some("d1".to_owned()))
            .expect("set");
        let reopened = library_in(&root);
        assert_eq!(
            reopened
                .get(&asset.id)
                .and_then(|held| held.drive_id.as_deref()),
            Some("d1")
        );
        library
            .set_drive_id(&asset.id, Some("  ".to_owned()))
            .expect("cleared");
        assert_eq!(
            library_in(&root).get(&asset.id).expect("held").drive_id,
            None
        );

        let copy = library.path_of(&asset);
        library.remove(&asset.id).expect("removed");
        assert!(!copy.exists());
        assert!(library_in(&root).assets.is_empty());
        library
            .remove(&asset.id)
            .expect("removing it again is fine");
    }

    #[test]
    fn a_missing_file_is_dropped_on_open_and_so_is_a_name_that_walks_out() {
        let root = scratch();
        let source = write(&root.path().join("inbox/pic.png"), PNG);
        let mut library = library_in(&root);
        let asset = library.take(&source, origin("fox"), true).expect("taken");
        std::fs::remove_file(library.path_of(&asset)).expect("gone");
        assert!(library_in(&root).assets.is_empty());

        write(&root.path().join("escape.png"), PNG);
        write(
            &root.path().join("assets/assets.json"),
            br#"{"assets":[{"id":"x","name":"x","file":"../escape.png","kind":"image"}]}"#,
        );
        assert!(library_in(&root).assets.is_empty());
    }

    #[test]
    fn kinds_and_asset_checks_ignore_case() {
        assert_eq!(kind_of(Path::new("a.PNG")), Some(MediaKind::Image));
        assert_eq!(kind_of(Path::new("b.MoV")), Some(MediaKind::Video));
        assert_eq!(kind_of(Path::new("c.txt")), None);
        assert_eq!(kind_of(Path::new("noext")), None);
        assert!(is_asset(Path::new("x.JPEG")));
        assert!(is_asset(Path::new("dir/x.m4v")));
        assert!(!is_asset(Path::new("x.mp3")));
    }

    #[test]
    fn sha256_of_is_the_digest_of_the_bytes() {
        let root = scratch();
        let file = write(&root.path().join("abc.bin"), b"abc");
        assert_eq!(
            sha256_of(&file).expect("a digest"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(sha256_of(&root.path().join("missing")).is_err());
    }
}
