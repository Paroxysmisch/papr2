//! The research library, stored as plain files and folders.
//!
//! The file system is the source of truth, so everything can also be managed
//! from a file manager:
//!
//! ```text
//! ~/Documents/papr2/                 (override with PAPR2_HOME)
//!   Library/
//!     Attention Is All You Need.pdf  the one real copy of every paper;
//!     ...                            the file name is the paper's name
//!   Collections/
//!     Thesis/                        one folder per collection
//!       Attention Is All You Need.pdf -> ../../Library/Attention Is All You Need.pdf
//!   .papr2-cache.json                hidden cache, safe to delete
//! ```
//!
//! Collections hold relative symbolic links into `Library/`, never copies.
//! Changes made outside the app are picked up by [`Library::refresh`]:
//!
//! * rename a PDF in `Library/`    → the paper is renamed, collection links are repaired
//! * rename a link in a collection → the paper is renamed everywhere
//! * rename a collection folder    → the collection is renamed
//! * delete a link                 → the paper leaves that collection
//! * delete a PDF from `Library/`  → the paper is removed, links are cleaned up
//! * put a PDF in `Library/`       → it's added to the library
//! * put a PDF in a collection     → it's moved into `Library/` and linked back
//!
//! The cache only remembers file identities (inode, size, mtime) and content
//! hashes, so renames can be told apart from deletions.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// A paper is identified by its file name inside `Library/`.
pub type PaperId = String;
/// A collection is identified by its folder name inside `Collections/`.
pub type CollectionId = String;

const LIBRARY_DIR: &str = "Library";
const COLLECTIONS_DIR: &str = "Collections";
const CACHE_FILE: &str = ".papr2-cache.json";

#[derive(Debug, Clone, PartialEq)]
pub struct Paper {
    /// File name in `Library/`, e.g. `Attention Is All You Need.pdf`.
    pub id: PaperId,
    /// Display name: the file name without `.pdf`.
    pub title: String,
    pub size_bytes: u64,
    /// Last modification, seconds since the Unix epoch.
    pub modified: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Collection {
    /// Folder name in `Collections/`.
    pub id: CollectionId,
    /// Papers linked from this folder, sorted.
    pub papers: Vec<PaperId>,
}

impl Collection {
    pub fn name(&self) -> &str {
        &self.id
    }
}

pub enum ImportOutcome {
    Added(PaperId),
    /// A file with identical contents is already in the library.
    Duplicate(PaperId),
}

/// What changed during a [`Library::refresh`].
#[derive(Debug, Default)]
pub struct ScanReport {
    /// The visible state (papers, collections, membership) changed.
    pub changed: bool,
    /// `(old id, new id)` of papers renamed since the last scan.
    pub renamed_papers: Vec<(PaperId, PaperId)>,
    /// `(old id, new id)` of collections renamed since the last scan.
    pub renamed_collections: Vec<(CollectionId, CollectionId)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct FileInfo {
    /// Inode number (0 where unavailable).
    ino: u64,
    size: u64,
    mtime: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

impl FileInfo {
    fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            ino: inode(meta),
            size: meta.len(),
            mtime: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            sha256: None,
        }
    }

