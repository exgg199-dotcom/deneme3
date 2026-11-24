//! Protect prepared roots while the game or resource readers retain them.
use super::*;
use horizon_assets::resource_source::ResourceSource;
struct Pin {
    path: PathBuf,
    alive: Box<dyn Fn() -> bool + Send + Sync>,
    _lease: Option<horizon_assets::root_lease::RootLease>,
}
static PINS: std::sync::Mutex<Vec<Pin>> = std::sync::Mutex::new(Vec::new());

/// Keep a root protected from pruning until the final token clone is dropped.
pub fn retain_root(path: &Path) -> std::sync::Arc<()> {
    let token = std::sync::Arc::new(());
    let weak = std::sync::Arc::downgrade(&token);
    register(path, Box::new(move || weak.strong_count() != 0));
    token
}
pub(super) fn retain_source(path: &Path, source: &ResourceSource) {
    register(path, Box::new(source.liveness()));
}
fn register(path: &Path, alive: Box<dyn Fn() -> bool + Send + Sync>) {
    let lease = horizon_assets::root_lease::lease_for(path);
    let mut pins = PINS.lock().unwrap();
    pins.retain(|pin| (pin.alive)());
    pins.push(Pin {
        path: path.to_owned(),
        alive,
        _lease: lease,
    });
}
pub(super) fn release_dead_pins() {
    PINS.lock().unwrap().retain(|pin| (pin.alive)());
}
fn protected(path: &Path) -> bool {
    let mut pins = PINS.lock().unwrap();
    pins.retain(|pin| (pin.alive)());
    pins.iter().any(|pin| pin.path == path)
}

pub(super) struct StagingRoot(pub(super) PathBuf);
impl Drop for StagingRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Remove stale, unretained roots other than the current candidate.
/// Game tokens protect the active root through preparation failures; resource
/// source observers protect older generations for as long as readers own them.
pub(super) fn prune_roots(roots: &Path, current: &Path, keep: std::time::Duration) {
    use horizon_assets::root_lease::RootLease;
    release_dead_pins();
    let Ok(entries) = fs::read_dir(roots) else {
        return;
    };
    let old_layout = roots.join(STAMP).is_file();
    let age = |file: PathBuf| {
        fs::metadata(file)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
    };
    let stale = |path: &Path| {
        [age(path.join(STAMP)), age(used_path(path))]
            .into_iter()
            .flatten()
            .min()
            .is_some_and(|age| age > keep)
    };
    let legacy = if old_layout && roots != current && stale(roots) && !protected(roots) {
        RootLease::try_exclusive(roots).ok().flatten()
    } else {
        None
    };
    let clean_legacy = legacy.is_some() && stale(roots) && !protected(roots);
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current || protected(&path) {
            continue;
        }
        let name = entry.file_name();
        // A root's reuse marker belongs to that root, not the legacy layout.
        // Remove it only after deleting its directory successfully.
        if name
            .to_str()
            .and_then(|name| name.strip_suffix(".used"))
            .is_some_and(|name| {
                name.len() == ROOT_NAME && name.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            continue;
        }
        let is_root = name.len() == ROOT_NAME
            && name
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            && path.is_dir();
        if is_root {
            if !stale(&path) {
                continue;
            }
            let guard = match RootLease::try_exclusive(&path) {
                Ok(Some(guard)) => guard,
                Ok(None) => continue,
                Err(error) => {
                    log::warn!("Horizon: cannot collect {}: {error}", path.display());
                    continue;
                }
            };
            if !stale(&path) || protected(&path) {
                continue;
            }
            if let Err(error) = fs::remove_dir_all(&path).inspect(|_| {
                let _ = fs::remove_file(used_path(&path));
            }) {
                log::warn!(
                    "Horizon: an old mod content root stays: {}: {error}",
                    path.display()
                );
            }
            drop(guard);
        } else if clean_legacy {
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(error) = removed {
                log::warn!(
                    "Horizon: an old mod content root stays: {}: {error}",
                    path.display()
                );
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_and_retained_readers_protect_stale_roots_until_both_release() {
        let scratch = tempfile::tempdir().unwrap();
        let roots = scratch.path().join("mod-content");
        fs::create_dir(&roots).unwrap();
        let old = roots.join("0123456789abcdef");
        fs::create_dir(&old).unwrap();
        fs::write(old.join(STAMP), b"old").unwrap();
        let token = retain_root(&old);
        let source = ResourceSource::directory(old.clone());
        retain_source(&old, &source);
        let current = roots.join("fedcba9876543210");
        prune_roots(&roots, &current, std::time::Duration::ZERO);
        assert!(old.is_dir());
        drop(token);
        prune_roots(&roots, &current, std::time::Duration::ZERO);
        assert!(old.is_dir());
        drop(source);
        prune_roots(&roots, &current, std::time::Duration::ZERO);
        assert!(!old.exists());
    }
    #[test]
    fn source_pins_hold_file_leases_and_legacy_cleanup_keeps_reuse_markers() {
        use horizon_assets::root_lease::RootLease;
        let scratch = tempfile::tempdir().unwrap();
        let roots = scratch.path().join("mod-content");
        let old = roots.join("0123456789abcdef");
        fs::create_dir_all(&old).unwrap();
        for file in [roots.join(STAMP), old.join(STAMP), used_path(&old)] {
            fs::write(&file, b"old").unwrap();
            fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                .unwrap();
        }
        let lease = RootLease::acquire(&old).unwrap();
        let source = ResourceSource::directory(old.clone());
        retain_source(&old, &source);
        drop(lease);
        assert!(RootLease::try_exclusive(&old).unwrap().is_none());
        let current = roots.join("fedcba9876543210");
        prune_roots(&roots, &current, std::time::Duration::ZERO);
        assert!(old.is_dir());
        assert!(used_path(&old).is_file());
        drop(source);
        release_dead_pins();
        assert!(RootLease::try_exclusive(&old).unwrap().is_some());
        prune_roots(&roots, &current, std::time::Duration::ZERO);
        assert!(!old.exists());
        assert!(!used_path(&old).exists());
    }

    #[test]
    fn unsuccessful_staging_is_removed_but_previous_root_survives() {
        let scratch = tempfile::tempdir().unwrap();
        let old = scratch.path().join("old");
        fs::create_dir(&old).unwrap();
        let stage = scratch.path().join("candidate-tables");
        let failure = || -> Result<()> {
            let _stage = StagingRoot(stage.clone());
            fs::create_dir(&stage)?;
            fs::write(stage.join("partial.json"), b"{}")?;
            Err("handler refused".into())
        };
        assert!(failure().is_err());
        assert!(!stage.exists());
        assert!(old.exists());
    }
}
