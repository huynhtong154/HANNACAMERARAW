use super::*;

#[test]
fn pending_marks_survive_restart_and_undo_redo_without_touching_photos_or_sidecars() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let photo = folder.path().join("Wedding.NEF");
    let sidecar = folder.path().join("Wedding.NEF.calibraw");
    fs::write(&photo, b"original RAW")?;
    fs::write(&sidecar, b"unchanged legacy sidecar")?;
    let mut store = PendingDeleteStore::load(folder.path())?;
    assert_eq!(store.set_pending(std::slice::from_ref(&photo), true)?, 1);
    assert_eq!(store.set_pending(std::slice::from_ref(&photo), true)?, 0);
    let mut reopened = PendingDeleteStore::load(folder.path())?;
    assert!(reopened.is_pending(&photo));
    assert!(reopened.can_undo());
    assert!(reopened.undo()?);
    assert!(!reopened.is_pending(&photo));
    assert!(!reopened.can_undo());
    assert!(reopened.can_redo());
    assert!(reopened.redo()?);
    assert!(reopened.is_pending(&photo));
    assert_eq!(
        reopened.set_pending(std::slice::from_ref(&photo), false)?,
        1
    );
    assert!(!PendingDeleteStore::load(folder.path())?.is_pending(&photo));
    assert_eq!(fs::read(&photo)?, b"original RAW");
    assert_eq!(fs::read(&sidecar)?, b"unchanged legacy sidecar");
    assert!(!folder.path().join(DELETED_DIRECTORY).exists());
    Ok(())
}

#[test]
fn marked_raw_bundle_moves_and_restores_exact_names_and_bytes() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let source = folder.path().join("Lễ cưới");
    fs::create_dir(&source)?;
    let files = [
        ("Ảnh 001.NEF", b"RAW".as_slice()),
        ("Ảnh 001.JPG", b"JPEG".as_slice()),
        ("Ảnh 001.HEIC", b"HEIC".as_slice()),
        ("Ảnh 001.NEF.calibraw", b"legacy sidecar".as_slice()),
        ("Ảnh 001.JPG.calibraw", b"jpeg sidecar".as_slice()),
        ("Ảnh 001.NEF.calibraw-denoise", b"denoise".as_slice()),
        ("Ảnh 001.NEF.calibraw-people", b"people".as_slice()),
        ("Ảnh 001.NEF.calibraw-thumb.jpg", b"thumbnail".as_slice()),
        (
            "Ảnh 001.NEF.calibraw-thumb.fingerprint",
            b"fingerprint".as_slice(),
        ),
        ("Ảnh 001.XMP", b"source XMP".as_slice()),
        ("Ảnh 001.NEF.xmp", b"file-named XMP".as_slice()),
    ];
    for (name, bytes) in files {
        fs::write(source.join(name), bytes)?;
    }
    fs::write(source.join("Ảnh 001.png"), b"unrelated export")?;
    fs::write(source.join("Ảnh 002.JPG"), b"different photo")?;
    let photo = source.join("Ảnh 001.NEF");
    let mut marks = PendingDeleteStore::load(folder.path())?;
    marks.set_pending(&[photo, source.join("Ảnh 001.JPG")], true)?;
    let moved = move_marked(folder.path())?;
    assert!(moved.failures.is_empty(), "{:?}", moved.failures);
    assert_eq!(moved.files, files.len());
    assert_eq!(quarantined_files(folder.path())?, files.len());
    for (name, _) in files {
        assert!(!source.join(name).exists());
    }
    assert_eq!(fs::read(source.join("Ảnh 001.png"))?, b"unrelated export");
    assert!(PendingDeleteStore::load(folder.path())?
        .pending_paths()
        .is_empty());
    let restored = restore_all(folder.path())?;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert_eq!(restored.files, files.len());
    for (name, bytes) in files {
        assert_eq!(fs::read(source.join(name))?, bytes);
    }
    assert_eq!(quarantined_files(folder.path())?, 0);
    assert_eq!(restore_all(folder.path())?.files, 0);
    assert!(folder.path().join(DELETED_DIRECTORY).is_dir());
    Ok(())
}