    /// Whether two observations are of the same file (e.g. across a rename).
    fn same_file(&self, other: &FileInfo) -> bool {
        if self.ino != 0 && other.ino != 0 {
            self.ino == other.ino && self.size == other.size
        } else {
            self.size == other.size && self.mtime == other.mtime
        }
    }
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cache {
    papers: BTreeMap<PaperId, FileInfo>,
    /// Collection folder name → inode, to follow folder renames.
    collections: BTreeMap<CollectionId, u64>,
}

pub struct Library {
    root: PathBuf,
    papers: Vec<Paper>,
    collections: Vec<Collection>,
    cache: Cache,
}

impl Library {
    /// `$PAPR2_HOME`, or `~/Documents/papr2`.
    pub fn default_root() -> PathBuf {
        if let Some(home) = std::env::var_os("PAPR2_HOME") {
            return PathBuf::from(home);
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Documents")
            .join("papr2")
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let cache = fs::read_to_string(root.join(CACHE_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let mut library = Self {
            root,
            papers: Vec::new(),
            collections: Vec::new(),
            cache,
        };
        library.refresh()?;
        Ok(library)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn library_dir(&self) -> PathBuf {
        self.root.join(LIBRARY_DIR)
    }

    fn collections_dir(&self) -> PathBuf {
        self.root.join(COLLECTIONS_DIR)
    }

    fn collection_dir(&self, id: &str) -> PathBuf {
        self.collections_dir().join(id)
    }

    // ----- reading --------------------------------------------------------

    pub fn papers(&self) -> &[Paper] {
        &self.papers
    }

    pub fn paper(&self, id: &str) -> Option<&Paper> {
        self.papers.iter().find(|p| p.id == id)
    }

    pub fn paper_path(&self, id: &str) -> PathBuf {
        self.library_dir().join(id)
    }

    /// Path relative to the library root, for display.
    pub fn relative_path(&self, id: &str) -> String {
        format!("{LIBRARY_DIR}/{id}")
    }

    pub fn collections(&self) -> &[Collection] {
        &self.collections
    }

    pub fn collection(&self, id: &str) -> Option<&Collection> {
        self.collections.iter().find(|c| c.id == id)
    }

    pub fn collections_of<'a>(&'a self, paper: &'a str) -> impl Iterator<Item = &'a Collection> {
        self.collections
            .iter()
            .filter(move |c| c.papers.iter().any(|p| p == paper))
    }

    // ----- scanning -------------------------------------------------------

    /// Re-read the folders, reconcile changes made outside the app, and
    /// update the in-memory state.
    pub fn refresh(&mut self) -> Result<ScanReport> {
        fs::create_dir_all(self.library_dir())
            .with_context(|| format!("creating {}", self.library_dir().display()))?;
        fs::create_dir_all(self.collections_dir())
            .with_context(|| format!("creating {}", self.collections_dir().display()))?;

        // Reconciling may itself change the file system (e.g. repairing a
        // link); scan again until things settle. The cache is only replaced
        // at the end so renames are still detectable on later passes.
        for _ in 0..5 {
            if let Some(report) = self.scan_pass()? {
                return Ok(report);
            }
        }
        // Still in flux (e.g. a file manager mid-operation); try next time.
        Ok(ScanReport::default())
    }

    /// Returns `None` if the file system was modified and needs a re-scan.
    fn scan_pass(&mut self) -> Result<Option<ScanReport>> {
        let library = self.read_library()?;
        let renamed_papers = detect_renames(&self.cache.papers, &library);
        let folders = self.read_collection_folders()?;
        let renamed_collections: Vec<(String, String)> = self
            .cache
            .collections
            .iter()
            .filter(|(old, ino)| **ino != 0 && !folders.contains_key(*old))
            .filter_map(|(old, ino)| {
                folders
                    .iter()
                    .find(|(new, new_ino)| *new_ino == ino && !self.cache.collections.contains_key(*new))
                    .map(|(new, _)| (old.clone(), new.clone()))
            })
            .collect();

        let mut collections = Vec::new();
        for name in folders.keys() {
            match self.reconcile_collection(name, &library, &renamed_papers)? {
                Some(papers) => collections.push(Collection {
                    id: name.clone(),
                    papers,
                }),
                None => return Ok(None),
            }
        }

        // Settled: publish the new state.
        let papers: Vec<Paper> = library
            .iter()
            .map(|(id, info)| Paper {
                id: id.clone(),
                title: title_of(id),
                size_bytes: info.size,
                modified: info.mtime,
            })
            .collect();
        let changed = papers != self.papers || collections != self.collections;
        self.papers = papers;
        self.collections = collections;

        // Carry known hashes over to the new cache.
        let mut cache = Cache {
            papers: library,
            collections: folders,
        };
        for (id, info) in cache.papers.iter_mut() {
            let old_id = renamed_papers.get(id.as_str()).map(String::as_str);
            let old = old_id
                .and_then(|old| self.cache.papers.get(old))
                .or_else(|| self.cache.papers.get(id));
            if let Some(old) = old.filter(|old| old.same_file(info) && old.mtime == info.mtime) {
                info.sha256 = old.sha256.clone();
            }
        }
        if cache != self.cache {
            self.cache = cache;
            self.save_cache();
        }

        let mut renamed_papers: Vec<_> = renamed_papers
            .into_iter()
            .map(|(new, old)| (old, new))
            .collect();
        renamed_papers.sort();
        Ok(Some(ScanReport {
            changed,
            renamed_papers,
            renamed_collections,
        }))
    }

    /// Regular PDF files in `Library/`.
    fn read_library(&self) -> Result<BTreeMap<PaperId, FileInfo>> {
        let mut out = BTreeMap::new();
        for entry in read_dir(&self.library_dir())? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !is_pdf_name(&name) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_file() {
                out.insert(name, FileInfo::from_metadata(&meta));
            }
        }
        Ok(out)
    }

    /// Folders in `Collections/` with their inode numbers.
    fn read_collection_folders(&self) -> Result<BTreeMap<CollectionId, u64>> {
        let mut out = BTreeMap::new();
        for entry in read_dir(&self.collections_dir())? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if let Ok(meta) = fs::symlink_metadata(entry.path()) {
                if meta.is_dir() {
                    out.insert(name, inode(&meta));
                }
            }
        }
        Ok(out)
    }

    /// Reads one collection folder, fixing anything that changed outside
    /// the app. Returns `None` if the file system was modified.
    fn reconcile_collection(
        &mut self,
        collection: &str,
        library: &BTreeMap<PaperId, FileInfo>,
        // new name → old name
        renamed: &BTreeMap<PaperId, PaperId>,
    ) -> Result<Option<Vec<PaperId>>> {
        let dir = self.collection_dir(collection);
        let library_dir = fs::canonicalize(self.library_dir())?;
        let old_to_new: BTreeMap<&str, &str> = renamed
            .iter()
            .map(|(new, old)| (old.as_str(), new.as_str()))
            .collect();
        let mut members = BTreeSet::new();

        for entry in read_dir(&dir)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };

            if meta.is_symlink() {
                let Ok(target) = fs::read_link(&path) else {
                    continue;
                };
                let resolved = dir.join(&target);
                let target_name = target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let into_library = resolved
                    .parent()
                    .and_then(|p| fs::canonicalize(p).ok())
                    .is_some_and(|p| p == library_dir);

                if into_library && library.contains_key(&target_name) {
                    if name == target_name {
                        members.insert(target_name);
                        continue;
                    }
                    // The link was renamed: rename the paper to match.
                    let new_name = with_pdf_extension(&name);
                    if !library.contains_key(&new_name) && valid_file_name(&new_name) {
                        rename(&library_dir.join(&target_name), &library_dir.join(&new_name))?;
                        self.retarget_links(&target_name, &new_name)?;
                    } else {
                        // Can't take that name; restore the link's name.
                        rename(&path, &dir.join(&target_name))?;
                    }
                    return Ok(None);
                }

                if into_library {
                    // Target is gone: either renamed or deleted.
                    if let Some(new_name) = old_to_new.get(target_name.as_str()) {
                        fs::remove_file(&path)?;
                        self.link(collection, new_name)?;
                    } else {
                        fs::remove_file(&path)?;
                    }
                    return Ok(None);
                }

                // A link to a PDF elsewhere on disk: copy it into the library.
                if resolved.is_file() && is_pdf_name(&target_name) {
                    let id = self.import_file(&resolved, Transfer::Copy)?.id();
                    fs::remove_file(&path)?;
                    self.link(collection, &id)?;
                    return Ok(None);
                }
                // Anything else isn't ours; leave it alone.
                continue;
            }

            if meta.is_file() && is_pdf_name(&name) {
                // A real PDF dropped into the collection: move it into the
                // library and replace it with a link.
                let id = self.import_file(&path, Transfer::Move)?.id();
                self.link(collection, &id)?;
                return Ok(None);
            }
        }
        Ok(Some(members.into_iter().collect()))
    }

