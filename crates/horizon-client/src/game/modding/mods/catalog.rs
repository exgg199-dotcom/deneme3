//! Worker-owned descriptions and immutable sources for content generations.
use horizon_assets::root_lease::RootLease;
use horizon_client::mod_list::Installed;
use horizon_client::mod_registry::Cancellation;
use horizon_mod_package::{Content, Manifest, Package};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub revision: String,
    pub live: bool,
    pub source: PathBuf,
    pub manifest: Manifest,
    pub dev: bool,
    pub lease: RootLease,
}
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision && self.live == other.live && self.dev == other.dev
    }
}
impl Eq for Entry {}

#[derive(Clone, Debug, Default)]
pub(crate) struct Catalog {
    pub native_ids: BTreeSet<String>,
    pub rows: Vec<Installed>,
    pub entries: BTreeMap<String, Arc<Entry>>,
    pub order: Vec<String>,
    pub invalid: BTreeSet<String>,
    pub info: BTreeMap<String, horizon_client::mod_list::ModInfo>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Selection {
    pub enabled: BTreeSet<String>,
    pub entries: BTreeMap<String, Arc<Entry>>,
    pub order: Vec<String>,
}
impl Selection {
    pub fn wanted(enabled: &BTreeSet<String>, catalog: &Catalog) -> Self {
        Self {
            enabled: enabled.clone(),
            entries: catalog
                .entries
                .iter()
                .filter(|(id, entry)| {
                    enabled.contains(*id) || entry.dev || catalog.native_ids.contains(*id)
                })
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect(),
            order: catalog
                .order
                .iter()
                .filter(|id| {
                    catalog.entries.get(*id).is_some_and(|entry| {
                        enabled.contains(*id) || entry.dev || catalog.native_ids.contains(*id)
                    })
                })
                .cloned()
                .collect(),
        }
    }
}

/// One immutable content generation, built by the canonical upstream loader.
#[derive(Clone)]
pub(crate) struct PreparedContent {
    pub root: horizon_assets::resource_source::ResourceRoot,
    pub catalog: Arc<Catalog>,
    pub selection: Selection,
    /// Metadata for this generation; no jobs, transactions or previous content.
    pub sources: super::ModsState,
    pub data_changed: bool,
}
impl std::fmt::Debug for PreparedContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedContent")
            .field("root", &self.root)
            .field("selection", &self.selection)
            .field("data_changed", &self.data_changed)
            .finish()
    }
}

pub(crate) struct Switch {
    pub next: Selection,
    pub waiting: BTreeSet<String>,
}

/// Classify differences against the running generation, not just enabled IDs.
/// Both generations must be live-safe: removing old registry content also
/// changes IDs even if the replacement contains only assets.
pub(crate) fn switch_plan(
    applied: &Selection,
    wanted: &BTreeSet<String>,
    catalog: &Catalog,
    local: bool,
) -> Switch {
    let mut next = Selection::wanted(wanted, catalog);
    let ids: BTreeSet<_> = applied
        .entries
        .keys()
        .chain(next.entries.keys())
        .chain(
            catalog
                .invalid
                .iter()
                .filter(|id| wanted.contains(*id) || applied.enabled.contains(*id)),
        )
        .cloned()
        .collect();
    let mut waiting = BTreeSet::new();
    for id in ids {
        let old = applied.entries.get(&id);
        let new = next.entries.get(&id);
        if old == new && !catalog.invalid.contains(&id) {
            continue;
        }
        let can_apply = local
            && !catalog.invalid.contains(&id)
            && old.is_none_or(|entry| entry.live)
            && new.is_none_or(|entry| entry.live);
        if can_apply {
            continue;
        }
        waiting.insert(id.clone());
        match old {
            Some(entry) => {
                next.entries.insert(id.clone(), entry.clone());
            }
            None => {
                next.entries.remove(&id);
            }
        }
        if applied.enabled.contains(&id) {
            next.enabled.insert(id);
        } else {
            next.enabled.remove(&id);
        }
    }
    // Keep existing registry precedence stable through a live switch. Newly
    // enabled assets/data-only entries follow the captured catalog order.
    let mut order: Vec<_> = applied
        .order
        .iter()
        .filter(|id| next.entries.contains_key(*id))
        .cloned()
        .collect();
    for id in &catalog.order {
        if next.entries.contains_key(id) && !order.contains(id) {
            order.push(id.clone());
        }
    }
    next.order = order;
    Switch { next, waiting }
}

