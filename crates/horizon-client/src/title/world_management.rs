//! World-list operations. Metadata edits preserve every other NBT field;
//! backups use vanilla's ZIP layout and never include the session lock.
use crate::save::{read_dat, write_dat};
use horizon_core::nbt::Tag;
use std::{
    fs,
    io::{self, Seek, Write},
    path::{Path, PathBuf},
};

fn world_path(saves: &Path, folder: &str) -> crate::Result<PathBuf> {
    if Path::new(folder).components().count() != 1
        || !matches!(
            Path::new(folder).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err("Invalid world folder".into());
    }
    let path = saves.join(folder);
    if !fs::symlink_metadata(&path)?.is_dir() {
        return Err("World folder is not a directory".into());
    }
    Ok(path)
}

/// EditWorldScreen holds a validated LevelStorageAccess while editing or
/// backing up. File-command callers need the same ownership for the operation.
fn world_access(saves: &Path, folder: &str) -> crate::Result<(PathBuf, crate::save::SessionLock)> {
    let world = world_path(saves, folder)?;
    let lock = crate::save::SessionLock::acquire_existing(
        &world,
        Some(&saves.join("allowed_symlinks.txt")),
    )?;
    Ok((world, lock))
}

/// Forbidden links for the world-open warning. Use the same Java-compatible
/// allowlist as the access lock, so explicitly permitted links stay usable.
pub fn symlinks(saves: &Path, folder: &str) -> crate::Result<Vec<PathBuf>> {
    let world = world_path(saves, folder)?;
    Ok(
        crate::world_links::validate(&world, &saves.join("allowed_symlinks.txt"))?
            .into_iter()
            .map(|entry| entry.link)
            .collect(),
    )
}

pub fn rename(saves: &Path, folder: &str, name: &str) -> crate::Result<()> {
    let (world, _lock) = world_access(saves, folder)?;
    let path = world.join("level.dat");
    let mut root = read_dat(&path).ok_or("Could not read level.dat")?;
    let Some(Tag::Compound(data)) = root.get_mut("Data") else {
        return Err("Missing level data".into());
    };
    data.insert("LevelName".into(), Tag::String(name.trim().into()));
    write_dat(&path, root)
}

pub(super) fn reset_icon(saves: &Path, folder: &str) -> crate::Result<()> {
    let (world, _lock) = world_access(saves, folder)?;
    fs::remove_file(world.join("icon.png"))?;
    Ok(())
}