    fn save_cache(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.cache) {
            let tmp = self.root.join(".papr2-cache.json.tmp");
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, self.root.join(CACHE_FILE));
            }
        }
    }

    // ----- papers ---------------------------------------------------------

    /// Copy a PDF into the library. The file name becomes the paper's name.
    pub fn import_pdf(&mut self, source: &Path) -> Result<ImportOutcome> {
        let outcome = self.import_file(source, Transfer::Copy)?;
        self.refresh()?;
        Ok(outcome)
    }

    fn import_file(&mut self, source: &Path, transfer: Transfer) -> Result<ImportOutcome> {
        if !source.is_file() || !is_pdf_name(&source.to_string_lossy()) {
            bail!("{} is not a PDF", source.display());
        }
        if let Some(existing) = self.find_duplicate(source)? {
            if transfer == Transfer::Move {
                fs::remove_file(source)?;
            }
            return Ok(ImportOutcome::Duplicate(existing));
        }

        let file_name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled.pdf".into());
        let name = self.unique_name(&sanitize_file_name(&file_name));
        let dest = self.library_dir().join(&name);
        match transfer {
            Transfer::Move if fs::rename(source, &dest).is_ok() => {}
            _ => {
                // Copy under a hidden name first so a scan never sees a partial file.
                let partial = self.library_dir().join(format!(".{name}.part"));
                fs::copy(source, &partial)
                    .with_context(|| format!("copying {} into the library", source.display()))?;
                fs::rename(&partial, &dest)?;
                if transfer == Transfer::Move {
                    fs::remove_file(source)?;
                }
            }
        }
        Ok(ImportOutcome::Added(name))
    }

    /// A library file with the same contents as `source`, if any.
    fn find_duplicate(&mut self, source: &Path) -> Result<Option<PaperId>> {
        let size = fs::metadata(source)?.len();
        let library = self.read_library()?;
        let candidates: Vec<_> = library.iter().filter(|(_, i)| i.size == size).collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        let source_hash = hash_file(source)?;
        for (id, info) in candidates {
            let cached = self
                .cache
                .papers
                .get(id)
                .filter(|c| c.same_file(info) && c.mtime == info.mtime)
                .and_then(|c| c.sha256.clone());
            let hash = match cached {
                Some(hash) => hash,
                None => {
                    let hash = hash_file(&self.library_dir().join(id))?;
                    let mut entry = info.clone();
                    entry.sha256 = Some(hash.clone());
                    self.cache.papers.insert(id.clone(), entry);
                    hash
                }
            };
            if hash == source_hash {
                return Ok(Some(id.clone()));
            }
        }
        Ok(None)
    }

    /// Renames the PDF (and its links in every collection). Returns the new id.
    pub fn rename_paper(&mut self, id: &str, title: &str) -> Result<PaperId> {
        let new_id = with_pdf_extension(title.trim());
        if !valid_file_name(&new_id) {
            bail!("“{}” can't be used as a file name", title.trim());
        }
        if new_id == id {
            return Ok(new_id);
        }
        let library_dir = self.library_dir();
        if library_dir.join(&new_id).exists() && !new_id.eq_ignore_ascii_case(id) {
            bail!("A paper named “{}” already exists", title_of(&new_id));
        }
        rename(&library_dir.join(id), &library_dir.join(&new_id))?;
        self.retarget_links(id, &new_id)?;
        self.refresh()?;
        Ok(new_id)
    }

    /// Removes the PDF from disk and from every collection.
    pub fn delete_paper(&mut self, id: &str) -> Result<()> {
        for collection in self.collections_containing(id) {
            self.unlink(&collection, id)?;
        }
        let path = self.paper_path(id);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
        }
        self.refresh()?;
        Ok(())
    }

    fn collections_containing(&self, id: &str) -> Vec<CollectionId> {
        self.collections_of(id).map(|c| c.id.clone()).collect()
    }

    // ----- collections ----------------------------------------------------

    pub fn create_collection(&mut self, name: &str) -> Result<CollectionId> {
        let name = name.trim();
        if !valid_file_name(name) {
            bail!("“{name}” can't be used as a folder name");
        }
        let dir = self.collection_dir(name);
        if dir.exists() {
            bail!("A collection named “{name}” already exists");
        }
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        self.refresh()?;
        Ok(name.to_string())
    }

    pub fn rename_collection(&mut self, id: &str, name: &str) -> Result<CollectionId> {
        let name = name.trim();
        if !valid_file_name(name) {
            bail!("“{name}” can't be used as a folder name");
        }
        if name == id {
            return Ok(name.to_string());
        }
        if self.collection_dir(name).exists() && !name.eq_ignore_ascii_case(id) {
            bail!("A collection named “{name}” already exists");
        }
        rename(&self.collection_dir(id), &self.collection_dir(name))?;
        self.refresh()?;
        Ok(name.to_string())
    }

    /// Deletes the collection folder. Papers stay in the library.
    pub fn delete_collection(&mut self, id: &str) -> Result<()> {
        let dir = self.collection_dir(id);
        let papers = self.collection(id).map(|c| c.papers.clone()).unwrap_or_default();
        for paper in &papers {
            self.unlink(id, paper)?;
        }
        let result = fs::remove_dir(&dir);
        self.refresh()?;
        if result.is_err() && dir.exists() {
            bail!(
                "The folder {} contains other files, so it was kept",
                dir.display()
            );
        }
        Ok(())
    }

    /// Returns `true` if the paper was newly added.
    pub fn add_to_collection(&mut self, collection: &str, paper: &str) -> Result<bool> {
        if self.paper(paper).is_none() || self.collection(collection).is_none() {
            return Ok(false);
        }
        if self.collections_of(paper).any(|c| c.id == collection) {
            return Ok(false);
        }
        self.link(collection, paper)?;
        self.refresh()?;
        Ok(true)
    }

    pub fn remove_from_collection(&mut self, collection: &str, paper: &str) -> Result<()> {
        self.unlink(collection, paper)?;
        self.refresh()?;
        Ok(())
    }

    /// Creates `Collections/<collection>/<paper>` → `../../Library/<paper>`.
    fn link(&self, collection: &str, paper: &str) -> Result<()> {
        let link = self.collection_dir(collection).join(paper);
        if fs::symlink_metadata(&link).is_ok() {
            return Ok(());
        }
        let target = Path::new("..").join("..").join(LIBRARY_DIR).join(paper);
        symlink(&target, &link).with_context(|| format!("creating link {}", link.display()))
    }

    /// Points every collection link to `old` at `new`, renaming the links too.
    fn retarget_links(&self, old: &str, new: &str) -> Result<()> {
        for (collection, _) in self.read_collection_folders()? {
            let dir = self.collection_dir(&collection);
            let mut found = false;
            for entry in read_dir(&dir)? {
                let target = fs::read_link(entry.path()).ok();
                if target.is_some_and(|t| t.file_name().is_some_and(|n| n == old)) {
                    fs::remove_file(entry.path())?;
                    found = true;
                }
            }
            if found {
                self.link(&collection, new)?;
            }
        }
        Ok(())
    }

    /// Removes every link in the collection that points at `paper`.
    fn unlink(&self, collection: &str, paper: &str) -> Result<()> {
        let dir = self.collection_dir(collection);
        for entry in read_dir(&dir)? {
            let path = entry.path();
            let points_at_paper = fs::read_link(&path)
                .ok()
                .and_then(|t| t.file_name().map(|n| n == paper))
                .unwrap_or(false);
            if points_at_paper {
                fs::remove_file(&path)
                    .with_context(|| format!("removing link {}", path.display()))?;
            }
        }
        Ok(())
    }

    fn unique_name(&self, file_name: &str) -> String {
        let stem = title_of(file_name);
        let mut candidate = file_name.to_string();
        let mut n = 2;
        while fs::symlink_metadata(self.library_dir().join(&candidate)).is_ok() {
            candidate = format!("{stem} ({n}).pdf");
            n += 1;
        }
        candidate
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Copy,
    Move,
}

