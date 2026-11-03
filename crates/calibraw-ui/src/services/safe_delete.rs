//! Reversible, folder-local photo removal. Pending marks are workflow metadata,
//! never photo reviews or edits. File operations require an explicit caller action.

use anyhow::{bail, ensure, Context, Result};
use calibraw_core::file_ops::{sync_parent_directory, write_atomically};
use calibraw_core::pipeline::{is_camera_raw_path, is_supported_image_path};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const DELETED_DIRECTORY: &str = "HANNA_Deleted";
const METADATA_FILE: &str = ".hanna-workflow.json";
const LOCK_FILE: &str = ".hanna-workflow.lock";
const MANIFEST_FILE: &str = "manifest.json";
const VERSION: u32 = 1;
const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
const HISTORY_LIMIT: usize = 64;
static NEXT_BATCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MarkChange {
    added: BTreeSet<PathBuf>,
    removed: BTreeSet<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MarkDocument {
    version: u32,
    pending: BTreeSet<PathBuf>,
    undo: Vec<MarkChange>,
    redo: Vec<MarkChange>,
}

impl Default for MarkDocument {
    fn default() -> Self {
        Self {
            version: VERSION,
            pending: BTreeSet::new(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

/// Cached read model. Mutations lock the folder and reload the latest document
/// before writing, so two app windows cannot silently lose each other's marks.
#[derive(Clone, Debug)]
pub(crate) struct PendingDeleteStore {
    root: PathBuf,
    // Retain the Library's spelling too (Windows 8.3 aliases and junctions).
    // Both prefixes are resolved once at load, never per thumbnail/frame.
    input_root: PathBuf,
    document: MarkDocument,
}

impl PendingDeleteStore {
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let input_root = ordinary_path(std::path::absolute(root)?);
        let root = canonical_root(root)?;
        let _lock = FolderLock::acquire(&root)?;
        let document = load_marks(&root)?;
        Ok(Self {
            root,
            input_root,
            document,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn is_pending(&self, path: &Path) -> bool {
        self.relative_path(path)
            .is_ok_and(|relative| self.document.pending.contains(&relative))
    }

    pub(crate) fn pending_paths(&self) -> Vec<PathBuf> {
        self.document
            .pending
            .iter()
            .map(|path| self.input_root.join(path))
            .collect()
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.document.undo.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        !self.document.redo.is_empty()
    }

    fn relative_path(&self, path: &Path) -> Result<PathBuf> {
        relative_path(&self.root, path).or_else(|_| relative_path(&self.input_root, path))
    }

    /// Marks only the selected images. Selecting a RAW for the later move also
    /// moves its same-stem in-camera JPEG/HEIC copies, as stated in the dialog.
    pub(crate) fn set_pending(&mut self, paths: &[PathBuf], pending: bool) -> Result<usize> {
        let _lock = FolderLock::acquire(&self.root)?;
        let mut next = load_marks(&self.root)?;
        let mut change = MarkChange::default();
        for path in paths {
            let relative = self.relative_path(path)?;
            ensure!(
                is_supported_image_path(&relative),
                "Not a supported photo: {}",
                path.display()
            );
            if pending {
                checked_file(&self.root, &relative, false)?;
                if next.pending.insert(relative.clone()) {
                    change.added.insert(relative);
                }
            } else if next.pending.remove(&relative) {
                change.removed.insert(relative);
            }
        }
        let changed = change.added.len() + change.removed.len();
        if changed != 0 {
            next.undo.push(change);
            if next.undo.len() > HISTORY_LIMIT {
                next.undo.remove(0);
            }
            next.redo.clear();
            save_json(&self.root.join(METADATA_FILE), &next)?;
        }
        self.document = next;
        Ok(changed)
    }

    pub(crate) fn undo(&mut self) -> Result<bool> {
        self.change_history(false)
    }

    pub(crate) fn redo(&mut self) -> Result<bool> {
        self.change_history(true)
    }

    fn change_history(&mut self, redo: bool) -> Result<bool> {
        let _lock = FolderLock::acquire(&self.root)?;
        let mut next = load_marks(&self.root)?;
        let history = if redo { &mut next.redo } else { &mut next.undo };
        let Some(change) = history.pop() else {
            self.document = next;
            return Ok(false);
        };
        let (insert, remove) = if redo {
            (&change.added, &change.removed)
        } else {
            (&change.removed, &change.added)
        };
        for relative in insert {
            // Never resurrect a mark for an image that another file action moved.
            checked_file(&self.root, relative, false)?;
            next.pending.insert(relative.clone());
        }
        for relative in remove {
            next.pending.remove(relative);
        }
        if redo {
            next.undo.push(change);
        } else {
            next.redo.push(change);
        }
        save_json(&self.root.join(METADATA_FILE), &next)?;
        self.document = next;
        Ok(true)
    }
}

/// A partial operation is a successful report with failures: its journal and all
/// unprocessed files remain available to Restore. An invalid journal is an error.
#[derive(Debug, Default)]
pub(crate) struct OperationReport {
    pub(crate) files: usize,
    pub(crate) failures: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum EntryState {
    Planned,
    Moving,
    Stored,
    Restoring,
    Restored,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    original: PathBuf,
    stored: PathBuf,
    state: EntryState,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}

/// Moves the current durable marks. The caller must show its own confirmation,
/// finish pending sidecar writes and release photo handles before calling this
/// on a worker. No permanent deletion or automatic cleanup is performed.
pub(crate) fn move_marked(root: &Path) -> Result<OperationReport> {
    let root = canonical_root(root)?;
    let _lock = FolderLock::acquire(&root)?;
    let mut marks = load_marks(&root)?;
    if marks.pending.is_empty() {
        return Ok(OperationReport::default());
    }
    // Validate the complete bundle before moving anything.
    let mut originals = BTreeSet::new();
    for relative in &marks.pending {
        originals.extend(photo_bundle(&root, relative)?);
    }
    let quarantine = quarantine_directory(&root, true)?;
    let batch = create_batch(&quarantine)?;
    let path = batch.join(MANIFEST_FILE);
    let mut manifest = Manifest {
        version: VERSION,
        entries: originals
            .into_iter()
            .map(|original| ManifestEntry {
                stored: Path::new("files").join(&original),
                original,
                state: EntryState::Planned,
            })
            .collect(),
    };
    save_json(&path, &manifest)?;
    let mut report = OperationReport::default();
    for index in 0..manifest.entries.len() {
        let original = manifest.entries[index].original.clone();
        let stored = manifest.entries[index].stored.clone();
        let source = checked_file(&root, &original, false)?;
        let destination = safe_destination(&batch, &stored)?;
        manifest.entries[index].state = EntryState::Moving;
        save_json(&path, &manifest)?;
        if let Err(error) = move_without_replacing(&source, &destination) {
            report
                .failures
                .push(format!("{}: {error}", source.display()));
            break;
        }
        report.files += 1;
        manifest.entries[index].state = EntryState::Stored;
        // A failure here leaves the already-durable Moving transition, which
        // Restore reconciles from source/destination existence.
        save_json(&path, &manifest)?;
    }
    if report.failures.is_empty() {
        marks.pending.clear();
        marks.undo.clear();
        marks.redo.clear();
        save_json(&root.join(METADATA_FILE), &marks)?;
    }
    Ok(report)
}

/// Restores journalled files to their exact original names. A name collision is
/// reported and left untouched at both locations; retry after resolving it.
/// Planned/Moving/Restoring entries are reconciled too, allowing interrupted
/// moves and restores to be retried after restarting the app.
pub(crate) fn restore_all(root: &Path) -> Result<OperationReport> {
    let root = canonical_root(root)?;
    let _lock = FolderLock::acquire(&root)?;
    let manifests = manifests(&root)?;
    let mut marks = load_marks(&root)?;
    let mut report = OperationReport::default();
    let mut restored_photos = BTreeSet::new();
    for (batch, mut manifest) in manifests {
        let manifest_path = batch.join(MANIFEST_FILE);
        for index in 0..manifest.entries.len() {
            // A completed old transaction must not unmark a newly marked photo
            // or interfere with a later transaction of the same filename.
            if matches!(manifest.entries[index].state, EntryState::Restored) {
                continue;
            }
            let original = manifest.entries[index].original.clone();
            let stored = manifest.entries[index].stored.clone();
            let source = checked_file(&batch, &stored, true)?;
            let destination = checked_file(&root, &original, true)?;
            match (source.exists(), destination.exists()) {
                (true, false) => {
                    let destination = safe_destination(&root, &original)?;
                    manifest.entries[index].state = EntryState::Restoring;
                    save_json(&manifest_path, &manifest)?;
                    if let Err(error) = move_without_replacing(&source, &destination) {
                        report
                            .failures
                            .push(format!("{}: {error}", destination.display()));
                        continue;
                    }
                    report.files += 1;
                }
                (true, true) => {
                    report.failures.push(format!(
                        "Restore conflict: {} already exists; the quarantined file was retained.",
                        destination.display()
                    ));
                    continue;
                }
                (false, false) => {
                    report.failures.push(format!(
                        "Missing at both original and quarantine paths: {}",
                        original.display()
                    ));
                    continue;
                }
                (false, true) => {}
            }
            manifest.entries[index].state = EntryState::Restored;
            if is_supported_image_path(&original) {
                restored_photos.insert(original);
            }
            save_json(&manifest_path, &manifest)?;
        }
    }
    if restored_photos
        .iter()
        .any(|path| marks.pending.contains(path))
    {
        marks.pending.retain(|path| !restored_photos.contains(path));
        marks.undo.clear();
        marks.redo.clear();
        save_json(&root.join(METADATA_FILE), &marks)?;
    }
    Ok(report)
}

pub(crate) fn quarantined_files(root: &Path) -> Result<usize> {
    let root = canonical_root(root)?;
    let _lock = FolderLock::acquire(&root)?;
    let mut count = 0;
    for (batch, manifest) in manifests(&root)? {
        for entry in manifest.entries {
            count += usize::from(checked_file(&batch, &entry.stored, true)?.exists());
        }
    }
    Ok(count)
}

fn load_marks(root: &Path) -> Result<MarkDocument> {
    let path = root.join(METADATA_FILE);
    let document = if path.try_exists()? {
        read_json(&path)?
    } else {
        MarkDocument::default()
    };
    ensure!(
        document.version == VERSION,
        "Unsupported HANNA workflow metadata version {}",
        document.version
    );
    ensure!(
        document.undo.len() <= HISTORY_LIMIT && document.redo.len() <= HISTORY_LIMIT,
        "Too much workflow history"
    );
    for relative in document.pending.iter().chain(
        document
            .undo
            .iter()
            .chain(&document.redo)
            .flat_map(|change| change.added.iter().chain(&change.removed)),
    ) {
        validate_relative(relative)?;
        ensure!(
            is_supported_image_path(relative),
            "Invalid pending photo: {}",
            relative.display()
        );
    }
    Ok(document)
}

fn manifests(root: &Path) -> Result<Vec<(PathBuf, Manifest)>> {
    let quarantine = quarantine_directory(root, false)?;
    if !quarantine.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(quarantine)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Refusing a symbolic link in HANNA_Deleted"
        );
        if !metadata.is_dir() {
            continue;
        }
        let batch = entry.path();
        let path = batch.join(MANIFEST_FILE);
        if !path.try_exists()? {
            continue;
        }
        let manifest: Manifest = read_json(&path)?;
        ensure!(
            manifest.version == VERSION,
            "Unsupported HANNA deletion manifest version {}",
            manifest.version
        );
        let mut originals = BTreeSet::new();
        for entry in &manifest.entries {
            validate_relative(&entry.original)?;
            ensure!(
                entry.stored == Path::new("files").join(&entry.original),
                "Invalid quarantine path in manifest"
            );
            ensure!(
                originals.insert(&entry.original),
                "Duplicate original path in manifest"
            );
            checked_file(root, &entry.original, true)?;
            checked_file(&batch, &entry.stored, true)?;
        }
        result.push((batch, manifest));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

fn photo_bundle(root: &Path, selected: &Path) -> Result<BTreeSet<PathBuf>> {
    let selected_path = checked_file(root, selected, false)?;
    ensure!(
        is_supported_image_path(&selected_path),
        "Not a supported photo"
    );
    let parent = selected_path
        .parent()
        .context("Photo has no parent folder")?;
    let mut photos = BTreeSet::from([selected_path.clone()]);
    let siblings = fs::read_dir(parent)?.collect::<std::io::Result<Vec<_>>>()?;
    if is_camera_raw_path(&selected_path) {
        let stem = lower_stem(&selected_path);
        for sibling in &siblings {
            let path = sibling.path();
            if lower_stem(&path) == stem
                && path.extension().is_some_and(|extension| {
                    matches!(
                        extension.to_string_lossy().to_ascii_lowercase().as_str(),
                        "jpg" | "jpeg" | "heic" | "heif"
                    )
                })
            {
                photos.insert(path);
            }
        }
    }
    let mut bundle = BTreeSet::new();
    for photo in &photos {
        bundle.insert(relative_path(root, photo)?);
        for companion in calibraw_core::sidecar::raw_companion_paths(photo) {
            if companion.try_exists()? {
                bundle.insert(relative_path(root, &companion)?);
            }
        }
        for suffix in [".calibraw-thumb.jpg", ".calibraw-thumb.fingerprint"] {
            let mut companion = photo.as_os_str().to_os_string();
            companion.push(suffix);
            let companion = PathBuf::from(companion);
            if companion.try_exists()? {
                bundle.insert(relative_path(root, &companion)?);
            }
        }
        let photo_name = photo
            .file_name()
            .context("Photo has no filename")?
            .to_string_lossy()
            .to_lowercase();
        for sibling in &siblings {
            let path = sibling.path();
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("xmp"))
                && (lower_stem(&path) == lower_stem(photo) || lower_stem(&path) == photo_name)
            {
                bundle.insert(relative_path(root, &path)?);
            }
        }
    }
    for relative in &bundle {
        checked_file(root, relative, false)?;
    }
    Ok(bundle)
}

fn lower_stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(root)
        .with_context(|| format!("Cannot open photo folder {}", root.display()))?;
    // Windows canonicalize adds a verbatim prefix. LibraryAsset paths keep the
    // ordinary drive/UNC spelling; normalize once rather than canonicalizing
    // every thumbnail lookup on the UI thread.
    let root = ordinary_path(canonical);
    ensure!(root.is_dir(), "The photo folder is not a directory");
    ensure!(
        !root
            .components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case(DELETED_DIRECTORY)),
        "HANNA_Deleted cannot be used as a photo library"
    );
    Ok(root)
}

fn ordinary_path(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(path) if path.starts_with(r"\\?\UNC\") => PathBuf::from(format!(r"\\{}", &path[8..])),
        Some(path) if path.starts_with(r"\\?\") => PathBuf::from(&path[4..]),
        _ => path,
    }
}

fn validate_relative(relative: &Path) -> Result<()> {
    ensure!(!relative.as_os_str().is_empty(), "Empty photo path");
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("Unsafe relative photo path: {}", relative.display());
        };
        ensure!(
            !name.eq_ignore_ascii_case(DELETED_DIRECTORY)
                && name != METADATA_FILE
                && name != LOCK_FILE,
            "Reserved workflow path: {}",
            relative.display()
        );
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .context("Photo is outside the working folder")?
    } else {
        path
    };
    validate_relative(relative)?;
    Ok(relative.to_path_buf())
}

/// Rejects symlinks in every component, not only the final entry. A missing
/// suffix is permitted only when inspecting/restoring an original location.
fn checked_file(root: &Path, relative: &Path, allow_missing: bool) -> Result<PathBuf> {
    validate_relative(relative)?;
    let mut path = root.to_path_buf();
    let parts = relative.components().collect::<Vec<_>>();
    for (index, component) in parts.iter().enumerate() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "Refusing symbolic link: {}",
                    path.display()
                );
                if index + 1 == parts.len() {
                    ensure!(metadata.is_file(), "Not a regular file: {}", path.display());
                } else {
                    ensure!(metadata.is_dir(), "Not a directory: {}", path.display());
                }
            }
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot inspect {}", path.display()))
            }
        }
    }
    Ok(path)
}