#[test]
fn rendered_photo_does_not_implicitly_move_its_raw() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let jpeg = folder.path().join("pair.JPG");
    fs::write(&jpeg, b"jpeg")?;
    fs::write(folder.path().join("pair.NEF"), b"raw")?;
    let mut marks = PendingDeleteStore::load(folder.path())?;
    marks.set_pending(&[jpeg], true)?;
    assert_eq!(move_marked(folder.path())?.files, 1);
    assert_eq!(fs::read(folder.path().join("pair.NEF"))?, b"raw");
    assert_eq!(restore_all(folder.path())?.files, 1);
    Ok(())
}

#[test]
fn completed_journal_does_not_unmark_a_new_delete_or_interfere_with_a_later_move() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let photo = folder.path().join("photo.NEF");
    fs::write(&photo, b"original")?;
    let mut store = PendingDeleteStore::load(folder.path())?;
    store.set_pending(std::slice::from_ref(&photo), true)?;
    move_marked(folder.path())?;
    restore_all(folder.path())?;
    store.set_pending(std::slice::from_ref(&photo), true)?;
    assert_eq!(restore_all(folder.path())?.files, 0);
    assert!(PendingDeleteStore::load(folder.path())?.is_pending(&photo));
    move_marked(folder.path())?;
    let restored = restore_all(folder.path())?;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert_eq!(restored.files, 1);
    assert_eq!(fs::read(&photo)?, b"original");
    Ok(())
}

#[test]
fn restore_conflicts_keep_both_files_and_can_be_retried() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let photo = folder.path().join("photo.NEF");
    fs::write(&photo, b"original")?;
    PendingDeleteStore::load(folder.path())?.set_pending(std::slice::from_ref(&photo), true)?;
    move_marked(folder.path())?;
    fs::write(&photo, b"new occupant")?;
    let conflict = restore_all(folder.path())?;
    assert_eq!(conflict.files, 0);
    assert_eq!(conflict.failures.len(), 1);
    assert_eq!(fs::read(&photo)?, b"new occupant");
    assert_eq!(quarantined_files(folder.path())?, 1);
    // Only a generated test file is removed to resolve the simulated conflict.
    fs::remove_file(&photo)?;
    let restored = restore_all(folder.path())?;
    assert!(restored.failures.is_empty());
    assert_eq!(restored.files, 1);
    assert_eq!(fs::read(&photo)?, b"original");
    Ok(())
}

#[test]
fn recovery_reconciles_journalled_moving_and_restoring_transitions() -> Result<()> {
    for state in [EntryState::Moving, EntryState::Restoring] {
        let folder = tempfile::tempdir()?;
        let photo = folder.path().join("photo.NEF");
        fs::write(&photo, b"original")?;
        let quarantine = quarantine_directory(folder.path(), true)?;
        let batch = create_batch(&quarantine)?;
        let stored = PathBuf::from("files/photo.NEF");
        let destination = safe_destination(&batch, &stored)?;
        let manifest = Manifest {
            version: VERSION,
            entries: vec![ManifestEntry {
                original: PathBuf::from("photo.NEF"),
                stored,
                state,
            }],
        };
        save_json(&batch.join(MANIFEST_FILE), &manifest)?;
        // Simulate a crash after the file move but before saving Stored/Restored.
        if matches!(state, EntryState::Moving) {
            move_without_replacing(&photo, &destination)?;
        }
        let restored = restore_all(folder.path())?;
        assert!(restored.failures.is_empty());
        assert_eq!(
            restored.files,
            usize::from(matches!(state, EntryState::Moving))
        );
        assert_eq!(fs::read(&photo)?, b"original");
        assert!(!destination.exists());
        let recovered: Manifest = read_json(&batch.join(MANIFEST_FILE))?;
        assert!(matches!(recovered.entries[0].state, EntryState::Restored));
    }
    Ok(())
}

#[test]
fn a_bad_companion_fails_preflight_without_moving_any_photo() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let photo = folder.path().join("photo.NEF");
    fs::write(&photo, b"raw")?;
    fs::create_dir(folder.path().join("photo.NEF.calibraw"))?;
    let mut marks = PendingDeleteStore::load(folder.path())?;
    marks.set_pending(std::slice::from_ref(&photo), true)?;
    assert!(move_marked(folder.path()).is_err());
    assert_eq!(fs::read(&photo)?, b"raw");
    assert!(!folder.path().join(DELETED_DIRECTORY).exists());
    assert!(PendingDeleteStore::load(folder.path())?.is_pending(&photo));
    Ok(())
}