impl ImportOutcome {
    pub fn id(self) -> PaperId {
        match self {
            ImportOutcome::Added(id) | ImportOutcome::Duplicate(id) => id,
        }
    }
}

/// Files that disappeared from the cache and reappeared under a new name.
/// Returns new name → old name.
fn detect_renames(
    cache: &BTreeMap<PaperId, FileInfo>,
    now: &BTreeMap<PaperId, FileInfo>,
) -> BTreeMap<PaperId, PaperId> {
    let mut out = BTreeMap::new();
    for (old, old_info) in cache.iter().filter(|(old, _)| !now.contains_key(*old)) {
        let found = now.iter().find(|(new, info)| {
            !cache.contains_key(*new) && !out.contains_key(*new) && old_info.same_file(info)
        });
        if let Some((new, _)) = found {
            out.insert(new.clone(), old.clone());
        }
    }
    out
}

fn read_dir(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)
        .with_context(|| format!("renaming {} to {}", from.display(), to.display()))
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn inode(meta: &fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::ino(meta)
}

#[cfg(not(unix))]
fn inode(_: &fs::Metadata) -> u64 {
    0
}

pub fn is_pdf_name(name: &str) -> bool {
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".pdf")
}

pub fn is_pdf(path: &Path) -> bool {
    path.is_file() && is_pdf_name(&path.to_string_lossy())
}