fn safe_destination(root: &Path, relative: &Path) -> Result<PathBuf> {
    let destination = checked_file(root, relative, true)?;
    ensure!(
        !destination.try_exists()?,
        "Destination already exists: {}",
        destination.display()
    );
    let parent = relative.parent().context("Destination has no parent")?;
    let mut directory = root.to_path_buf();
    for component in parent.components() {
        directory.push(component.as_os_str());
        match fs::create_dir(&directory) {
            Ok(()) => {
                sync_parent_directory(directory.parent().context("Directory has no parent")?)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&directory)?;
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Unsafe destination directory: {}",
                    directory.display()
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(destination)
}

fn quarantine_directory(root: &Path, create: bool) -> Result<PathBuf> {
    let path = root.join(DELETED_DIRECTORY);
    if create && !path.try_exists()? {
        fs::create_dir(&path)?;
        sync_parent_directory(root)?;
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "HANNA_Deleted must be a real directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {}
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn create_batch(quarantine: &Path) -> Result<PathBuf> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for _ in 0..100 {
        let path = quarantine.join(format!(
            "{timestamp}-{}-{}",
            std::process::id(),
            NEXT_BATCH.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                sync_parent_directory(quarantine)?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    bail!("Cannot allocate an unused HANNA_Deleted transaction folder")
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Unsafe metadata file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_DOCUMENT_BYTES,
        "Workflow metadata exceeds its size limit"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
        "Workflow metadata exceeds its size limit"
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid HANNA metadata: {}", path.display()))
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Unsafe metadata file: {}",
            path.display()
        );
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    ensure!(
        bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
        "Workflow metadata exceeds its size limit"
    );
    write_atomically(path, |file| file.write_all(&bytes))
        .with_context(|| format!("Cannot save HANNA metadata: {}", path.display()))
}

struct FolderLock {
    _file: File,
}

impl FolderLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(LOCK_FILE);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Unsafe workflow lock"
            );
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock()
            .context("Another HANNA workflow operation is using this folder")?;
        Ok(Self { _file: file })
    }
}

/// Windows' existing MoveFileExW helper is atomic without replacement. On
/// other systems use only the no-clobber hard-link path: never its rename
/// fallback, which could race with another writer and overwrite a photo.
fn move_without_replacing(source: &Path, destination: &Path) -> Result<()> {
    if cfg!(windows) {
        calibraw_core::file_ops::move_file_without_replacing(source, destination)?;
    } else {
        fs::hard_link(source, destination)
            .context("This filesystem must support hard links for safe moves")?;
        fs::remove_file(source).context("File was stored but its original could not be removed; Restore will report the conflict")?;
    }
    sync_parent_directory(source.parent().context("Source has no parent")?)?;
    sync_parent_directory(destination.parent().context("Destination has no parent")?)?;
    Ok(())
}

#[cfg(test)]
mod tests;