#[test]
fn malformed_versions_and_traversal_fail_closed() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let photo = folder.path().join("photo.NEF");
    fs::write(&photo, b"raw")?;
    fs::write(
        folder.path().join(METADATA_FILE),
        br#"{"version":2,"pending":[],"undo":[],"redo":[]}"#,
    )?;
    assert!(PendingDeleteStore::load(folder.path()).is_err());
    fs::write(folder.path().join(METADATA_FILE), b"not JSON")?;
    assert!(PendingDeleteStore::load(folder.path()).is_err());
    fs::remove_file(folder.path().join(METADATA_FILE))?;
    let mut marks = PendingDeleteStore::load(folder.path())?;
    assert!(marks
        .set_pending(&[PathBuf::from("../outside.NEF")], true)
        .is_err());
    assert!(marks
        .set_pending(&[PathBuf::from("HANNA_Deleted/photo.NEF")], true)
        .is_err());
    let batch = create_batch(&quarantine_directory(folder.path(), true)?)?;
    for original in ["../outside.NEF", "HANNA_Deleted/photo.NEF"] {
        let manifest = Manifest {
            version: VERSION,
            entries: vec![ManifestEntry {
                original: PathBuf::from(original),
                stored: Path::new("files").join(original),
                state: EntryState::Stored,
            }],
        };
        save_json(&batch.join(MANIFEST_FILE), &manifest)?;
        assert!(restore_all(folder.path()).is_err());
        assert_eq!(fs::read(&photo)?, b"raw");
    }
    Ok(())
}

#[test]
fn simultaneous_operations_refuse_the_folder_and_failed_mark_does_not_change_memory() -> Result<()>
{
    let folder = tempfile::tempdir()?;
    let photo = folder.path().join("photo.NEF");
    fs::write(&photo, b"raw")?;
    let mut marks = PendingDeleteStore::load(folder.path())?;
    let lock = FolderLock::acquire(folder.path())?;
    assert!(marks
        .set_pending(std::slice::from_ref(&photo), true)
        .is_err());
    assert!(!marks.is_pending(&photo));
    assert!(PendingDeleteStore::load(folder.path()).is_err());
    drop(lock);
    assert_eq!(marks.set_pending(&[photo], true)?, 1);
    Ok(())
}

#[test]
fn destination_created_by_another_writer_is_never_overwritten() -> Result<()> {
    let folder = tempfile::tempdir()?;
    let source = folder.path().join("source.NEF");
    let destination = folder.path().join("destination.NEF");
    fs::write(&source, b"source")?;
    fs::write(&destination, b"existing destination")?;
    assert!(move_without_replacing(&source, &destination).is_err());
    assert_eq!(fs::read(&source)?, b"source");
    assert_eq!(fs::read(&destination)?, b"existing destination");
    Ok(())
}

#[cfg(unix)]
#[test]
fn symbolic_link_components_cannot_escape_the_working_folder() -> Result<()> {
    use std::os::unix::fs::symlink;
    let folder = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    fs::write(outside.path().join("outside.NEF"), b"outside")?;
    symlink(outside.path(), folder.path().join("link"))?;
    let mut marks = PendingDeleteStore::load(folder.path())?;
    assert!(marks
        .set_pending(&[folder.path().join("link/outside.NEF")], true)
        .is_err());
    symlink(outside.path(), folder.path().join(DELETED_DIRECTORY))?;
    assert!(restore_all(folder.path()).is_err());
    assert_eq!(fs::read(outside.path().join("outside.NEF"))?, b"outside");
    Ok(())
}

#[cfg(windows)]
#[test]
fn locked_windows_file_leaves_a_recoverable_partial_transaction() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let folder = tempfile::tempdir()?;
    let photo = folder.path().join("photo.NEF");
    let jpeg = folder.path().join("photo.JPG");
    fs::write(&photo, b"raw")?;
    fs::write(&jpeg, b"jpeg")?;
    PendingDeleteStore::load(folder.path())?.set_pending(std::slice::from_ref(&photo), true)?;
    let held = OpenOptions::new().read(true).share_mode(0).open(&photo)?;
    let result = move_marked(folder.path());
    drop(held);
    match result {
        Ok(report) => assert!(!report.failures.is_empty()),
        Err(error) => assert!(!error.to_string().is_empty()),
    }
    let restored = restore_all(folder.path())?;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert_eq!(fs::read(&photo)?, b"raw");
    assert_eq!(fs::read(&jpeg)?, b"jpeg");
    Ok(())
}