/// The display name of a paper: its file name without `.pdf`.
pub fn title_of(file_name: &str) -> String {
    if is_pdf_name(file_name) {
        file_name[..file_name.len() - 4].to_string()
    } else {
        file_name.to_string()
    }
}

fn with_pdf_extension(name: &str) -> String {
    if is_pdf_name(name) {
        name.to_string()
    } else {
        format!("{name}.pdf")
    }
}

/// Whether `name` is usable as a single file or folder name.
fn valid_file_name(name: &str) -> bool {
    let stem = title_of(name);
    !stem.trim().is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name.len() <= 255
}

/// Makes an arbitrary name safe to use as a PDF file name.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | '\0') { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim();
    let cleaned = if title_of(cleaned).trim().is_empty() {
        "Untitled".to_string()
    } else {
        cleaned.to_string()
    };
    with_pdf_extension(&cleaned)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// One-time migration from papr2's original storage format
/// (`<data dir>/papr2/library.json` + `papers/<id>.pdf`).
///
/// Files are copied, not moved; the old `library.json` is renamed to
/// `library.json.migrated` so this only runs once.
pub fn migrate_legacy(root: &Path) -> Result<Option<usize>> {
    let Some(legacy) = dirs::data_dir().map(|d| d.join("papr2")) else {
        return Ok(None);
    };
    let index = legacy.join("library.json");
    if !index.exists() {
        return Ok(None);
    }

    #[derive(Deserialize)]
    struct OldPaper {
        id: u64,
        title: String,
        file: String,
    }
    #[derive(Deserialize)]
    struct OldCollection {
        name: String,
        papers: Vec<u64>,
    }
    #[derive(Deserialize)]
    struct OldLibrary {
        papers: Vec<OldPaper>,
        collections: Vec<OldCollection>,
    }

    let old: OldLibrary = serde_json::from_str(&fs::read_to_string(&index)?)
        .with_context(|| format!("parsing {}", index.display()))?;
    let mut library = Library::open(root)?;
    let mut ids = BTreeMap::new();
    for paper in &old.papers {
        let source = legacy.join("papers").join(&paper.file);
        if !source.is_file() {
            continue;
        }
        if let Some(existing) = library.find_duplicate(&source)? {
            ids.insert(paper.id, existing);
            continue;
        }
        let name = library.unique_name(&sanitize_file_name(&paper.title));
        let partial = library.library_dir().join(format!(".{name}.part"));
        fs::copy(&source, &partial)?;
        fs::rename(&partial, library.library_dir().join(&name))?;
        ids.insert(paper.id, name);
    }
    for collection in &old.collections {
        let name = sanitize_file_name(&collection.name);
        let name = title_of(&name);
        let dir = library.collection_dir(&name);
        fs::create_dir_all(&dir)?;
        for id in &collection.papers {
            if let Some(paper) = ids.get(id) {
                library.link(&name, paper)?;
            }
        }
    }
    library.refresh()?;
    fs::rename(&index, legacy.join("library.json.migrated"))?;
    Ok(Some(ids.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "papr2-test-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_pdf(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, format!("%PDF-1.4 {body}")).unwrap();
        path
    }

    fn ids(lib: &Library) -> Vec<String> {
        lib.papers().iter().map(|p| p.id.clone()).collect()
    }

    #[test]
    fn collections_link_to_the_library() {
        let tmp = TempDir::new("links");
        let pdf = write_pdf(&tmp.0.join("src"), "Attention.pdf", "a");
        let mut lib = Library::open(tmp.0.join("papr2")).unwrap();

        let id = lib.import_pdf(&pdf).unwrap().id();
        assert_eq!(id, "Attention.pdf");
        assert_eq!(lib.paper(&id).unwrap().title, "Attention");

        lib.create_collection("Project A").unwrap();
        lib.create_collection("Project B").unwrap();
        assert!(lib.add_to_collection("Project A", &id).unwrap());
        assert!(lib.add_to_collection("Project B", &id).unwrap());
        assert!(!lib.add_to_collection("Project B", &id).unwrap());

        let link = tmp.0.join("papr2/Collections/Project A/Attention.pdf");
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read(&link).unwrap(), b"%PDF-1.4 a");
        assert_eq!(fs::read_dir(tmp.0.join("papr2/Library")).unwrap().count(), 1);

        // Same contents under another name is a duplicate.
        let copy = write_pdf(&tmp.0.join("src"), "copy.pdf", "a");
        assert!(matches!(lib.import_pdf(&copy).unwrap(), ImportOutcome::Duplicate(d) if d == id));
        // Different contents with the same name get a unique name.
        let other = write_pdf(&tmp.0.join("src2"), "Attention.pdf", "b");
        assert_eq!(lib.import_pdf(&other).unwrap().id(), "Attention (2).pdf");

        // Renaming in the app renames the file and every link.
        let new_id = lib.rename_paper(&id, "Attention Is All You Need").unwrap();
        assert_eq!(new_id, "Attention Is All You Need.pdf");
        assert!(tmp.0.join("papr2/Library/Attention Is All You Need.pdf").exists());
        assert!(tmp.0.join("papr2/Collections/Project B/Attention Is All You Need.pdf").exists());
        assert_eq!(lib.collections_of(&new_id).count(), 2);

        // Deleting removes the file and its links.
        lib.delete_paper(&new_id).unwrap();
        assert!(lib.collections().iter().all(|c| c.papers.is_empty()));
        assert_eq!(ids(&lib), vec!["Attention (2).pdf"]);
    }

    #[test]
    fn picks_up_changes_made_in_a_file_manager() {
        let tmp = TempDir::new("external");
        let root = tmp.0.join("papr2");
        let mut lib = Library::open(&root).unwrap();
        let id = lib
            .import_pdf(&write_pdf(&tmp.0.join("src"), "bert.pdf", "bert"))
            .unwrap()
            .id();
        lib.create_collection("NLP").unwrap();
        lib.create_collection("Thesis").unwrap();
        lib.add_to_collection("NLP", &id).unwrap();
        lib.add_to_collection("Thesis", &id).unwrap();

        // Rename the PDF in Library/: links follow.
        fs::rename(root.join("Library/bert.pdf"), root.join("Library/BERT.pdf")).unwrap();
        let report = lib.refresh().unwrap();
        assert!(report.changed);
        assert_eq!(report.renamed_papers, vec![("bert.pdf".into(), "BERT.pdf".into())]);
        assert_eq!(lib.collection("NLP").unwrap().papers, vec!["BERT.pdf"]);
        assert_eq!(lib.collection("Thesis").unwrap().papers, vec!["BERT.pdf"]);
        assert!(root.join("Collections/Thesis/BERT.pdf").exists());

        // Rename a link inside a collection: the paper is renamed everywhere.
        fs::rename(
            root.join("Collections/NLP/BERT.pdf"),
            root.join("Collections/NLP/BERT paper.pdf"),
        )
        .unwrap();
        lib.refresh().unwrap();
        assert_eq!(ids(&lib), vec!["BERT paper.pdf"]);
        assert_eq!(lib.collection("Thesis").unwrap().papers, vec!["BERT paper.pdf"]);

        // Rename a collection folder.
        fs::rename(root.join("Collections/NLP"), root.join("Collections/Language")).unwrap();
        let report = lib.refresh().unwrap();
        assert_eq!(
            report.renamed_collections,
            vec![("NLP".to_string(), "Language".to_string())]
        );
        assert_eq!(lib.collection("Language").unwrap().papers, vec!["BERT paper.pdf"]);

        // Delete a link: the paper leaves that collection only.
        fs::remove_file(root.join("Collections/Language/BERT paper.pdf")).unwrap();
        lib.refresh().unwrap();
        assert!(lib.collection("Language").unwrap().papers.is_empty());
        assert_eq!(lib.collection("Thesis").unwrap().papers.len(), 1);

        // Drop a real PDF into a collection: it moves into the library.
        write_pdf(&root.join("Collections/Thesis"), "resnet.pdf", "resnet");
        lib.refresh().unwrap();
        assert_eq!(ids(&lib), vec!["BERT paper.pdf", "resnet.pdf"]);
        assert!(fs::symlink_metadata(root.join("Collections/Thesis/resnet.pdf"))
            .unwrap()
            .is_symlink());

        // Drop a PDF straight into Library/.
        write_pdf(&root.join("Library"), "vit.pdf", "vit");
        lib.refresh().unwrap();
        assert_eq!(lib.papers().len(), 3);

        // Delete a PDF from Library/: dangling links are cleaned up.
        fs::remove_file(root.join("Library/resnet.pdf")).unwrap();
        lib.refresh().unwrap();
        assert_eq!(lib.collection("Thesis").unwrap().papers, vec!["BERT paper.pdf"]);
        assert!(fs::symlink_metadata(root.join("Collections/Thesis/resnet.pdf")).is_err());

        // State survives reopening.
        drop(lib);
        let lib = Library::open(&root).unwrap();
        assert_eq!(lib.papers().len(), 2);
        assert_eq!(lib.collections().len(), 2);
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_name("a/b"), "a-b.pdf");
        assert_eq!(sanitize_file_name(".hidden.pdf"), "hidden.pdf");
        assert_eq!(sanitize_file_name("  "), "Untitled.pdf");
        assert!(!valid_file_name(".pdf"));
        assert!(!valid_file_name("a/b.pdf"));
        assert!(valid_file_name("Thesis"));
    }
}
