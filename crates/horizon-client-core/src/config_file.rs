//! Publish a complete file without truncating the previous one.

use std::{fs, io, path::Path};

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    write_atomic_with(path, |file| file.write_all(bytes))
}

/// Stream an encoded file beside the destination, then publish it after the writer succeeds.
pub fn write_atomic_with<E: From<io::Error>>(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> Result<(), E>,
) -> Result<(), E> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Keep an existing user link: publish beside its target, not over the link.
    let mut destination = path.to_path_buf();
    let mut links = 0;
    loop {
        match fs::symlink_metadata(&destination) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if links == 40 {
                    return Err(
                        io::Error::other("Too many symbolic links at the output path").into(),
                    );
                }
                links += 1;
                let target = fs::read_link(&destination)?;
                destination = if target.is_absolute() {
                    target
                } else {
                    destination.parent().unwrap_or(Path::new("")).join(target)
                };
            }
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    let permissions = match fs::metadata(&destination) {
        Ok(meta) if meta.is_dir() => {
            return Err(io::Error::from(io::ErrorKind::IsADirectory).into());
        }
        // Non-file destinations (for example /dev/null) have no stored snapshot
        // to replace; retain the ordinary write semantics for those.
        Ok(meta) if !meta.is_file() => return write(&mut fs::File::create(&destination)?),
        Ok(meta) if meta.permissions().readonly() => {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied).into());
        }
        Ok(meta) => Some(meta.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut open = fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    if permissions.is_some() {
        // Keep replacement contents private until their final mode is applied.
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let (temporary, file) = loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary =
            destination.with_file_name(format!(".horizon-config-{}-{n}.tmp", std::process::id()));
        match open.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    struct StagingCleanup<'a> {
        path: &'a Path,
        published: bool,
    }
    impl Drop for StagingCleanup<'_> {
        fn drop(&mut self) {
            if !self.published {
                let _ = fs::remove_file(self.path);
            }
        }
    }
    let mut cleanup = StagingCleanup {
        path: &temporary,
        published: false,
    };
    // The file closes before the cleanup guard runs, including on unwind.
    let mut file = file;
    write(&mut file)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, &destination)?;
    cleanup.published = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[cfg(unix)]
    #[test]
    fn private_config_is_staged_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("horizon-config-mode-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("options.txt");
        fs::write(&path, b"before").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic_with::<io::Error>(&path, |file| {
            assert_eq!(
                file.metadata()?.permissions().mode() & 0o077,
                0,
                "staging file is accessible to group or others"
            );
            file.write_all(b"after")
        })
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"after");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn writer_panic_removes_the_staging_file() {
        let dir = std::env::temp_dir().join(format!("horizon-config-panic-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("options.txt");
        fs::write(&path, b"guiScale:2\n").unwrap();

        let result = std::panic::catch_unwind(|| {
            write_atomic_with::<io::Error>(&path, |file| {
                file.write_all(b"partial replacement")?;
                panic!("fixture encoding panic");
            })
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"guiScale:2\n");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_partial_write_keeps_the_previous_config() {
        let dir = std::env::temp_dir().join(format!("horizon-config-write-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("options.txt");
        fs::write(&path, b"guiScale:2\n").unwrap();

        let result: io::Result<()> = write_atomic_with(&path, |file| {
            file.write_all(b"partial replacement")?;
            Err(io::Error::other("fixture write failure"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"guiScale:2\n");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        write_atomic(&path, b"guiScale:3\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"guiScale:3\n");
        fs::remove_dir_all(dir).unwrap();
    }
}