/// `LevelStorageAccess.deleteLevel`. The world's
/// session lock is taken first, so a world open in this or another instance is
/// refused rather than pulled from under it; everything but the lock goes,
/// then the lock is released and removed with the folder. A failed pass (a
/// file briefly held by a virus scanner, say) is retried up to five times,
/// half a second apart, as vanilla does. Only the named folder directly in
/// `saves` is touched: symbolic links inside it are removed, never followed.
pub fn delete(saves: &Path, folder: &str) -> crate::Result<()> {
    let world = world_path(saves, folder)?;
    let lock = crate::save::SessionLock::acquire_existing(&world, None)?;
    let lock_path = world.join("session.lock");
    log::info!("Deleting level {folder}");
    for attempt in 1.. {
        match remove_contents(&world, &lock_path) {
            Ok(()) => break,
            Err(e) if attempt >= 5 => return Err(e.into()),
            Err(e) => {
                log::warn!("Failed to delete {}: {e}", world.display());
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    drop(lock);
    match fs::remove_file(&lock_path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    fs::remove_dir(&world)?;
    Ok(())
}

fn remove_contents(world: &Path, lock: &Path) -> io::Result<()> {
    for entry in fs::read_dir(world)? {
        let entry = entry?;
        let path = entry.path();
        if path == lock {
            continue;
        }
        // `file_type` does not follow links, and `remove_dir_all` never does.
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

pub fn backups_path(saves: &Path) -> PathBuf {
    saves.parent().unwrap_or(saves).join("backups")
}

/// `Blaze3D.openPath` on the world's (or the backups') folder, made first if it is missing.
pub(super) fn open_folder(path: &Path) -> crate::Result<()> {
    fs::create_dir_all(path)?;
    crate::desktop::open_path(path)?;
    Ok(())
}

/// A zip of the whole world folder in the backups folder: where it is, and its size.
pub fn backup(saves: &Path, folder: &str) -> crate::Result<(PathBuf, u64)> {
    let (world, _lock) = world_access(saves, folder)?;
    let backups = backup_destination(saves, &world)?;
    let base = format!(
        "{}_{}",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S"),
        folder
    );
    let mut staged = tempfile::Builder::new()
        .prefix(".horizon-backup-")
        .suffix(".tmp")
        .tempfile_in(&backups)?;
    let mut zip = zip::ZipWriter::new(staged.as_file_mut());
    write_tree(&mut zip, &world, &world, folder)?;
    let file = zip.finish()?;
    file.sync_all()?;
    let bytes = file.metadata()?.len();
    let path = publish_backup(staged, &backups, &base)?;
    Ok((path, bytes))
}

fn backup_destination(saves: &Path, world: &Path) -> io::Result<PathBuf> {
    let backups = backups_path(saves);
    fs::create_dir_all(&backups)?;
    let backups = fs::canonicalize(backups)?;
    if backups.starts_with(fs::canonicalize(world)?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The backups folder points inside the world being backed up.",
        ));
    }
    Ok(backups)
}

fn backup_name(base: &str, suffix: &str) -> String {
    let room = super::world_folder::MAX_FILE_NAME - suffix.len() - ".zip".len();
    // Java counts UTF-16 units. Filesystems using UTF-8 paths can hit their
    // byte limit sooner, so reserve room for the suffix and extension there too.
    #[cfg(windows)]
    let base = super::world_folder::java_prefix(base, room);
    #[cfg(not(windows))]
    let base = &base[..base.floor_char_boundary(room)];
    format!("{base}{suffix}.zip")
}

fn publish_backup(
    mut staged: tempfile::NamedTempFile,
    backups: &Path,
    base: &str,
) -> io::Result<PathBuf> {
    for index in 0..=u32::MAX {
        let suffix = if index == 0 {
            String::new()
        } else {
            format!(" ({index})")
        };
        let path = backups.join(backup_name(base, &suffix));
        match staged.persist_noclobber(&path) {
            Ok(_) => return Ok(path),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                staged = error.file;
            }
            Err(error) if exclusive_publish_unavailable(&error.error) => {
                return publish_backup_with_unique_name(error.file, backups, base);
            }
            Err(error) => return Err(error.error),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}

fn exclusive_publish_unavailable(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        // Darwin's ENOTSUP (45), reported as Uncategorized on exFAT.
        || (cfg!(target_vendor = "apple") && error.raw_os_error() == Some(45))
}

fn publish_backup_with_unique_name(
    staged: tempfile::NamedTempFile,
    backups: &Path,
    base: &str,
) -> io::Result<PathBuf> {
    // Some removable filesystems support neither exclusive rename nor hard
    // links. Each concurrent backup owns a distinct staging name, so add its
    // random suffix before renaming the complete file into view.
    let suffix = staged
        .path()
        .file_stem()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".horizon-backup-"))
        .ok_or_else(|| io::Error::other("Backup staging file has no unique name"))?;
    let path = backups.join(backup_name(base, &format!(" ({suffix})")));
    match fs::symlink_metadata(&path) {
        Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    staged.persist(&path).map_err(|error| error.error)?;
    Ok(path)
}

#[cfg(test)]
mod backup_publication_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn backup_destination_cannot_be_inside_its_source() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let saves = home.path().join("saves");
        let world = saves.join("World");
        let inside = world.join("nested");
        let outside = home.path().join("external");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir(&outside).unwrap();
        let link = backups_path(&saves);

        symlink(&inside, &link).unwrap();
        let error = backup_destination(&saves, &world).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_dir(&inside).unwrap().count(), 0);

        fs::remove_file(&link).unwrap();
        symlink(&outside, &link).unwrap();
        assert_eq!(
            backup_destination(&saves, &world).unwrap(),
            fs::canonicalize(outside).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn backup_refuses_file_names_it_cannot_preserve() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join(r"region\chunk"), b"world data").unwrap();
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        assert!(write_tree(&mut writer, source.path(), source.path(), "World").is_err());

        let invalid = OsString::from_vec(vec![0xff]);
        assert!(crate::import::zip_path(Path::new(&invalid)).is_err());
        assert_eq!(
            crate::import::zip_path(Path::new("region/région.mca")).unwrap(),
            "region/région.mca"
        );
    }

    #[test]
    fn world_backup_omits_only_the_root_session_lock() {
        let world = tempfile::tempdir().unwrap();
        let root = world.path();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("session.lock"), b"world lock").unwrap();
        fs::write(root.join("data/session.lock"), b"saved mod data").unwrap();
        fs::write(root.join("level.dat"), b"world data").unwrap();

        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        write_tree(&mut writer, root, root, "World").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let mut archive = zip::ZipArchive::new(io::Cursor::new(&bytes)).unwrap();
        assert!(archive.by_name("World/session.lock").is_err());
        assert!(archive.by_name("World/data/session.lock").is_ok());

        let home = tempfile::tempdir().unwrap();
        let backup = home.path().join("backup.zip");
        fs::write(&backup, bytes).unwrap();
        let restored = crate::import::Pack::open(&backup)
            .unwrap()
            .install(home.path())
            .unwrap();
        assert!(!restored.path.join("session.lock").exists());
        assert_eq!(
            fs::read(restored.path.join("data/session.lock")).unwrap(),
            b"saved mod data"
        );
    }

    #[test]
    #[ignore = "daily"]
    fn backup_tree_can_write_a_zip64_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.bin");
        let size = u64::from(u32::MAX) + 1;
        fs::File::create(path).unwrap().set_len(size).unwrap();
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        write_tree_skipping(
            &mut writer,
            directory.path(),
            directory.path(),
            "world",
            &[],
        )
        .unwrap();
        let mut archive = zip::ZipArchive::new(writer.finish().unwrap()).unwrap();
        let entry = archive.by_name("world/large.bin").unwrap();
        assert_eq!(entry.size(), size);
        assert!(entry.compressed_size() < size);
    }

    #[test]
    fn long_world_backup_names_fit_with_and_without_a_collision() {
        for folder in ["a".repeat(255), "界".repeat(84)] {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join(&folder)).unwrap();
            let base = format!("2026-11-18_04-00-00_{folder}");
            let publish = |payload: &[u8]| {
                let mut staged = tempfile::Builder::new()
                    .prefix(".horizon-backup-")
                    .suffix(".tmp")
                    .tempfile_in(dir.path())
                    .unwrap();
                staged.write_all(payload).unwrap();
                publish_backup(staged, dir.path(), &base).unwrap()
            };

            let first = publish(b"first backup");
            let second = publish(b"second backup");
            assert_ne!(first, second);
            assert_eq!(first.extension().unwrap(), "zip");
            assert_eq!(second.extension().unwrap(), "zip");
            assert_eq!(fs::read(first).unwrap(), b"first backup");
            assert_eq!(fs::read(second).unwrap(), b"second backup");
        }
    }

    #[test]
    fn completed_backup_publishes_without_overwriting_an_existing_archive() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("world.zip");
        fs::write(&original, b"previous backup").unwrap();
        let mut staged = tempfile::Builder::new()
            .prefix(".horizon-backup-")
            .suffix(".tmp")
            .tempfile_in(dir.path())
            .unwrap();
        let mut zip = zip::ZipWriter::new(staged.as_file_mut());
        zip.start_file("world/level.dat", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"replacement backup").unwrap();
        zip.finish().unwrap();
        assert!(!dir.path().join("world (1).zip").exists());
        let published = publish_backup(staged, dir.path(), "world").unwrap();
        assert_ne!(published, original);
        assert_eq!(fs::read(original).unwrap(), b"previous backup");
        let mut archive = zip::ZipArchive::new(fs::File::open(published).unwrap()).unwrap();
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut archive.by_name("world/level.dat").unwrap(), &mut bytes)
            .unwrap();
        assert_eq!(bytes, b"replacement backup");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}

fn write_tree<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    root: &Path,
    dir: &Path,
    folder: &str,
) -> crate::Result<()> {
    write_tree_skipping(zip, root, dir, folder, &["session.lock"])
}

/// Every regular file under `root` (named `<prefix>/<relative>` in the zip),
/// except paths in `skip` relative to the root. Symlinks are refused. The repro bundle zips a
/// whole recording with it.
pub fn write_tree_skipping<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    root: &Path,
    dir: &Path,
    prefix: &str,
    skip: &[&str],
) -> crate::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        if skip.iter().any(|name| relative == Path::new(name)) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(format!("Cannot back up symbolic link: {}", path.display()).into());
        }
        if kind.is_dir() {
            write_tree_skipping(zip, root, &path, prefix, skip)?;
        } else if kind.is_file() {
            let prefix = crate::import::zip_path(Path::new(prefix))?;
            let relative = crate::import::zip_path(relative)?;
            let name = format!("{prefix}/{relative}");
            let file = fs::File::open(path)?;
            let file_size = file.metadata()?.len();
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            crate::import::write_zip_entry(zip, &name, file, file_size, options)?;
        }
    }
    Ok(())
}