fn cancelled(cancel: &Cancellation) -> Result<(), String> {
    if cancel.cancelled() {
        Err("mod catalogue read cancelled".into())
    } else {
        Ok(())
    }
}

/// Hash/copy on the worker in bounded chunks. Reuse the project's SHA-256
/// machinery; no package-wide byte allocation is needed to retain an old source.
fn hash_file(path: &Path, hash: &mut Sha256, cancel: &Cancellation) -> Result<(), String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buffer = [0; 64 * 1024];
    loop {
        cancelled(cancel)?;
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(())
}

fn revision(path: &Path, files: &[String], cancel: &Cancellation) -> Result<String, String> {
    let mut hash = Sha256::new();
    if path.is_dir() {
        for name in files {
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update(
                fs::metadata(path.join(name))
                    .map_err(|e| e.to_string())?
                    .len()
                    .to_le_bytes(),
            );
            hash_file(&path.join(name), &mut hash, cancel)?;
        }
    } else {
        hash_file(path, &mut hash, cancel)?;
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn copy_file(from: &Path, to: &Path, cancel: &Cancellation) -> Result<(), String> {
    fs::create_dir_all(to.parent().ok_or("snapshot file has no parent")?)
        .map_err(|e| e.to_string())?;
    let mut input = fs::File::open(from).map_err(|e| e.to_string())?;
    let mut output = fs::File::create(to).map_err(|e| e.to_string())?;
    let mut buffer = [0; 64 * 1024];
    loop {
        cancelled(cancel)?;
        let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
    }
    output.sync_all().map_err(|e| e.to_string())
}

/// File names only: package preparation stays streamed even for large resource
/// trees. Keep the metadata the upstream package reader exposes, including icons,
/// layouts, and the source of directory-based code packages.
fn snapshot_files(
    path: &Path,
    content: &Content,
    cancel: &Cancellation,
) -> Result<Vec<String>, String> {
    cancelled(cancel)?;
    let mut files = content.files.clone();
    files.push(horizon_mod_package::manifest::MANIFEST.into());
    for name in [
        horizon_mod_package::manifest::WASM,
        "icon.png",
        horizon_mod_package::layouts::FILE,
    ] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(_) => files.push(name.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    if path.join("Cargo.toml").is_file() {
        let mut pending = vec![path.to_owned()];
        let mut count = 0usize;
        let mut bytes = 0u64;
        let mut directories = 0usize;
        while let Some(dir) = pending.pop() {
            cancelled(cancel)?;
            directories += 1;
            if directories > 4096 {
                return Err("source directory limit exceeded".into());
            }
            for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
                cancelled(cancel)?;
                let entry = entry.map_err(|e| e.to_string())?;
                let candidate = entry.path();
                let name = candidate
                    .strip_prefix(path)
                    .map_err(|e| e.to_string())?
                    .to_str()
                    .ok_or("non-UTF-8 package path")?
                    .replace('\\', "/");
                let kind = entry.file_type().map_err(|e| e.to_string())?;
                if kind.is_dir() {
                    if name == "src" || name.starts_with("src/") {
                        if !name
                            .split('/')
                            .any(|part| horizon_mod_package::manifest::NOT_PACKAGED.contains(&part))
                        {
                            if pending.len() >= 512 {
                                return Err("source directory limit exceeded".into());
                            }
                            pending.push(candidate);
                        }
                    }
                } else if horizon_mod_package::manifest::source_file(&name) {
                    count += 1;
                    bytes = bytes
                        .checked_add(entry.metadata().map_err(|e| e.to_string())?.len())
                        .ok_or("source too large")?;
                    if count > 511 || bytes > 64 << 20 {
                        return Err("source exceeds package size limit".into());
                    }
                    files.push(name);
                }
            }
        }
    }
    let canonical = fs::canonicalize(path).map_err(|e| e.to_string())?;
    files.sort();
    files.dedup();
    for file in &files {
        cancelled(cancel)?;
        let target = fs::canonicalize(path.join(file)).map_err(|e| e.to_string())?;
        if !target.starts_with(&canonical) || !target.is_file() {
            return Err(format!("package snapshot path leaves its source: {file}"));
        }
    }
    Ok(files)
}

/// A running registry-bearing mod must remain reproducible after an update
/// replaces its installed archive. Retain a verified immutable source and its
/// lease, rather than quietly rebuilding that old ID from the new archive.
pub(super) fn pin_source(
    home: &Path,
    path: &Path,
    content: &Content,
    cancel: &Cancellation,
) -> Result<(PathBuf, String, RootLease), String> {
    let directory = path.is_dir();
    let files = if directory {
        snapshot_files(path, content, cancel)?
    } else {
        Vec::new()
    };
    let digest = revision(path, &files, cancel)?;
    let root = home.join(".horizon-cache/mod-sources").join(&digest);
    let _builder = RootLease::build_lock(&root).map_err(|e| e.to_string())?;
    let source = if directory {
        root.join("files")
    } else {
        root.join("package.hmod")
    };
    let stamp = root.join("horizon-mod-source.stamp");
    let lease = RootLease::acquire(&root).map_err(|e| e.to_string())?;
    if fs::read_to_string(&stamp).is_ok_and(|kept| kept == digest) && source.exists() {
        cancelled(cancel)?;
        fs::write(&stamp, &digest).map_err(|e| e.to_string())?;
        return Ok((source, digest, lease));
    }
    drop(lease);
    // The caller holds the home snapshot gate; no other builder can publish
    // this source while its previous interrupted attempt is being replaced.
    let writer = RootLease::try_exclusive(&root)
        .map_err(|e| e.to_string())?
        .ok_or("incomplete mod snapshot still has readers; retry after they finish")?;
    if root.exists() {
        fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    if directory {
        if path.join("src").is_dir() {
            fs::create_dir_all(source.join("src")).map_err(|e| e.to_string())?;
        }
        for name in &files {
            copy_file(&path.join(name), &source.join(name), cancel)?;
        }
    } else {
        copy_file(path, &source, cancel)?;
    }
    if directory {
        let package = Package::read(path)?;
        let current = Content::read(path, &package.manifest)?;
        let current_files = snapshot_files(path, &current, cancel)?;
        if current_files != files {
            return Err("mod files changed during snapshot preparation; retry".into());
        }
    }
    if revision(&source, &files, cancel)? != digest || revision(path, &files, cancel)? != digest {
        return Err(
            "mod changed while its source snapshot was being built; retry the refresh".into(),
        );
    }
    cancelled(cancel)?;
    fs::write(&stamp, &digest).map_err(|e| e.to_string())?;
    let lease = writer
        .into_shared(RootLease::default())
        .map_err(|e| e.to_string())?;
    Ok((source, digest, lease))
}

/// The caller holds mod_registry::with_content_snapshot on a worker.
pub(crate) fn read(
    home: &Path,
    dev: &[PathBuf],
    cancel: &Cancellation,
) -> Result<Arc<Catalog>, String> {
    read_at(home, dev, cancel, |_| {})
}

pub(crate) fn read_at(
    home: &Path,
    dev: &[PathBuf],
    cancel: &Cancellation,
    mut before_pin: impl FnMut(&Path),
) -> Result<Arc<Catalog>, String> {
    let mut paths = horizon_mod_package::find(&home.join(horizon_client::mod_list::DIR));
    paths.retain(|path| !dev.contains(path));
    paths.extend(dev.iter().cloned());
    let mut catalog = Catalog::default();
    for path in paths {
        cancelled(cancel)?;
        let mut row = Installed {
            id: String::new(),
            title: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            description: String::new(),
            problem: None,
            dev: dev.contains(&path),
            native: None,
        };
        let result = (|| {
            let package = Package::read(&path)?;
            let manifest = &package.manifest;
            row.id = manifest.id.clone();
            row.title = format!("{} {}", manifest.name, manifest.version);
            row.description = manifest.description.clone();
            // Compiled native identities come from Runtime, not a directory name.
            // Their executable-adjacent content is already validated and retained.
            if package.is_native() {
                catalog.info.insert(
                    manifest.id.clone(),
                    horizon_client::mod_list::ModInfo::of(manifest, package.icon.as_ref()),
                );
                return Ok(());
            }
            let declared = super::super::native_content_items::owned_item_ids(
                manifest,
                &super::native_declared().items,
            );
            let content = Content::read_declaring(&path, manifest, &declared)?;
            before_pin(&path);
            let (source, revision, lease) = pin_source(home, &path, &content, cancel)?;
            // Classification and metadata describe the pinned bytes, not a
            // mutable package read before a developer edited it.
            let pinned = Package::read(&source)?;
            let manifest = &pinned.manifest;
            let declared = super::super::native_content_items::owned_item_ids(
                manifest,
                &super::native_declared().items,
            );
            let content = Content::read_declaring(&source, manifest, &declared)?;
            let live = pinned.manifest.wasm.is_none()
                && horizon_client::mod_content::live_switch(&content).is_ok();
            row.id = manifest.id.clone();
            row.title = format!("{} {}", manifest.name, manifest.version);
            row.description = manifest.description.clone();
            catalog.info.insert(
                manifest.id.clone(),
                horizon_client::mod_list::ModInfo::of(manifest, pinned.icon.as_ref()),
            );
            if !catalog.entries.contains_key(&manifest.id) {
                catalog.order.push(manifest.id.clone());
                catalog.entries.insert(
                    manifest.id.clone(),
                    Arc::new(Entry {
                        revision,
                        live,
                        source,
                        manifest: manifest.clone(),
                        dev: row.dev,
                        lease,
                    }),
                );
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = result {
            cancelled(cancel)?;
            if row.id.is_empty() {
                if let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|id| horizon_mod_package::index::valid_id(id))
                {
                    catalog.invalid.insert(id.into());
                }
            } else {
                catalog.invalid.insert(row.id.clone());
            }
            row.problem = Some(error);
        }
        catalog.rows.push(row);
    }
    Ok(Arc::new(catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "horizon-catalog-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join(horizon_client::mod_list::DIR)).unwrap();
            Self(path)
        }
        fn package(&self, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(horizon_client::mod_list::DIR).join("ruby.hmod");
            fs::write(&path, bytes).unwrap();
            path
        }
        fn read(&self) -> Arc<Catalog> {
            read(&self.0, &[], &Cancellation::default()).unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn package(version: &str, registry: bool) -> Vec<u8> {
        let manifest = format!(
            "id = \"ruby\"\nname = \"Ruby\"\nversion = \"{version}\"\napi = \"0.2\"\nside = \"both\"\ncapabilities = [\"registries\"]\n"
        );
        let file = if registry {
            "data/ruby/horizon/item/gem.json"
        } else {
            "assets/ruby/lang/en_us.json"
        };
        let bytes = if registry {
            br#"{"template":"minecraft:diamond"}"#.as_slice()
        } else {
            version.as_bytes()
        };
        Package::write_files(&manifest, &[(file, bytes)]).unwrap()
    }
    #[test]
    fn directory_snapshot_keeps_icon_and_layout_metadata() {
        let dir = Scratch::new();
        let path = dir.0.join("dev-ruby");
        fs::create_dir_all(path.join("assets/ruby/lang")).unwrap();
        fs::write(path.join("assets/ruby/lang/en_us.json"), b"{}").unwrap();
        fs::write(path.join("horizon-mod.toml"),b"id=\"ruby\"\nname=\"Ruby\"\nversion=\"1\"\napi=\"0.2\"\ncapabilities=[\"registries\"]\n").unwrap();
        let mut image = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut image, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[12, 34, 56, 255])
                .unwrap();
        }
        fs::write(path.join("icon.png"), &image).unwrap();
        fs::write(
            path.join("layouts.json"),
            br#"{"ruby":{"ruby::T":"0123456789abcdef"}}"#,
        )
        .unwrap();
        let original = Package::read(&path).unwrap();
        let catalog = read(&dir.0, std::slice::from_ref(&path), &Default::default()).unwrap();
        let pinned = Package::read(&catalog.entries["ruby"].source).unwrap();
        assert_eq!(pinned.icon.as_ref().unwrap().bytes(), image);
        assert_eq!(
            pinned.layouts.first_difference("ruby", &original.layouts),
            None
        );
    }

    #[test]
    fn same_enabled_id_update_uses_revision_and_retains_old_real_bytes() {
        let dir = Scratch::new();
        dir.package(&package("1.0.0", false));
        let old = dir.read();
        let enabled = BTreeSet::from(["ruby".into()]);
        let running = Selection::wanted(&enabled, &old);
        dir.package(&package("2.0.0", false));
        let new = dir.read();
        let plan = switch_plan(&running, &enabled, &new, true);
        assert_ne!(plan.next, running);
        assert!(plan.waiting.is_empty());
        assert_eq!(
            Package::read(&running.entries["ruby"].source)
                .unwrap()
                .manifest
                .version,
            "1.0.0"
        );
        assert_eq!(
            Package::read(&plan.next.entries["ruby"].source)
                .unwrap()
                .manifest
                .version,
            "2.0.0"
        );
        assert!(
            RootLease::try_exclusive(running.entries["ruby"].source.parent().unwrap())
                .unwrap()
                .is_none()
        );
        let remote = switch_plan(&running, &enabled, &new, false);
        assert_eq!(remote.next, running);
        assert!(remote.waiting.contains("ruby"));
    }
    #[test]
    fn registry_to_assets_update_waits_and_disabled_broken_pack_does_not() {
        let dir = Scratch::new();
        dir.package(&package("1.0.0", true));
        let old = dir.read();
        let enabled = BTreeSet::from(["ruby".into()]);
        let running = Selection::wanted(&enabled, &old);
        dir.package(&package("2.0.0", false));
        let mut new = (*dir.read()).clone();
        new.invalid.insert("broken_disabled".into());
        let plan = switch_plan(&running, &enabled, &new, true);
        assert_eq!(plan.next, running);
        assert_eq!(plan.waiting, BTreeSet::from(["ruby".into()]));
    }
    #[test]
    fn editing_installed_archive_between_read_and_pin_classifies_pinned_generation() {
        let dir = Scratch::new();
        dir.package(&package("1.0.0", false));
        let replacement = package("2.0.0", true);
        let catalog = read_with(&dir.0, &[], &Cancellation::default(), |path| {
            fs::write(path, &replacement).unwrap()
        })
        .unwrap();
        let entry = &catalog.entries["ruby"];
        assert!(
            !entry.live,
            "the pinned registry change must not be classified by the earlier asset-only archive"
        );
        assert_eq!(entry.manifest.version, "2.0.0");
        assert_eq!(
            Package::read(&entry.source).unwrap().manifest,
            entry.manifest
        );
        assert_eq!(catalog.rows[0].title, "Ruby 2.0.0");
    }
    #[test]
    fn cancelled_refresh_does_not_publish_a_partial_catalogue() {
        let dir = Scratch::new();
        dir.package(&package("1.0.0", false));
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(read(&dir.0, &[], &cancel).is_err());
        assert!(!dir.0.join(".horizon-cache/mod-sources").exists());
    }
}

#[cfg(test)]
fn read_with(
    home: &Path,
    dev: &[PathBuf],
    cancel: &Cancellation,
    before_pin: impl FnMut(&Path),
) -> Result<Arc<Catalog>, String> {
    read_at(home, dev, cancel, before_pin)
}
impl Catalog {
    pub fn wanted_ids(
        &self,
        enabled: &BTreeSet<String>,
        disabled: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        enabled
            .iter()
            .filter(|id| !self.native_ids.contains(*id))
            .cloned()
            .chain(
                self.native_ids
                    .iter()
                    .filter(|id| !disabled.contains(*id))
                    .cloned(),
            )
            .collect()
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use crate::game::mods::catalog;
    #[test]
    fn a_mods_live_switch_verdict_follows_its_pinned_generation() {
        let dir = std::env::temp_dir().join(format!("horizon-verdict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ruby = dir.join("ruby");
        std::fs::create_dir_all(ruby.join("assets/ruby/lang")).unwrap();
        std::fs::create_dir_all(ruby.join("data/ruby")).unwrap();
        std::fs::write(ruby.join("assets/ruby/lang/en_us.json"), "{}").unwrap();
        std::fs::write(
            ruby.join("horizon-mod.toml"),
            r#"id = "ruby"
name = "Ruby"
version = "0.1.0"
api = "0.2"
side = "both"
capabilities = ["registries"]
"#,
        )
        .unwrap();
        let read =
            || catalog::read(&dir, std::slice::from_ref(&ruby), &Default::default()).unwrap();
        let original = read();
        let original_entry = original.entries["ruby"].clone();
        assert!(original_entry.live);
        let enabled = std::collections::BTreeSet::from(["ruby".into()]);
        let running = Selection::wanted(&enabled, &original);
        let unchanged = read();
        assert_eq!(unchanged.entries["ruby"].revision, original_entry.revision);
        assert!(
            catalog::switch_plan(&running, &enabled, &unchanged, true)
                .waiting
                .is_empty()
        );
        let worldgen = ruby.join("data/ruby/worldgen/configured_feature");
        std::fs::create_dir_all(&worldgen).unwrap();
        std::fs::write(worldgen.join("ore.json"), "{}").unwrap();
        let edited = read();
        assert!(!edited.entries["ruby"].live);
        assert_ne!(edited.entries["ruby"].revision, original_entry.revision);
        assert!(
            original_entry.live,
            "the running snapshot keeps its original classification"
        );
        assert!(
            !original_entry
                .source
                .join("data/ruby/worldgen/configured_feature/ore.json")
                .exists()
        );
        let plan = catalog::switch_plan(&running, &enabled, &edited, true);
        assert_eq!(plan.waiting, enabled);
        assert_eq!(plan.next, running, "worldgen changes wait for restart");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pinned_mods_keep_their_order_and_their_own_errors() {
        let dir = std::env::temp_dir().join(format!("horizon-read-at-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths: Vec<PathBuf> = (0..12).map(|i| dir.join(format!("m{i}"))).collect();
        for (i, path) in paths.iter().enumerate() {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(
                path.join("horizon-mod.toml"),
                format!(
                    "id = \"m{i}\"\nname = \"M\"\nversion = \"0.1.0\"\napi = \"0.2\"\n\
                     side = \"both\"\ncapabilities = [\"registries\"]\n"
                ),
            )
            .unwrap();
            // Mod i has i + 1 language files; mod 5 also writes into another mod's namespace.
            let lang = path.join(format!("assets/m{i}/lang"));
            std::fs::create_dir_all(&lang).unwrap();
            for n in 0..=i {
                std::fs::write(lang.join(format!("l{n}.json")), "{}").unwrap();
            }
            if i == 5 {
                std::fs::create_dir_all(path.join("assets/m0/lang")).unwrap();
                std::fs::write(path.join("assets/m0/lang/x.json"), "{}").unwrap();
            }
        }
        let catalog = catalog::read_at(&dir, &paths, &Default::default(), |_| {}).unwrap();
        let expected: Vec<_> = (0..12)
            .filter(|i| *i != 5)
            .map(|i| format!("m{i}"))
            .collect();
        assert_eq!(catalog.order, expected);
        assert!(catalog.invalid.contains("m5"));
        for (i, path) in paths.iter().enumerate().filter(|(i, _)| *i != 5) {
            let entry = &catalog.entries[&format!("m{i}")];
            assert_ne!(&entry.source, path);
            let package = horizon_mod_package::Package::read(&entry.source).unwrap();
            let content =
                horizon_mod_package::Content::read(&entry.source, &package.manifest).unwrap();
            assert_eq!(content.files.len(), i + 1);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
