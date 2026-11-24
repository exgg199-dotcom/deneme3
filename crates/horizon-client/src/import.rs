//! Game files travel between a computer and a phone as one ZIP archive.
//!
//! A prepared tree is around 589 MB across five thousand files (`horizon-prepare`), a phone's app
//! bundle is read-only at runtime and CI refuses to put assets in a bundle at all, and iOS's Files
//! app neither shows nor lets you create a dotfolder -- so neither "copy the folder in" nor "ship it
//! inside the `.ipa`" works. What does work is one archive that the game unpacks when it arrives,
//! through either door: dropped into `Documents` over a cable, or posted to the phone over Wi-Fi.
//!
//! The archive is understood by what sits at its root, which is also what a person gets when they
//! compress a folder themselves: `horizon-resources.json` is the prepared game, `level.dat` is a
//! world, `pack.mcmeta` is a resource pack. [`pack`] writes exactly that shape, so the two ends
//! cannot drift apart, and [`Pack`] refuses an archive that is none of those before it writes a
//! single file.

pub mod download;
pub mod http;
pub mod screen;

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak},
};
use zip::{CompressionMethod, ZipArchive, write::SimpleFileOptions, write::ZipWriter};

use crate::Result;

/// The largest archive accepted, the most it may unpack to, and the most files it may hold. All
/// three sit far above a real prepared tree (~589 MB, ~10k files), so they are not a guess about
/// what a player sends: they are what stops one request from a stranger on the same Wi-Fi filling
/// the phone's storage or opening a million entries.
const MAX_PACKED: u64 = 2 << 30;
const MAX_UNPACKED: u64 = 4 << 30;
const MAX_ENTRIES: usize = 50_000;
const WORLD_IMPORT_STAGING: &str = ".horizon-world-importing";
const PACK_IMPORT_STAGING: &str = ".horizon-pack-importing";

#[derive(Debug, Default)]
struct HomeGate {
    busy: Mutex<bool>,
    ready: Condvar,
}

#[derive(Debug)]
struct HomeTransaction {
    gate: Arc<HomeGate>,
}

impl Drop for HomeTransaction {
    fn drop(&mut self) {
        *self
            .gate
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = false;
        self.gate.ready.notify_one();
    }
}

/// Owned access to one resource generation. Cloning extends the same
/// transaction across worker tails or handoff objects; only dropping its last
/// clone releases the home. A clone does not acquire another transaction.
#[derive(Clone, Debug)]
pub struct HomeAccess {
    _transaction: Arc<HomeTransaction>,
}

/// Acquire access to one existing game home. This waits on the calling thread:
/// callers must acquire on a worker, not the UI thread. The owned token can then
/// be moved or cloned to other threads without keeping a borrowed mutex guard.
///
/// Canonical aliases share a gate; different homes remain independent. This
/// coordinates only this process. It is not reentrant: while holding a token,
/// clone it instead of calling `acquire_home_access`, `with_home_access`,
/// `install`, or `recover` for that home again.
pub fn acquire_home_access(home: &Path) -> Result<HomeAccess> {
    type HomeGates = Mutex<BTreeMap<PathBuf, Weak<HomeGate>>>;
    static GATES: OnceLock<HomeGates> = OnceLock::new();
    let home = fs::canonicalize(home)?;
    let gate = {
        let mut gates = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Active transactions and their waiters retain a home. Do not retain
        // every home that was ever used after its operations have all ended.
        gates.retain(|_, gate| gate.strong_count() != 0);
        match gates.get(&home).and_then(Weak::upgrade) {
            Some(gate) => gate,
            None => {
                let gate = Arc::new(HomeGate::default());
                gates.insert(home, Arc::downgrade(&gate));
                gate
            }
        }
    };
    let mut busy = gate.busy.lock().unwrap_or_else(PoisonError::into_inner);
    while *busy {
        busy = gate
            .ready
            .wait(busy)
            .unwrap_or_else(PoisonError::into_inner);
    }
    *busy = true;
    drop(busy);
    Ok(HomeAccess {
        _transaction: Arc::new(HomeTransaction { gate }),
    })
}

/// Run a synchronous operation while holding the home's transaction. Detached
/// workers must instead acquire and retain `HomeAccess` until all their reads
/// and any result handoff have finished.
pub fn with_home_access<T>(home: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _access = acquire_home_access(home)?;
    operation()
}

/// What an archive holds, named for the file that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The prepared tree: `horizon-resources.json` and everything `horizon-prepare` writes beside it.
    Resources,
    /// One world folder: `level.dat` or its recovery copy, and the saved chunks.
    World,
    /// One resource pack, which stays an archive rather than being unpacked.
    ResourcePack,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Resources, Kind::World, Kind::ResourcePack];

    /// The entry that identifies this kind, wherever the archive puts it.
    pub fn marker(self) -> &'static str {
        match self {
            Kind::Resources => "horizon-resources.json",
            Kind::World => "level.dat",
            Kind::ResourcePack => "pack.mcmeta",
        }
    }

    fn markers(self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.marker()).chain((self == Kind::World).then_some("level.dat_old"))
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Resources => "game files",
            Kind::World => "world",
            Kind::ResourcePack => "resource pack",
        }
    }

    /// The folder under the game home that holds this kind; on a phone, `Documents/` and this.
    pub fn folder(self) -> &'static str {
        match self {
            Kind::Resources => "resources",
            Kind::World => "saves",
            Kind::ResourcePack => "resourcepacks",
        }
    }
}

/// Where an import ended up, for the screen and the log to describe.
#[derive(Debug, Clone)]
pub struct Installed {
    pub kind: Kind,
    pub path: PathBuf,
    pub files: usize,
    pub bytes: u64,
}

/// Where the prepared files are, of the two places they can be: the folder beside the executable,
/// and the folder an import unpacked into the sandbox's home (`Documents/resources`, the one the
/// Files app can reach). Prefer the bundled files unless their prepared tables fail the startup
/// check and the imported copy passes. `None` when neither holds `horizon-resources.json`, which
/// leaves the caller's `.local/horizon-client/26.3` in charge.
///
/// A phone needs the second stop because it has no other door: the bundle is read while the game
/// runs, so the game cannot put its own assets where the first stop looks, and `.local/...` is a
/// dotfolder iOS neither shows nor lets a player create. A desktop is not asked (`home` is `None`)
/// because a `resources` folder under its working directory means something else there.
pub fn resources_root(sibling: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let has = |dir: &Path| dir.join(Kind::Resources.marker()).is_file();
    let imported = home
        .map(|home| home.join(Kind::Resources.folder()))
        .filter(|dir| has(dir));
    match (has(sibling), imported) {
        (true, Some(imported))
            if imported != sibling
                && damaged_prepared(sibling).is_some()
                && damaged_prepared(&imported).is_none() =>
        {
            Some(imported)
        }
        (true, _) => Some(sibling.to_owned()),
        (false, imported) => imported,
    }
}

// These files are read unconditionally by Assets, item defaults and gameplay tables.
const REQUIRED_PREPARED_TABLES: &[&str] = &[
    "facts.json",
    "block-tags.json",
    "block-behaviours.json",
    "gameplay-facts.json",
    "items.json",
];

/// Which top-level prepared file a killed preparation or an interrupted copy
/// left unusable. The loader parses these before it can start; naming one
/// beats serde's bare "expected value at line 1 column 1".
pub fn damaged_prepared(root: &Path) -> Option<String> {
    let manifest = root.join("horizon-resources.json");
    let bytes = match std::fs::read(&manifest) {
        Ok(bytes) => bytes,
        Err(error) => return Some(format!("cannot read horizon-resources.json: {error}")),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Some("horizon-resources.json is empty".into());
    }
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Some("horizon-resources.json is not valid JSON".into());
    };
    let expected = horizon_core::version::DATA_VERSION;
    match manifest["data_version"].as_i64() {
        Some(actual) if actual == i64::from(expected) => {}
        Some(actual) => {
            return Some(format!(
                "horizon-resources.json names data version {actual}; this build needs {expected}"
            ));
        }
        None => return Some("horizon-resources.json is missing its data_version".into()),
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => return Some(format!("cannot list prepared resources: {error}")),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Some(format!("cannot read a prepared-resource entry: {error}")),
        };
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Some(format!(
                    "cannot inspect {}: {error}",
                    entry.file_name().to_string_lossy()
                ));
            }
        };
        if metadata.len() == 0 {
            return Some(format!("{} is empty", entry.file_name().to_string_lossy()));
        }
    }
    for name in REQUIRED_PREPARED_TABLES {
        if !root.join(name).is_file() {
            return Some(format!("{name} is missing or is not a regular file"));
        }
    }
    None
}

/// Whether a game that found nothing to load should wait on screen instead of giving up. On a phone,
/// always: there is nothing else on the device to say what is missing, and a silent exit teaches a
/// player nothing. Anywhere else it has to be asked for (`HORIZON_IMPORT_SCREEN`), which is how the
/// screen is driven and tested on a machine with no phone in it.
pub fn waits_on_screen() -> bool {
    cfg!(any(target_os = "ios", target_os = "android"))
        || std::env::var_os("HORIZON_IMPORT_SCREEN").is_some()
}

/// The folder an import writes into: `Documents` on a phone, where the Files app and a cable both
/// reach; on Linux the game's home (`HORIZON_HOME`, else `~/.local/share/horizon`), where a start
/// downloads the game files once and finds them after; and the working directory everywhere else, and
/// in a run that asked for the screen (`HORIZON_IMPORT_SCREEN`), which points the game there.
pub fn home() -> PathBuf {
    if cfg!(target_os = "linux") && std::env::var_os("HORIZON_IMPORT_SCREEN").is_none() {
        return crate::logging::game_home();
    }
    std::env::current_dir().unwrap_or_default()
}

/// Names a desktop adds to ZIPs that could hide the common wrapper folder.
fn is_junk(name: &str) -> bool {
    let last = name.rsplit('/').next().unwrap_or(name);
    name.starts_with("__MACOSX/") || matches!(last, ".DS_Store" | "Thumbs.db" | "desktop.ini")
}

/// Drop the session lock at the root of a save (including when an archive
/// carries it beside a wrapping directory). A nested file can be mod data.
fn archive_session_lock(name: &str, prefix: &str) -> bool {
    name.strip_suffix("session.lock").is_some_and(|parent| {
        parent.is_empty() || (parent.ends_with('/') && prefix.starts_with(parent))
    })
}

/// The name an archive carries for itself, with anything that would turn it into a path or a
/// terminal escape removed. It arrives from a stranger's file: it is a label, not an instruction.
fn sanitize(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        .collect();
    let trimmed = kept.trim_matches('.').trim();
    if trimmed.is_empty() {
        "imported".to_owned()
    } else {
        // Leave space under 255 bytes for a collision suffix and .zip.
        let end = trimmed
            .char_indices()
            .nth(64)
            .map_or(trimmed.len(), |(i, _)| i);
        trimmed[..trimmed.floor_char_boundary(end.min(238))].to_owned()
    }
}

/// Discard current-directory components before looking for an archive's root.
fn normalized_zip_name(name: &str) -> String {
    let name = name.replace('\\', "/");
    let mut normalized = String::new();
    if name.starts_with('/') {
        normalized.push('/');
    }
    for part in name
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        if !normalized.is_empty() && !normalized.ends_with('/') {
            normalized.push('/');
        }
        normalized.push_str(part);
    }
    if name.ends_with('/') && !normalized.is_empty() && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

/// Where an entry belongs inside the destination, or `None` when it belongs nowhere: no `..`, no
/// absolute path, no drive letter. A bad name fails the whole archive instead of quietly dropping
/// the entry, so a player hears that the archive is strange rather than playing a half-imported
/// world.
fn inside(name: &str, prefix: &str) -> Option<PathBuf> {
    if name.starts_with('/')
        || name.contains([':', '\0'])
        || name.split('/').any(|part| part == "..")
    {
        return None;
    }
    let rest = if prefix.is_empty() {
        name
    } else {
        name.strip_prefix(prefix)?
    };
    let mut out = PathBuf::new();
    for part in rest.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            _ => out.push(part),
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// The one folder every entry shares one level below `prefix`, if there is one: what compressing a
/// folder produces, as opposed to compressing its contents.
fn common_folder(names: &[String], prefix: &str) -> Option<String> {
    let mut entries = names
        .iter()
        .filter(|name| name.strip_prefix(prefix) != Some("session.lock"));
    let (folder, _) = entries.next()?.strip_prefix(prefix)?.split_once('/')?;
    if folder.is_empty() || folder == ".." {
        return None;
    }
    entries
        .all(|name| {
            name.strip_prefix(prefix)
                .and_then(|name| name.split_once('/'))
                .is_some_and(|(candidate, _)| candidate == folder)
        })
        .then(|| folder.to_owned())
}

/// Does one of the kinds' markers sit directly under `prefix`?
fn has_marker(names: &[String], prefix: &str) -> Option<Kind> {
    Kind::ALL.iter().copied().find(|kind| {
        names.iter().any(|name| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| kind.markers().any(|marker| marker == rest))
        })
    })
}

/// One archive, opened and understood.
pub struct Pack {
    archive: ZipArchive<File>,
    path: PathBuf,
    /// The folder or file name the game gives what arrived: the folder the markers live under, else
    /// the archive's own stem.
    item: String,
    /// Every entry above the root the game reads, including the trailing `/`: `""`, `"Horizon/"`, or
    /// `"horizon-client/26.3/"` for somebody who compressed one folder too many.
    prefix: String,
    /// Even a pack with no wrapper needs rewriting if its entry names changed.
    normalized_names: bool,
    pub kind: Kind,
}

impl Pack {
    /// Require prepared game files before automatic setup can mutate the home.
    /// General manual import still accepts worlds and resource packs.
    pub fn require_game_files(self) -> Result<Self> {
        if self.kind != Kind::Resources {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                crate::lang::get_or(
                    "horizon.import.game_files_only",
                    "This archive does not contain prepared game files.",
                ),
            )
            .into());
        }
        Ok(self)
    }

    /// Look inside an archive and say what it is, writing nothing. Only the central directory is
    /// read, so this is cheap even for a prepared tree.
    pub fn open(path: &Path) -> Result<Self> {
        let input = File::open(path)?;
        if input.metadata()?.len() > MAX_PACKED {
            return Err("the ZIP exceeds Horizon's 2 GiB import limit".into());
        }
        let archive = ZipArchive::new(input)?;
        let mut names = Vec::new();
        let mut declared_bytes = 0u128;
        let mut normalized_names = false;
        for index in 0..archive.len() {
            let entry = archive.by_index_data(index)?;
            let original = entry.name()?;
            let name = normalized_zip_name(&original);
            if name.is_empty() || name.ends_with('/') || is_junk(&name) {
                continue;
            }
            normalized_names |= name != original.as_ref();
            if names.len() == MAX_ENTRIES {
                return Err("the archive holds more files than this will unpack".into());
            }
            declared_bytes += u128::from(entry.size());
            names.push(name);
        }
        if names.is_empty() {
            return Err("the archive holds nothing".into());
        }
        names.sort();
        // Descend through at most two shared folders looking for the marker, because compressing
        // `.local/horizon-client` rather than `26.3` is a mistake a careful person still makes.
        let mut prefix = String::new();
        let mut kind = has_marker(&names, "");
        for _ in 0..2 {
            if kind.is_some() {
                break;
            }
            let Some(folder) = common_folder(&names, &prefix) else {
                break;
            };
            prefix.push_str(&folder);
            prefix.push('/');
            kind = has_marker(&names, &prefix);
        }
        let kind = kind.ok_or_else(|| {
            format!(
                "{} is not Horizon's game files: it holds none of {}",
                path.display(),
                Kind::ALL
                    .iter()
                    .flat_map(|kind| kind.markers())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        // Packs stay compressed. For files we do extract, reject an oversized
        // tree before staging it and still enforce the actual byte limit while reading.
        if kind != Kind::ResourcePack && declared_bytes > u128::from(MAX_UNPACKED) {
            return Err("the ZIP exceeds Horizon's 4 GiB unpacked import limit".into());
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let item = sanitize(
            prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|last| !last.is_empty())
                .unwrap_or(&stem),
        );
        Ok(Self {
            archive,
            path: path.to_owned(),
            item,
            prefix,
            normalized_names,
            kind,
        })
    }

    /// Read every entry out into `into`, which is created here. Anything that goes wrong removes
    /// `into` again: the game must never see a half tree.
    fn unpack(&mut self, into: &Path, wanted: &impl Fn() -> bool) -> Result<(usize, u64)> {
        // If removing interrupted staging failed, never merge the incoming archive with it.
        fs::create_dir(into)?;
        let result = self.unpack_entries(into, wanted);
        if result.is_err() {
            // CRC, decompression and filesystem failures also leave partial
            // output. A world marker may already be visible in that output.
            let _ = fs::remove_dir_all(into);
        }
        result
    }

    fn unpack_entries(&mut self, into: &Path, wanted: &impl Fn() -> bool) -> Result<(usize, u64)> {
        let mut files = 0;
        let mut written = 0u64;
        for index in 0..self.archive.len() {
            if !wanted() {
                return Err("the import was cancelled".into());
            }
            let file = self.archive.by_index(index).map_err(io::Error::from)?;
            let name = normalized_zip_name(&file.name()?);
            let failure = |message: String| Err::<(usize, u64), _>(message.into());
            // Not `ZipArchive::extract`, which would create these: an archive from a stranger must
            // not be able to point a later write somewhere else in the container.
            if file.is_symlink() {
                return failure(format!("the archive holds a symbolic link: {name}"));
            }
            if name.is_empty()
                || name.ends_with('/')
                || is_junk(&name)
                || archive_session_lock(&name, &self.prefix)
            {
                continue;
            }
            let Some(relative) = inside(&name, &self.prefix) else {
                return failure(format!("the archive holds a path that escapes it: {name}"));
            };
            let target = into.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&target)?;
            // The limit is on bytes actually written, not on what the archive claims: a header can
            // say anything at all. The extra byte is what tells "exactly the limit" from "past it"
            // without a second pass.
            let mut input = file.take(MAX_UNPACKED - written + 1);
            const COPY_CHUNK: u64 = 1 << 20;
            loop {
                if !wanted() {
                    return Err("the import was cancelled".into());
                }
                let copied = io::copy(&mut input.by_ref().take(COPY_CHUNK), &mut out)?;
                written += copied;
                if written > MAX_UNPACKED {
                    return failure("the archive unpacks to more than this will hold".to_owned());
                }
                if copied < COPY_CHUNK {
                    break;
                }
            }
            files += 1;
        }
        Ok((files, written))
    }

    /// A folder-wrapped resource pack must have its resource root promoted to
    /// the ZIP root before the normal pack repository can see it. Copy compressed
    /// entries directly: rewriting names does not require inflating HD assets.
    fn normalize_resource_pack(&mut self, preferred: &Path) -> Result<(PathBuf, u64)> {
        let mut entries = BTreeMap::new();
        for index in 0..self.archive.len() {
            let file = self.archive.by_index(index)?;
            let name = normalized_zip_name(&file.name()?);
            if file.is_symlink() {
                return Err(format!("the archive holds a symbolic link: {name}").into());
            }
            if name.is_empty()
                || name.ends_with('/')
                || is_junk(&name)
                || archive_session_lock(&name, &self.prefix)
            {
                continue;
            }
            let relative = inside(&name, &self.prefix)
                .ok_or_else(|| format!("the archive holds a path that escapes it: {name}"))?;
            // The unpacked kinds also give the final entry for a path priority.
            // Deduplicate after normalization so the ZIP writer need not emit
            // ambiguous duplicate names, including `a/./b` aliases.
            entries.insert(relative.to_string_lossy().replace('\\', "/"), index);
        }
        if !entries.contains_key(Kind::ResourcePack.marker()) {
            return Err("the archive has no resource pack metadata at its resource root".into());
        }
        let published = publish_pack(preferred, |file| {
            let mut output = ZipWriter::new(file);
            for (name, index) in entries {
                output.raw_copy_file_rename(self.archive.by_index(index)?, name)?;
            }
            Ok(output.finish()?)
        })?;
        consume_installed_archive(&self.path);
        Ok(published)
    }

    /// Put the archive where the game reads it, under `home` (`Documents` on a phone), and consume
    /// it: a phone does not have 589 MB to spend twice, and what arrives is a copy of something that
    /// is still on the computer that sent it.
    pub fn install(self, home: &Path) -> Result<Installed> {
        self.install_guarded(home, || true)
    }

    fn install_guarded(self, home: &Path, wanted: impl Fn() -> bool) -> Result<Installed> {
        fs::create_dir_all(home)?;
        let access = acquire_home_access(home)?;
        self.install_with_access(home, &access, wanted)
    }

    /// Keep publication in the caller's existing home transaction.
    fn install_with_access(
        mut self,
        home: &Path,
        _access: &HomeAccess,
        wanted: impl Fn() -> bool,
    ) -> Result<Installed> {
        if !wanted() {
            return Err("the import was cancelled".into());
        }
        let archive = self.path.clone();
        // A previous import may have consumed this path while we waited for the home.
        let archive_bytes = fs::metadata(&archive)?.len();
        let folder = Path::new(self.kind.folder());
        let (path, files, bytes) = match self.kind {
            // A pack stays an archive: the game reads zips straight out of `resourcepacks`
            // (`packs::discover`), so unpacking one would only lose what vanilla players expect.
            Kind::ResourcePack => {
                let preferred = home.join(folder).join(format!("{}.zip", self.item));
                let (path, bytes) = if self.prefix.is_empty() && !self.normalized_names {
                    move_pack(&archive, &preferred, archive_bytes)?
                } else {
                    self.normalize_resource_pack(&preferred)?
                };
                (path, 1, bytes)
            }
            // A world is never written over: what would be overwritten is the player's own.
            Kind::World => {
                let saves = home.join(folder);
                fs::create_dir_all(&saves)?;
                clear_world_staging(&saves)?;
                let staging = saves.join(WORLD_IMPORT_STAGING);
                fs::create_dir(&staging)?;
                // The world list only checks the immediate children of saves. Put
                // the partial tree one level deeper, on the destination volume.
                let scratch = staging.join("world");
                let published = (|| -> Result<_> {
                    let (files, bytes) = self.unpack(&scratch, &wanted)?;
                    if !wanted() {
                        return Err("the import was cancelled".into());
                    }
                    let dest = publish_without_replacing(
                        &scratch,
                        &saves.join(&self.item),
                        rename_world_without_replacing,
                    )?;
                    Ok((dest, files, bytes))
                })();
                let _ = clear_world_staging(&saves);
                let installed = published?;
                consume_installed_archive(&archive);
                installed
            }
            Kind::Resources => {
                let scratch = home.join("resources.unpacking");
                clear_resource_slot(&scratch)?;
                let (files, bytes) = self.unpack(&scratch, &wanted)?;
                let staged = home.join("resources.staged");
                let staging = (|| -> Result<()> {
                    if let Some(problem) = damaged_prepared(&scratch) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("Horizon's prepared game files are damaged: {problem}"),
                        )
                        .into());
                    }
                    if !wanted() {
                        return Err("the import was cancelled".into());
                    }
                    clear_resource_slot(&staged)?;
                    // The marker may be the first ZIP entry; only the rename proves completion.
                    fs::rename(&scratch, &staged)?;
                    Ok(())
                })();
                if staging.is_err() {
                    let _ = fs::remove_dir_all(&scratch);
                }
                staging?;
                let dest = home.join(folder);
                publish(&staged, &dest, |from, to| fs::rename(from, to))?;
                consume_installed_archive(&archive);
                (dest, files, bytes)
            }
        };
        Ok(Installed {
            kind: self.kind,
            path,
            files,
            bytes,
        })
    }

    /// The archive itself, for a caller that only wanted to name it.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Remove the one private extraction tree left by a failed or interrupted world import.
/// Don't follow a link or remove any other files beside it.
fn clear_world_staging(saves: &Path) -> io::Result<()> {
    let staging = saves.join(WORLD_IMPORT_STAGING);
    match fs::symlink_metadata(&staging) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(io::Error::other("world import staging is not a directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    match fs::remove_dir_all(staging.join("world")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::remove_dir(staging)
}

// Preserve an unexpected file placed in a resource directory slot instead of deleting it.
fn clear_resource_slot(slot: &Path) -> io::Result<()> {
    match fs::symlink_metadata(slot) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(slot),
        Ok(_) => fs::rename(slot, unique(&slot.with_extension("obstructed"))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Park the current tree before publishing so recovery can restore it after an interrupted installation.
fn publish(
    scratch: &Path,
    dest: &Path,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    let previous = dest.with_file_name(format!(
        "{}.previous",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    if dest.exists() {
        if previous.is_dir()
            && fs::symlink_metadata(dest)?.is_dir()
            && damaged_prepared(dest).is_some()
            && damaged_prepared(&previous).is_none()
        {
            // A retry must keep the usable backup until the new tree is in place.
            fs::remove_dir_all(dest)?;
        } else {
            clear_resource_slot(&previous)?;
            rename(dest, &previous)?;
        }
    }
    if let Err(error) = rename(scratch, dest) {
        // A prior interrupted attempt may already have moved the old tree
        // aside. Keep it until the new tree has actually taken its place.
        if previous.is_dir() && !dest.exists() {
            if let Err(rollback) = rename(&previous, dest) {
                return Err(format!(
                    "could not install game files: {error}; could not restore the previous game files: {rollback}"
                )
                .into());
            }
            horizon_core::prepared_files::invalidate(dest);
        }
        return Err(error.into());
    }
    horizon_core::prepared_files::invalidate(dest);
    let _ = clear_resource_slot(&previous);
    Ok(())
}

/// The installed marker fingerprint, using the same fields as the iOS launcher.
fn resource_marker(home: &Path) -> io::Result<String> {
    let metadata = match fs::metadata(
        home.join(Kind::Resources.folder())
            .join(Kind::Resources.marker()),
    ) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok("missing".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("missing".into()),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "{}:{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(format!("{:?}:{}", metadata.modified()?, metadata.len()))
    }
}

/// Check for a newer install after acquiring the same home lock used to publish it.
fn game_files_are_current(home: &Path, expected_marker: &str) -> io::Result<bool> {
    let current = resource_marker(home)?;
    Ok(current == "missing" || current == expected_marker)
}

/// Install a launcher download only if another setup did not finish meanwhile.
pub fn install_game_files_if_current(
    archive: &Path,
    home: &Path,
    expected_marker: &str,
) -> Result<Option<Installed>> {
    fs::create_dir_all(home)?;
    let access = acquire_home_access(home)?;
    if !game_files_are_current(home, expected_marker)? {
        return Ok(None);
    }
    Ok(Some(
        Pack::open(archive)?
            .require_game_files()?
            .install_with_access(home, &access, || true)?,
    ))
}

/// Finish an install that a crash or a flat battery left half done, and report what it put back.
/// Called at startup, before the game looks for its resources.
pub fn recover(home: &Path) -> Result<Option<PathBuf>> {
    if !home.exists() {
        return Ok(None);
    }
    with_home_access(home, || {
        // A world extraction has no publish-ready state before its final rename.
        if let Err(error) = clear_world_staging(&home.join(Kind::World.folder())) {
            log::warn!("Horizon: could not clear unfinished world import: {error}");
        }
        if let Err(error) = clear_pack_staging(&home.join(Kind::ResourcePack.folder())) {
            log::warn!("Horizon: could not clear unfinished resource pack import: {error}");
        }
        let scratch = home.join("resources.unpacking");
        let staged = home.join("resources.staged");
        let dest = home.join(Kind::Resources.folder());
        let previous = home.join("resources.previous");
        let usable = |root: &Path| root.is_dir() && damaged_prepared(root).is_none();
        let installed_ok = (staged.is_dir() || previous.is_dir()) && usable(&dest);
        let replacement = if installed_ok {
            None
        } else {
            [&staged, &previous].into_iter().find(|root| usable(root))
        };
        if let Some(replacement) = replacement {
            clear_resource_slot(&dest)?;
            fs::rename(replacement, &dest)?;
            horizon_core::prepared_files::invalidate(&dest);
        }
        // Never promote a partial extraction. Keep an older backup if there is
        // no usable installation for this build, so it is available for recovery.
        let _ = clear_resource_slot(&scratch);
        let _ = clear_resource_slot(&staged);
        if installed_ok || replacement.is_some() {
            let _ = fs::remove_dir_all(&previous);
        }
        Ok(replacement.is_some().then_some(dest))
    })
}

/// A competing creator can take a name after `unique` checked it. Retry the
/// name selection without changing the occupied path or the staged import.
fn publish_without_replacing(
    staged: &Path,
    preferred: &Path,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    loop {
        let dest = unique(preferred);
        match rename(staged, &dest) {
            Ok(()) => return Ok(dest),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn rename_world_without_replacing(staged: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))]
    {
        use rustix::{
            fs::{CWD, RenameFlags, renameat_with},
            io::Errno,
        };
        match renameat_with(CWD, staged, CWD, dest, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(()),
            Err(Errno::NOTSUP | Errno::NOSYS | Errno::INVAL) => {}
            Err(Errno::EXIST | Errno::NOTEMPTY) => return Err(io::ErrorKind::AlreadyExists.into()),
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(windows)]
    {
        // On Windows a directory rename cannot replace an existing directory.
        fs::rename(staged, dest).map_err(|error| {
            if fs::symlink_metadata(dest).is_ok() {
                io::ErrorKind::AlreadyExists.into()
            } else {
                error
            }
        })
    }
    #[cfg(not(windows))]
    rename_into_reserved_world_folder(staged, dest)
}

#[cfg(not(windows))]
fn rename_into_reserved_world_folder(staged: &Path, dest: &Path) -> io::Result<()> {
    // Filesystems without exclusive rename can still reserve the name with
    // an exclusive mkdir. The empty reservation is not a listable world.
    fs::create_dir(dest)?;
    if let Err(error) = fs::rename(staged, dest) {
        let _ = fs::remove_dir(dest);
        return Err(error);
    }
    Ok(())
}

/// `path`, or `path (2)` if it is taken, then `path (3)` and so on.
fn unique(path: &Path) -> PathBuf {
    let taken = |path: &Path| path.exists() || path.is_symlink();
    if !taken(path) {
        return path.to_owned();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path.extension().and_then(|e| e.to_str());
    let mut index = 2;
    loop {
        let name = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = path.with_file_name(name);
        if !taken(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn park_archive_with_access(archive: &Path, _access: &HomeAccess) -> Result<PathBuf> {
    let parked = unique(&archive.with_extension("zip.failed"));
    fs::rename(archive, &parked)?;
    Ok(parked)
}

fn rename_pack_without_replacing(from: &Path, to: &Path) -> io::Result<()> {
    // A failed attempt must leave the source available for another name or a copy.
    let mut source = tempfile::TempPath::try_from_path(from)?;
    source.disable_cleanup(true);
    match source.persist_noclobber(to) {
        Ok(()) => return Ok(()),
        Err(error)
            if error.error.kind() == io::ErrorKind::Unsupported
                || (cfg!(target_vendor = "apple") && error.error.raw_os_error() == Some(45)) => {}
        Err(error) => return Err(error.error),
    }
    // Some removable filesystems lack exclusive rename and hard links.
    // Reserve the name without exposing partially copied archive bytes.
    let reservation = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    drop(reservation);
    if let Err(error) = fs::rename(from, to) {
        let _ = fs::remove_file(to);
        return Err(error);
    }
    Ok(())
}

/// Move the pack when possible and stage a copy when it comes from another volume.
fn move_pack(from: &Path, preferred: &Path, archive_bytes: u64) -> Result<(PathBuf, u64)> {
    if let Some(parent) = preferred.parent() {
        fs::create_dir_all(parent)?;
    }
    match publish_without_replacing(from, preferred, rename_pack_without_replacing) {
        Ok(path) => return Ok((path, archive_bytes)),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {}
        Err(error) => return Err(error.into()),
    }
    let mut source = File::open(from)?;
    let published = publish_pack(preferred, |mut output| {
        output.set_permissions(source.metadata()?.permissions())?;
        io::copy(&mut source, &mut output)?;
        Ok(output)
    })?;
    drop(source);
    consume_installed_archive(from);
    Ok(published)
}

/// The installation has already been published. A leftover source must
/// not turn success into an error or invite the startup scanner to re-import it.
fn consume_installed_archive(archive: &Path) {
    let error = match fs::remove_file(archive) {
        Ok(()) => return,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => error,
    };
    if fs::symlink_metadata(archive).is_ok_and(|meta| meta.is_dir()) {
        log::warn!(
            "Horizon: installed {} but the source path now names a directory",
            archive.display()
        );
        return;
    }
    let leftover = unique(&archive.with_extension("zip.imported"));
    match fs::rename(archive, &leftover) {
        Ok(()) => log::warn!(
            "Horizon: installed {} but could not delete the source: {error}; left it as {}",
            archive.display(),
            leftover.display()
        ),
        Err(rename_error) if rename_error.kind() == io::ErrorKind::NotFound => {}
        Err(rename_error) => log::warn!(
            "Horizon: installed {} but could not clean up the source: {error}; could not rename it: {rename_error}",
            archive.display()
        ),
    }
}

fn clear_pack_staging(folder: &Path) -> io::Result<()> {
    match fs::remove_file(folder.join(PACK_IMPORT_STAGING)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// The home lock serializes these writes. The staging file lives on the pack's
/// volume but has no ZIP extension, so a partial archive is never discoverable.
fn publish_pack(
    preferred: &Path,
    write: impl FnOnce(File) -> Result<File>,
) -> Result<(PathBuf, u64)> {
    let folder = preferred
        .parent()
        .ok_or("the resource pack needs a folder")?;
    fs::create_dir_all(folder)?;
    clear_pack_staging(folder)?;
    let temporary = folder.join(PACK_IMPORT_STAGING);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let published = (|| -> Result<_> {
        let file = write(file)?;
        file.sync_all()?;
        let bytes = file.metadata()?.len();
        drop(file);
        let path = publish_without_replacing(&temporary, preferred, rename_pack_without_replacing)?;
        Ok((path, bytes))
    })();
    if published.is_err() {
        let _ = clear_pack_staging(folder);
    }
    published
}

/// Say what an archive is without touching what it contains.
pub fn classify(path: &Path) -> Result<(Kind, String)> {
    let pack = Pack::open(path)?;
    Ok((pack.kind, pack.item))
}

/// Write a folder as an archive the phone accepts. The folder's own name becomes the single
/// top-level directory, which is the shape a desktop's Compress produces and what [`Pack::open`]
/// reads back, so an archive made here and one made by hand mean the same thing.
pub fn pack(folder: &Path, out: &Path, mut progress: impl FnMut(usize, u64)) -> Result<Installed> {
    let kind = Kind::ALL
        .iter()
        .copied()
        .find(|kind| kind.markers().any(|marker| folder.join(marker).is_file()))
        .ok_or_else(|| {
            format!(
                "{} holds none of the three kinds: no {} at its root",
                folder.display(),
                Kind::ALL
                    .iter()
                    .flat_map(|kind| kind.markers())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let top = folder
        .file_name()
        .ok_or("the folder has no name to give the archive")?;
    let top = zip_path(Path::new(top))?;
    let root = fs::canonicalize(folder)?;
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let destination =
        fs::canonicalize(parent)?.join(out.file_name().ok_or("the archive needs a file name")?);
    // Canonicalize an existing regular output to retain its actual spelling on
    // case-insensitive volumes. Do not follow an output symlink: replacing the
    // link must not replace its target.
    let destination = if fs::symlink_metadata(&destination).is_ok_and(|m| m.is_file()) {
        fs::canonicalize(&destination)?
    } else {
        destination
    };
    if kind
        .markers()
        .any(|marker| destination == root.join(marker))
    {
        return Err("the archive cannot replace the source folder's metadata".into());
    }
    // A nested output may replace an earlier export of this kind, but never
    // an input payload (region data, textures, or a different kind of archive).
    if destination.starts_with(&root)
        && fs::symlink_metadata(&destination).is_ok()
        && !Pack::open(&destination).is_ok_and(|archive| archive.kind == kind)
    {
        return Err("the archive cannot overwrite a file in the source folder".into());
    }
    let temporary = unique(&destination.with_extension("zip.exporting"));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let written = (|| -> Result<(usize, u64)> {
        let mut zip = ZipWriter::new(file);
        let excluded = [destination.as_path(), temporary.as_path()];
        let counts = write_tree(
            &mut zip,
            &top,
            &root,
            &root,
            options,
            &excluded,
            &mut progress,
        )?;
        let file = zip.finish()?;
        file.sync_all()?;
        drop(file);
        // The previous archive stays intact until its replacement is complete.
        fs::rename(&temporary, &destination)?;
        Ok(counts)
    })();
    match written {
        Ok((files, bytes)) => Ok(Installed {
            kind,
            path: out.to_owned(),
            files,
            bytes,
        }),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Encode filesystem path components as a ZIP name without changing their spelling.
pub(crate) fn zip_path(path: &Path) -> io::Result<String> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Cannot preserve this path in a Horizon ZIP: {}", path.display()),
        )
    };
    let mut name = String::new();
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(invalid());
        };
        let component = component.to_str().ok_or_else(&invalid)?;
        if component.contains(['\\', ':', '\0']) {
            return Err(invalid());
        }
        if !name.is_empty() {
            name.push('/');
        }
        name.push_str(component);
    }
    if name.is_empty() {
        return Err(invalid());
    }
    Ok(name)
}

/// Add a file of known length, with room for deflate expansion at the ZIP32 limit.
pub(crate) fn write_zip_entry<W: io::Write + io::Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    source: impl Read,
    size: u64,
    options: SimpleFileOptions,
) -> Result<u64> {
    zip.start_file(name, options.large_file(size >= 1 << 31))?;
    let copied = io::copy(&mut source.take(size.saturating_add(1)), zip)?;
    if copied != size {
        return Err(format!("File changed size during archive creation: {name}").into());
    }
    Ok(copied)
}

/// Walk `dir` into `zip` under `top`. Symlinks are refused on the way out for the same reason
/// [`Pack::unpack`] refuses them on the way in, and a running world's `session.lock` is left out
/// because it means "someone is in here", not anything worth importing.
fn write_tree(
    zip: &mut ZipWriter<File>,
    top: &str,
    root: &Path,
    dir: &Path,
    options: SimpleFileOptions,
    excluded: &[&Path],
    progress: &mut impl FnMut(usize, u64),
) -> Result<(usize, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // Exporting inside the source is supported, but neither an old archive
        // nor the file currently being written belongs inside the new ZIP.
        if excluded.contains(&path.as_path()) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(format!("not packing the symbolic link {}", path.display()).into());
        }
        if kind.is_dir() {
            let (more, written) = write_tree(zip, top, root, &path, options, excluded, progress)?;
            files += more;
            bytes += written;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let relative = zip_path(path.strip_prefix(root)?)?;
        if relative == "session.lock" || is_junk(&relative) {
            continue;
        }
        let source = File::open(&path)?;
        let size = source.metadata()?.len();
        bytes += write_zip_entry(zip, &format!("{top}/{relative}"), source, size, options)?;
        files += 1;
        if files % 256 == 0 {
            progress(files, bytes);
        }
    }
    Ok((files, bytes))
}

fn is_manual_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
        && path.is_file()
        && !download::reserved_archive(path)
}

fn manual_zips_in_import_order(home: &Path) -> Vec<PathBuf> {
    let mut archives: Vec<_> = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !is_manual_zip(&path) {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((modified, path))
        })
        .collect();
    // A later resource install replaces an earlier one. Process the newest
    // ZIP last, using the path as a stable tie-breaker on coarse filesystems.
    archives.sort_unstable();
    archives.into_iter().map(|(_, path)| path).collect()
}

/// The phone's other door: archives that arrive with no network involved, dropped into `Documents`
/// by the Files app, by Finder's or iTunes' File Sharing, or by a manager someone installed for the
/// purpose. Internal download cache names are left to their owning download,
/// whose source/type checks must run first. Other zips are tried; recognized
/// ones are installed and consumed; anything that is not ours is
/// left alone with a word in the log, and anything that was ours but would not unpack is renamed so
/// the next launch does not fail in the same place all over again.
pub fn scan(home: &Path) -> Vec<Installed> {
    let mut installed = Vec::new();
    for path in manual_zips_in_import_order(home) {
        let access = match acquire_home_access(home) {
            Ok(access) => access,
            Err(error) => {
                log::warn!("Horizon: could not access the import folder: {error}");
                break;
            }
        };
        // A Wi-Fi upload can have finished installing this archive while we
        // waited for the transaction. Open only the file still at this path.
        if !is_manual_zip(&path) {
            continue;
        }
        let pack = match Pack::open(&path) {
            Ok(pack) => pack,
            // Somebody else's archive, or not an archive: not ours to touch.
            Err(error) => {
                log::info!("Horizon: leaving {} alone: {error}", path.display());
                continue;
            }
        };
        match pack.install_with_access(home, &access, || true) {
            Ok(placed) => {
                log::info!(
                    "Horizon: imported {} {} to {} ({} files, {} bytes)",
                    placed.kind.label(),
                    placed
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    placed.path.display(),
                    placed.files,
                    placed.bytes
                );
                installed.push(placed);
            }
            Err(error) => match park_archive_with_access(&path, &access) {
                Ok(parked) => log::error!(
                    "Horizon: {} would not import: {error}; leaving it as {}",
                    path.display(),
                    parked.display()
                ),
                Err(park_error) => log::error!(
                    "Horizon: {} would not import: {error}; could not park it: {park_error}",
                    path.display()
                ),
            },
        }
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("horizon-import-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A test archive, written by hand because the point of these tests is what a stranger can send.
    fn write_archive(path: &Path, entries: &[(impl AsRef<str>, impl AsRef<str>)]) {
        let mut zip = ZipWriter::new(File::create(path).unwrap());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, body) in entries {
            zip.start_file(name.as_ref(), options).unwrap();
            zip.write_all(body.as_ref().as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    pub(super) fn prepared_entries() -> Vec<(&'static str, String)> {
        std::iter::once((
            "horizon-resources.json",
            serde_json::json!({"data_version": horizon_core::version::DATA_VERSION}).to_string(),
        ))
        .chain(
            REQUIRED_PREPARED_TABLES
                .iter()
                .map(|&name| (name, "{}".into())),
        )
        .collect()
    }

    fn write_prepared_tree(root: &Path, payload: &str) -> io::Result<()> {
        fs::create_dir_all(root)?;
        for (name, body) in prepared_entries() {
            fs::write(root.join(name), body)?;
        }
        fs::write(root.join("payload.txt"), payload)
    }

    fn prepared() -> Vec<(&'static str, String)> {
        let mut entries = prepared_entries();
        entries.push(("assets/minecraft/textures/block/grass.png", "png".into()));
        entries
    }

    #[test]
    #[ignore = "daily"]
    fn oversized_world_is_rejected_before_unpacking() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("large-world.zip");
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file("World/level.dat", options).unwrap();
        zip.write_all(b"world metadata").unwrap();
        zip.start_file("World/large.bin", options.large_file(true))
            .unwrap();
        io::copy(&mut io::repeat(0).take(MAX_UNPACKED), &mut zip).unwrap();
        zip.finish().unwrap();

        let error = Pack::open(&path)
            .err()
            .expect("ZIP should be refused before unpacking");
        assert!(error.to_string().contains("4 GiB unpacked import limit"));
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[test]
    fn prepared_files_unpack_into_the_folder_the_game_reads() {
        let home = temp_home("prepared");
        let archive = home.join("horizon.zip");
        write_archive(&archive, &prepared());
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        assert_eq!(installed.kind, Kind::Resources);
        assert_eq!(installed.files, prepared().len());
        assert!(home.join("resources/horizon-resources.json").is_file());
        assert!(
            home.join("resources/assets/minecraft/textures/block/grass.png")
                .is_file()
        );
        // The archive is a copy of what the computer still has, so it does not stay.
        assert!(!archive.exists());
        assert!(!home.join("resources.unpacking").exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn automatic_import_keeps_game_files_installed_after_the_source_lookup() {
        let home = temp_home("automatic-late-install");
        let download = home.join("automatic.zip");
        let manual = home.join("manual.zip");
        let expected = resource_marker(&home).unwrap();

        write_archive(&download, &prepared());
        let mut entries = prepared_entries();
        entries.push(("manual-install.txt", "keep me".into()));
        write_archive(&manual, &entries);
        std::thread::scope(|scope| {
            let access = acquire_home_access(&home).unwrap();
            let (answer, result) = std::sync::mpsc::channel();
            let (archive, game_home, marker) = (&download, &home, &expected);
            scope.spawn(move || {
                let outcome = install_game_files_if_current(archive, game_home, marker);
                answer
                    .send(
                        outcome
                            .map(|installed| installed.is_none())
                            .map_err(|error| error.to_string()),
                    )
                    .unwrap();
            });
            assert!(matches!(
                result.recv_timeout(std::time::Duration::from_millis(50)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            Pack::open(&manual)
                .unwrap()
                .install_with_access(&home, &access, || true)
                .unwrap();
            drop(access);
            assert!(result.recv().unwrap().unwrap());
        });
        let installed = home.join("resources/manual-install.txt");
        assert_eq!(fs::read(&installed).unwrap(), b"keep me");
        assert!(download.is_file());

        // A repair of exactly the files that were present when setup started is allowed.
        let expected = resource_marker(&home).unwrap();
        assert!(
            install_game_files_if_current(&download, &home, &expected)
                .unwrap()
                .is_some()
        );
        assert!(!installed.exists());
        assert!(!download.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn damaged_prepared_archives_cannot_replace_installed_resources() {
        let home = temp_home("damaged-prepared");
        let original = home.join("original.zip");
        write_archive(&original, &prepared());
        Pack::open(&original).unwrap().install(&home).unwrap();

        let version = horizon_core::version::DATA_VERSION;
        for (name, body, message) in [
            (
                "horizon-resources.json",
                Some("{".into()),
                "horizon-resources.json is not valid JSON",
            ),
            (
                "horizon-resources.json",
                Some(serde_json::json!({"data_version": version - 1}).to_string()),
                "names data version",
            ),
            ("facts.json", Some(String::new()), "facts.json is empty"),
            ("facts.json", None, "facts.json is missing"),
        ] {
            let archive = home.join("damaged.zip");
            let mut entries = prepared_entries();
            entries.retain(|(entry, _)| *entry != name);
            if let Some(body) = body {
                entries.push((name, body));
            }
            write_archive(&archive, &entries);
            let error = Pack::open(&archive)
                .unwrap()
                .install(&home)
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
            assert_eq!(
                fs::read_to_string(home.join("resources/facts.json")).unwrap(),
                "{}"
            );
            assert!(archive.is_file());
            assert!(!home.join("resources.unpacking").exists());
            assert!(!home.join("resources.staged").exists());
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn concurrent_resource_imports_publish_one_complete_archive() {
        let home = temp_home("concurrent-resources");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for owner in ["a", "b"] {
            let archive = home.join(format!("{owner}.zip"));
            let mut entries: Vec<_> = prepared_entries()
                .into_iter()
                .map(|(name, body)| (name.to_owned(), body))
                .collect();
            entries.push(("owner.txt".into(), owner.to_owned()));
            for index in 0..1500 {
                entries.push((format!("assets/shared/{index:04}.dat"), owner.repeat(128)));
            }
            write_archive(&archive, &entries);
            let pack = Pack::open(&archive).unwrap();
            let home = home.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                pack.install(&home).map_err(|e| e.to_string())
            }));
        }
        for worker in workers {
            let installed = worker.join().unwrap().unwrap();
            assert_eq!(installed.files, 1501 + prepared_entries().len());
        }
        let owner = fs::read_to_string(home.join("resources/owner.txt")).unwrap();
        assert!(matches!(owner.as_str(), "a" | "b"));
        for index in 0..1500 {
            assert_eq!(
                fs::read_to_string(home.join(format!("resources/assets/shared/{index:04}.dat")))
                    .unwrap(),
                owner.repeat(128)
            );
        }
        assert!(!home.join("resources.unpacking").exists());
        assert!(!home.join("resources.previous").exists());
        assert!(!home.join("a.zip").exists() && !home.join("b.zip").exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn concurrent_world_and_pack_imports_keep_each_unique_destination() {
        for kind in [Kind::World, Kind::ResourcePack] {
            let home = temp_home(&format!("concurrent-{}", kind.folder()));
            let barrier = Arc::new(std::sync::Barrier::new(16));
            let mut workers = Vec::new();
            for index in 0..16 {
                let owner = format!("owner-{index}");
                let source = home.join(format!("source-{index}"));
                fs::create_dir_all(&source).unwrap();
                let archive = source.join("Shared.zip");
                write_archive(
                    &archive,
                    &[
                        (kind.marker(), owner.as_str()),
                        ("owner.txt", owner.as_str()),
                    ],
                );
                let pack = Pack::open(&archive).unwrap();
                let home = home.clone();
                let barrier = barrier.clone();
                workers.push(std::thread::spawn(move || {
                    barrier.wait();
                    (owner, pack.install(&home).map_err(|e| e.to_string()))
                }));
            }
            let mut destinations = std::collections::BTreeSet::new();
            for worker in workers {
                let (owner, installed) = worker.join().unwrap();
                let installed = installed.unwrap();
                assert!(
                    destinations.insert(installed.path.clone()),
                    "reused a destination"
                );
                let contents = match kind {
                    Kind::World => fs::read_to_string(installed.path.join("owner.txt")).unwrap(),
                    Kind::ResourcePack => {
                        let mut archive =
                            ZipArchive::new(File::open(&installed.path).unwrap()).unwrap();
                        let mut contents = String::new();
                        archive
                            .by_name("owner.txt")
                            .unwrap()
                            .read_to_string(&mut contents)
                            .unwrap();
                        contents
                    }
                    Kind::Resources => unreachable!(),
                };
                assert_eq!(contents, owner);
            }
            assert_eq!(destinations.len(), 16);
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn startup_scan_leaves_an_upload_to_the_transaction_that_landed_it() {
        let home = temp_home("scan-during-upload");
        let archive = home.join("uploaded.zip");
        write_archive(&archive, &[("level.dat", "uploaded world")]);

        let access = acquire_home_access(&home).unwrap();
        let (started, waiting) = std::sync::mpsc::channel();
        let scanner_home = home.clone();
        let scanner = std::thread::spawn(move || {
            started.send(()).unwrap();
            scan(&scanner_home)
        });
        waiting.recv().unwrap();

        let installed = Pack::open(&archive)
            .unwrap()
            .install_with_access(&home, &access, || true)
            .unwrap();
        drop(access);
        assert!(scanner.join().unwrap().is_empty());
        assert_eq!(
            fs::read(installed.path.join("level.dat")).unwrap(),
            b"uploaded world"
        );
        assert_eq!(fs::read_dir(home.join("saves")).unwrap().count(), 1);
        assert!(!archive.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn recovery_waits_for_an_active_home_transaction_even_through_an_alias() {
        use std::sync::mpsc;
        use std::time::Duration;
        let base = temp_home("recover-concurrent");
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        #[cfg(unix)]
        let alias = {
            let alias = base.join("alias");
            std::os::unix::fs::symlink(&home, &alias).unwrap();
            alias
        };
        #[cfg(not(unix))]
        let alias = home.clone();
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let writer_home = home.clone();
        let writer = std::thread::spawn(move || {
            with_home_access(&writer_home, || {
                let scratch = writer_home.join("resources.unpacking");
                write_prepared_tree(&scratch, "staged")?;
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                fs::write(scratch.join("complete.txt"), "complete")?;
                fs::rename(&scratch, writer_home.join("resources.staged"))?;
                Ok(())
            })
            .map_err(|e| e.to_string())
        });
        entered_rx.recv().unwrap();
        let (started, started_rx) = mpsc::channel();
        let (finished, finished_rx) = mpsc::channel();
        let recovery_home = alias.clone();
        let recovery = std::thread::spawn(move || {
            started.send(()).unwrap();
            finished
                .send(recover(&recovery_home).map_err(|e| e.to_string()))
                .unwrap();
        });
        started_rx.recv().unwrap();
        let early = finished_rx.recv_timeout(Duration::from_millis(50));
        release.send(()).unwrap();
        writer.join().unwrap().unwrap();
        recovery.join().unwrap();
        assert!(
            matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
            "recovered a partial transaction"
        );
        assert_eq!(
            finished_rx.recv().unwrap().unwrap(),
            Some(alias.join("resources"))
        );
        assert_eq!(
            fs::read_to_string(home.join("resources/complete.txt")).unwrap(),
            "complete"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn detached_and_handoff_clones_keep_import_and_recovery_waiting_until_last_drop() {
        use std::sync::mpsc;
        use std::time::Duration;
        let home = temp_home("owned-transaction-handoff");
        fs::create_dir_all(home.join("resources")).unwrap();
        fs::write(home.join("resources/horizon-resources.json"), "old").unwrap();
        fs::write(home.join("resources/payload.txt"), "old payload").unwrap();
        let archive = home.join("new.zip");
        let mut entries = prepared_entries();
        entries.push(("payload.txt", "new payload".into()));
        write_archive(&archive, &entries);
        let pack = Pack::open(&archive).unwrap();
        let original = acquire_home_access(&home).unwrap();
        let handoff = original.clone();
        let detached_access = original.clone();
        drop(original);
        let (tail_started, tail_started_rx) = mpsc::channel();
        let (finish_tail, finish_tail_rx) = mpsc::channel();
        let tail_home = home.clone();
        let tail = std::thread::spawn(move || {
            let _access = detached_access;
            tail_started.send(()).unwrap();
            finish_tail_rx.recv().unwrap();
            fs::read_to_string(tail_home.join("resources/payload.txt")).unwrap()
        });
        tail_started_rx.recv().unwrap();
        let (writer_started, writer_started_rx) = mpsc::channel();
        let (written, written_rx) = mpsc::channel();
        let writer_home = home.clone();
        let writer = std::thread::spawn(move || {
            writer_started.send(()).unwrap();
            written
                .send(pack.install(&writer_home).map_err(|e| e.to_string()))
                .unwrap();
        });
        let (recovery_started, recovery_started_rx) = mpsc::channel();
        let (recovered, recovered_rx) = mpsc::channel();
        let recovery_home = home.clone();
        let recovery = std::thread::spawn(move || {
            recovery_started.send(()).unwrap();
            recovered
                .send(recover(&recovery_home).map_err(|e| e.to_string()))
                .unwrap();
        });
        writer_started_rx.recv().unwrap();
        recovery_started_rx.recv().unwrap();
        let writer_during_tail = written_rx.recv_timeout(Duration::from_millis(50));
        let recovery_during_tail = recovered_rx.recv_timeout(Duration::from_millis(50));
        finish_tail.send(()).unwrap();
        let observed = tail.join().unwrap();
        let writer_during_handoff = written_rx.recv_timeout(Duration::from_millis(50));
        let recovery_during_handoff = recovered_rx.recv_timeout(Duration::from_millis(50));
        drop(handoff);
        for blocked in [
            writer_during_tail.map(|_| ()),
            recovery_during_tail.map(|_| ()),
            writer_during_handoff.map(|_| ()),
            recovery_during_handoff.map(|_| ()),
        ] {
            assert!(
                matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)),
                "an operation escaped a live transaction clone"
            );
        }
        assert_eq!(observed, "old payload");
        assert_eq!(
            written_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .kind,
            Kind::Resources
        );
        assert_eq!(
            recovered_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            None
        );
        writer.join().unwrap();
        recovery.join().unwrap();
        assert_eq!(
            fs::read_to_string(home.join("resources/horizon-resources.json")).unwrap(),
            prepared_entries()[0].1
        );
        assert_eq!(
            fs::read_to_string(home.join("resources/payload.txt")).unwrap(),
            "new payload"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn owned_home_access_releases_waiting_imports_on_error_and_unwind() {
        use std::sync::mpsc;
        use std::time::Duration;
        for panic in [false, true] {
            let home = temp_home(&format!("owned-transaction-error-{panic}"));
            let archive = home.join("new.zip");
            write_archive(&archive, &prepared_entries());
            let pack = Pack::open(&archive).unwrap();
            let access = acquire_home_access(&home).unwrap();
            let (started, started_rx) = mpsc::channel();
            let (finished, finished_rx) = mpsc::channel();
            let writer_home = home.clone();
            let writer = std::thread::spawn(move || {
                started.send(()).unwrap();
                finished
                    .send(pack.install(&writer_home).map_err(|e| e.to_string()))
                    .unwrap();
            });
            started_rx.recv().unwrap();
            let blocked = finished_rx.recv_timeout(Duration::from_millis(50));
            let outcome = std::panic::catch_unwind(move || -> Result<()> {
                let _access = access;
                if panic {
                    panic!("resource stage failed");
                }
                Err("resource stage failed".into())
            });
            assert!(matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)));
            match outcome {
                Err(_) => assert!(panic),
                Ok(Err(_)) => assert!(!panic),
                Ok(Ok(())) => panic!("the stage unexpectedly succeeded"),
            }
            assert_eq!(
                finished_rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap()
                    .kind,
                Kind::Resources
            );
            writer.join().unwrap();
            assert_eq!(
                fs::read_to_string(home.join("resources/horizon-resources.json")).unwrap(),
                prepared_entries()[0].1
            );
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn resource_read_transaction_observes_one_generation_while_an_import_waits() {
        use std::sync::mpsc;
        use std::time::Duration;
        let home = temp_home("read-transaction");
        fs::create_dir_all(home.join("resources")).unwrap();
        fs::write(home.join("resources/horizon-resources.json"), "old").unwrap();
        fs::write(home.join("resources/payload.txt"), "old payload").unwrap();
        let archive = home.join("new.zip");
        let mut entries = prepared_entries();
        entries.push(("payload.txt", "new payload".into()));
        write_archive(&archive, &entries);
        let pack = Pack::open(&archive).unwrap();
        let (read_first, read_first_rx) = mpsc::channel();
        let (continue_reading, continue_reading_rx) = mpsc::channel();
        let reader_home = home.clone();
        let reader = std::thread::spawn(move || {
            with_home_access(&reader_home, || {
                let first =
                    fs::read_to_string(reader_home.join("resources/horizon-resources.json"))?;
                read_first.send(first.clone()).unwrap();
                continue_reading_rx.recv().unwrap();
                let payload = fs::read_to_string(reader_home.join("resources/payload.txt"))?;
                Ok((first, payload))
            })
            .map_err(|e| e.to_string())
        });
        assert_eq!(read_first_rx.recv().unwrap(), "old");
        let (started, started_rx) = mpsc::channel();
        let (completed, completed_rx) = mpsc::channel();
        let writer_home = home.clone();
        let writer = std::thread::spawn(move || {
            started.send(()).unwrap();
            completed
                .send(pack.install(&writer_home).map_err(|e| e.to_string()))
                .unwrap();
        });
        started_rx.recv().unwrap();
        let early = completed_rx.recv_timeout(Duration::from_millis(50));
        continue_reading.send(()).unwrap();
        let observed = reader.join().unwrap().unwrap();
        writer.join().unwrap();
        assert!(
            matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
            "replaced resources during a read transaction"
        );
        assert_eq!(observed, ("old".to_owned(), "old payload".to_owned()));
        let installed = completed_rx.recv().unwrap().unwrap();
        assert_eq!(installed.path, home.join("resources"));
        assert_eq!(
            fs::read_to_string(installed.path.join("horizon-resources.json")).unwrap(),
            prepared_entries()[0].1
        );
        assert_eq!(
            fs::read_to_string(installed.path.join("payload.txt")).unwrap(),
            "new payload"
        );
        assert!(!archive.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn separate_import_homes_can_make_progress_independently() {
        use std::sync::mpsc;
        use std::time::Duration;
        let base = temp_home("independent-homes");
        let first = base.join("first");
        let second = base.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            with_home_access(&first, || {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .map_err(|e| e.to_string())
        });
        entered_rx.recv().unwrap();
        let (finished, finished_rx) = mpsc::channel();
        let other = std::thread::spawn(move || {
            with_home_access(&second, || {
                fs::write(second.join("progress.txt"), "ready")?;
                finished.send(()).unwrap();
                Ok(())
            })
            .map_err(|e| e.to_string())
        });
        let independent = finished_rx.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        writer.join().unwrap().unwrap();
        other.join().unwrap().unwrap();
        assert!(
            independent.is_ok(),
            "another home was unnecessarily blocked"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn replacing_game_files_invalidates_the_previous_prepared_cache() {
        let home = temp_home("prepared-cache-replace");
        let root = home.join("resources");
        fs::create_dir_all(root.join("assets/minecraft/textures/block")).unwrap();
        fs::write(root.join("horizon-resources.json"), r#"{"schema":7}"#).unwrap();
        let member = "assets/minecraft/textures/block/grass.png";
        fs::write(root.join(member), b"old pixels").unwrap();
        horizon_core::prepared_files::write(&root).unwrap();
        let previous = horizon_core::prepared_files::open(&root).unwrap();
        let archive = home.join("new.zip");
        write_archive(&archive, &prepared());
        Pack::open(&archive).unwrap().install(&home).unwrap();
        assert_eq!(
            horizon_core::prepared_files::read(&root.join(member)).unwrap(),
            b"png"
        );
        assert_eq!(previous.read(member).unwrap().unwrap(), b"old pixels");
        horizon_core::prepared_files::invalidate(&root);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn desktop_prepared_binary_is_optional_after_mobile_import() {
        let home = temp_home("desktop-prepared-cache");
        let desktop = home.join("desktop");
        fs::create_dir_all(desktop.join("assets/minecraft/textures/block")).unwrap();
        for (name, body) in prepared_entries() {
            fs::write(desktop.join(name), body).unwrap();
        }
        let member = "assets/minecraft/textures/block/grass.png";
        fs::write(desktop.join(member), b"portable pixels").unwrap();
        horizon_core::prepared_files::write(&desktop).unwrap();
        let archive = home.join("prepared.zip");
        pack(&desktop, &archive, |_, _| {}).unwrap();
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        assert!(
            installed
                .path
                .join(horizon_core::prepared_files::FILE_NAME)
                .is_file()
        );
        // The cache names its original root; the original files remain portable.
        assert!(horizon_core::prepared_files::open(&installed.path).is_none());
        assert_eq!(
            horizon_core::prepared_files::read(&installed.path.join(member)).unwrap(),
            b"portable pixels"
        );
        horizon_core::prepared_files::invalidate(&installed.path);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_folder_wrapped_archive_means_the_same_thing() {
        // Compressing a folder on a desktop puts the folder's name in front of every entry.
        let home = temp_home("wrapped");
        let archive = home.join("Horizon-26.3.zip");
        let entries: Vec<(String, String)> = prepared()
            .into_iter()
            .map(|(name, body)| (format!("Horizon-26.3/{name}"), body))
            .collect();
        write_archive(&archive, &entries);
        let (kind, item) = classify(&archive).unwrap();
        assert_eq!(kind, Kind::Resources);
        assert_eq!(item, "Horizon-26.3");
        Pack::open(&archive).unwrap().install(&home).unwrap();
        assert!(home.join("resources/horizon-resources.json").is_file());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn one_folder_too_many_still_reads() {
        // Compressing `.local/horizon-client` instead of `26.3` puts the manifest two levels down.
        let home = temp_home("double");
        let archive = home.join("horizon-client.zip");
        let entries: Vec<(String, String)> = prepared()
            .into_iter()
            .map(|(name, body)| (format!("horizon-client/26.3/{name}"), body))
            .collect();
        write_archive(&archive, &entries);
        let (kind, item) = classify(&archive).unwrap();
        assert_eq!(kind, Kind::Resources);
        assert_eq!(item, "26.3");
        Pack::open(&archive).unwrap().install(&home).unwrap();
        assert!(home.join("resources/horizon-resources.json").is_file());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn desktop_droppings_do_not_hide_the_folder() {
        let home = temp_home("droppings");
        let archive = home.join("world.zip");
        write_archive(
            &archive,
            &[
                ("MyWorld/level.dat", "nbt"),
                ("MyWorld/session.lock", "locked"),
                ("MyWorld/data/session.lock", "nested data"),
                ("session.lock", "lock beside wrapper"),
                ("__MACOSX/MyWorld/._level.dat", "junk"),
                (".DS_Store", "junk"),
            ],
        );
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        assert_eq!(installed.kind, Kind::World);
        assert_eq!(installed.files, 2);
        assert_eq!(installed.path, home.join("saves/MyWorld"));
        assert_eq!(
            fs::read(installed.path.join("data/session.lock")).unwrap(),
            b"nested data"
        );
        assert!(home.join("saves/MyWorld/level.dat").is_file());
        assert!(!home.join("saves/MyWorld/session.lock").exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_world_is_never_written_over() {
        let home = temp_home("collision");
        fs::create_dir_all(home.join("saves/MyWorld")).unwrap();
        fs::write(home.join("saves/MyWorld/level.dat"), "mine").unwrap();
        let archive = home.join("world.zip");
        write_archive(&archive, &[("MyWorld/level.dat", "theirs")]);
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        assert_eq!(installed.path.file_name().unwrap(), "MyWorld (2)");
        assert_eq!(
            fs::read(home.join("saves/MyWorld/level.dat")).unwrap(),
            b"mine".to_vec()
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_pack_stays_an_archive() {
        let home = temp_home("pack");
        let archive = home.join("faithful.zip");
        write_archive(&archive, &[("pack.mcmeta", "{}"), ("assets/x.png", "p")]);
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        assert_eq!(installed.kind, Kind::ResourcePack);
        assert!(installed.path.ends_with("resourcepacks/faithful.zip"));
        assert!(!home.join("resourcepacks/faithful").exists());
        fs::remove_dir_all(home).unwrap();
    }

    fn texture_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[1, 2, 3, 255, 97, 38, 19, 17])
                .unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    fn pack_metadata() -> String {
        format!(
            r#"{{"pack":{{"description":"wrapped","pack_format":{}}}}}"#,
            crate::packs::RESOURCE_FORMAT.0
        )
    }

    fn assert_pack_repository_reads_texture(home: &Path, expected: &[u8]) {
        let vanilla = home.join("vanilla");
        fs::create_dir_all(&vanilla).unwrap();
        let packs = crate::packs::discover(&vanilla, &home.join("resourcepacks"));
        assert_eq!(
            packs.len(),
            1,
            "installed ZIP was not discovered as a resource pack"
        );
        let overlay = home.join("overlay");
        crate::packs::build_overlay(&vanilla, &packs, &overlay).unwrap();
        assert_eq!(
            fs::read(overlay.join("assets/test/textures/item/probe.png")).unwrap(),
            expected
        );
    }

    #[test]
    fn generated_resource_pack_archive_round_trips_into_the_repository() {
        let home = temp_home("resource-pack-round-trip");
        let source = home.join("FancyPack");
        fs::create_dir_all(source.join("assets/test/textures/item")).unwrap();
        let pixels = texture_png();
        fs::write(source.join("pack.mcmeta"), pack_metadata()).unwrap();
        fs::write(source.join("assets/test/textures/item/probe.png"), &pixels).unwrap();
        let archive = home.join("packed.zip");
        assert_eq!(
            pack(&source, &archive, |_, _| {}).unwrap().kind,
            Kind::ResourcePack
        );
        fs::remove_dir_all(source).unwrap();
        let imported = Pack::open(&archive).unwrap();
        assert_eq!(imported.prefix, "FancyPack/");
        assert_eq!(imported.install(&home).unwrap().kind, Kind::ResourcePack);
        assert!(!archive.exists());
        assert_pack_repository_reads_texture(&home, &pixels);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn one_and_two_folder_resource_packs_keep_compressed_assets_at_the_zip_root() {
        for (case, prefix) in ["Outer/", "Outer/Inner/"].into_iter().enumerate() {
            let home = temp_home(&format!("resource-pack-prefix-{case}"));
            let archive = home.join("wrapped.zip");
            let pixels = texture_png();
            let mut zip = ZipWriter::new(File::create(&archive).unwrap());
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            for (name, bytes) in [
                ("pack.mcmeta", pack_metadata().into_bytes()),
                ("assets/test/textures/item/probe.png", pixels.clone()),
            ] {
                zip.start_file(format!("{prefix}{name}"), options).unwrap();
                zip.write_all(&bytes).unwrap();
            }
            zip.finish().unwrap();
            let imported = Pack::open(&archive).unwrap();
            assert_eq!(imported.prefix, prefix);
            let installed = imported.install(&home).unwrap();
            let mut normalized = ZipArchive::new(File::open(&installed.path).unwrap()).unwrap();
            assert_eq!(
                normalized
                    .by_name("assets/test/textures/item/probe.png")
                    .unwrap()
                    .compression(),
                CompressionMethod::Deflated
            );
            assert!(normalized.by_name(&format!("{prefix}pack.mcmeta")).is_err());
            drop(normalized);
            assert_pack_repository_reads_texture(&home, &pixels);
            assert_eq!(fs::read_dir(home.join("resourcepacks")).unwrap().count(), 1);
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn normalizing_a_wrapped_pack_preserves_the_last_duplicate_entry() {
        let home = temp_home("resource-pack-duplicates");
        let archive = home.join("wrapped.zip");
        write_archive(
            &archive,
            &[
                ("Wrap/pack.mcmeta", pack_metadata()),
                ("Wrap/assets/test/probeA.txt", "first".to_owned()),
                ("Wrap/assets/test/probeB.txt", "last".to_owned()),
            ],
        );
        // ZipWriter refuses duplicate names. Rename the second entry in its
        // local and central headers without changing their encoded lengths.
        let mut bytes = fs::read(&archive).unwrap();
        for at in 0..=bytes.len() - b"probeB.txt".len() {
            if bytes[at..].starts_with(b"probeB.txt") {
                bytes[at..at + b"probeA.txt".len()].copy_from_slice(b"probeA.txt");
            }
        }
        fs::write(&archive, bytes).unwrap();
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        let mut zip = ZipArchive::new(File::open(installed.path).unwrap()).unwrap();
        let mut contents = String::new();
        zip.by_name("assets/test/probeA.txt")
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "last");
        assert_eq!(zip.len(), 2);
        drop(zip);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn pack_publication_hides_partial_bytes_and_cleans_up_failed_writes() {
        use std::io::Write;

        let home = temp_home("pack-publication");
        let packs = home.join(Kind::ResourcePack.folder());
        let destination = packs.join("new.zip");
        let staging = packs.join(PACK_IMPORT_STAGING);
        let published = publish_pack(&destination, |mut file| {
            file.write_all(b"partial archive")?;
            assert!(staging.is_file());
            assert!(!destination.exists());
            Err(io::Error::other("simulated copy failure").into())
        });
        assert!(published.is_err());
        assert!(!destination.exists());
        assert!(!staging.exists());

        // An interrupted process leaves staging bytes until startup recovery.
        fs::write(&staging, b"interrupted copy").unwrap();
        let existing = packs.join("existing.zip");
        fs::write(&existing, b"existing pack").unwrap();
        assert_eq!(recover(&home).unwrap(), None);
        assert!(!staging.exists());
        assert_eq!(fs::read(existing).unwrap(), b"existing pack");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn pack_publication_keeps_a_pack_that_arrived_while_staging() {
        let home = temp_home("pack-publication-collision");
        let preferred = home.join("resourcepacks/Shared.zip");
        let (installed, bytes) = publish_pack(&preferred, |mut output| {
            output.write_all(b"imported pack")?;
            assert!(!preferred.exists());
            fs::write(&preferred, b"arriving pack")?;
            Ok(output)
        })
        .unwrap();

        assert_eq!(installed, home.join("resourcepacks/Shared (2).zip"));
        assert_eq!(fs::read(preferred).unwrap(), b"arriving pack");
        assert_eq!(fs::read(installed).unwrap(), b"imported pack");
        assert_eq!(bytes, 13);
        assert!(
            !home
                .join("resourcepacks")
                .join(PACK_IMPORT_STAGING)
                .exists()
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn pack_rename_retries_after_a_competing_file_claims_the_name() {
        let home = temp_home("pack-rename-collision");
        let staged = home.join("staged");
        let preferred = home.join("Shared.zip");
        fs::write(&staged, b"imported pack").unwrap();
        let mut claimed = false;
        let installed = publish_without_replacing(&staged, &preferred, |from, to| {
            if !claimed {
                assert_eq!(to, preferred);
                fs::write(to, b"arriving pack")?;
                claimed = true;
            }
            rename_pack_without_replacing(from, to)
        })
        .unwrap();

        assert_eq!(installed, home.join("Shared (2).zip"));
        assert_eq!(fs::read(preferred).unwrap(), b"arriving pack");
        assert_eq!(fs::read(installed).unwrap(), b"imported pack");
        assert!(!staged.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_flat_resource_pack_keeps_its_original_archive_bytes() {
        let home = temp_home("resource-pack-flat-bytes");
        let archive = home.join("flat.zip");
        write_archive(&archive, &[("pack.mcmeta", pack_metadata())]);
        let original = fs::read(&archive).unwrap();
        let imported = Pack::open(&archive).unwrap();
        assert!(imported.prefix.is_empty());
        let installed = imported.install(&home).unwrap();
        assert_eq!(fs::read(installed.path).unwrap(), original);
        assert!(!archive.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn invalid_wrapped_resource_packs_keep_the_original_without_publishing() {
        for symlink in [false, true] {
            let home = temp_home(&format!("resource-pack-invalid-{symlink}"));
            let archive = home.join("wrapped.zip");
            let mut zip = ZipWriter::new(File::create(&archive).unwrap());
            let options = SimpleFileOptions::default();
            zip.start_file("Wrap/pack.mcmeta", options).unwrap();
            zip.write_all(pack_metadata().as_bytes()).unwrap();
            if symlink {
                zip.add_symlink("Wrap/assets/link", "../../outside", options)
                    .unwrap();
            } else {
                zip.start_file("Wrap/../outside", options).unwrap();
                zip.write_all(b"outside").unwrap();
            }
            zip.finish().unwrap();
            let original = fs::read(&archive).unwrap();
            assert!(Pack::open(&archive).unwrap().install(&home).is_err());
            assert_eq!(fs::read(&archive).unwrap(), original);
            assert!(!home.join("resourcepacks").exists());
            assert!(!home.join("outside").exists());
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn failed_wrapped_resource_pack_destination_keeps_the_original() {
        let home = temp_home("resource-pack-write-failure");
        let archive = home.join("wrapped.zip");
        write_archive(&archive, &[("Wrap/pack.mcmeta", pack_metadata())]);
        let original = fs::read(&archive).unwrap();
        // An existing file blocks the destination directory on every platform.
        fs::write(home.join("resourcepacks"), b"keep me").unwrap();
        assert!(Pack::open(&archive).unwrap().install(&home).is_err());
        assert_eq!(fs::read(&archive).unwrap(), original);
        assert_eq!(fs::read(home.join("resourcepacks")).unwrap(), b"keep me");
        assert_eq!(fs::read_dir(&home).unwrap().count(), 2);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn paths_that_escape_are_refused() {
        assert_eq!(inside("a/b.png", ""), Some(PathBuf::from("a/b.png")));
        assert_eq!(
            inside("Top/a/b.png", "Top/"),
            Some(PathBuf::from("a/b.png"))
        );
        assert!(inside("../out", "").is_none());
        assert!(inside("a/../../out", "").is_none());
        assert!(inside("/etc/passwd", "").is_none());
        assert!(inside("C:/Windows/system.ini", "").is_none());
        assert!(inside("assets/test/invalid\0.png", "").is_none());
        assert!(inside("", "").is_none());
    }

    #[test]
    fn corrupt_archive_removes_partial_tree_and_keeps_originals() {
        for resources in [false, true] {
            let home = temp_home(if resources {
                "crc-resources"
            } else {
                "crc-world"
            });
            fs::create_dir_all(home.join("saves/kept")).unwrap();
            fs::write(home.join("saves/kept/level.dat"), "existing world").unwrap();
            fs::create_dir_all(home.join("resources")).unwrap();
            fs::write(home.join("resources/facts.json"), "existing resources").unwrap();
            let archive = home.join("broken.zip");
            let marker = if resources {
                "horizon-resources.json"
            } else {
                "level.dat"
            };
            let payload = "unique corruptible region payload";
            write_archive(&archive, &[(marker, "{}"), ("region/r.0.0.mca", payload)]);
            let mut bytes = fs::read(&archive).unwrap();
            let offset = bytes
                .windows(payload.len())
                .position(|part| part == payload.as_bytes())
                .unwrap();
            bytes[offset] ^= 1;
            fs::write(&archive, bytes).unwrap();
            assert!(Pack::open(&archive).unwrap().install(&home).is_err());
            let partial = if resources {
                "resources.unpacking"
            } else {
                "saves/broken"
            };
            assert!(
                !home.join(partial).exists(),
                "failed extraction left {partial} visible"
            );
            assert!(!home.join("saves").join(WORLD_IMPORT_STAGING).exists());
            assert!(
                archive.exists(),
                "failed import consumed the original archive"
            );
            assert_eq!(
                fs::read_to_string(home.join("saves/kept/level.dat")).unwrap(),
                "existing world"
            );
            assert_eq!(
                fs::read_to_string(home.join("resources/facts.json")).unwrap(),
                "existing resources"
            );
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn world_import_uses_another_name_if_the_first_is_claimed_during_publication() {
        let movers = [
            rename_world_without_replacing as fn(&Path, &Path) -> io::Result<()>,
            #[cfg(not(windows))]
            rename_into_reserved_world_folder,
        ];
        for rename in movers {
            let home = temp_home("world-publication-collision");
            let staged = home.join("staged");
            let preferred = home.join("My World");
            fs::create_dir(&staged).unwrap();
            fs::write(staged.join("level.dat"), b"imported world").unwrap();
            let mut claimed = false;
            let installed = publish_without_replacing(&staged, &preferred, |from, to| {
                if !claimed {
                    assert_eq!(to, preferred);
                    fs::create_dir(to)?;
                    claimed = true;
                }
                rename(from, to)
            })
            .unwrap();

            assert_eq!(installed, home.join("My World (2)"));
            assert_eq!(fs::read_dir(&preferred).unwrap().count(), 0);
            assert_eq!(
                fs::read(installed.join("level.dat")).unwrap(),
                b"imported world"
            );
            assert!(!staged.exists());
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn interrupted_world_stays_off_the_list_and_can_be_reimported() {
        let home = temp_home("interrupted-world");
        let saves = home.join("saves");
        let staging = saves.join(WORLD_IMPORT_STAGING);
        let scratch = staging.join("world");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("level.dat"), "partial world").unwrap();
        let existing = saves.join("My World");
        fs::create_dir_all(&existing).unwrap();
        fs::write(existing.join("level.dat"), "original world").unwrap();
        let archive = home.join("world.zip");
        write_archive(
            &archive,
            &[
                ("My World/level.dat", "imported"),
                ("My World/region/r.0.0.mca", "region"),
            ],
        );

        let listed = crate::title::list_worlds(&saves);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].folder, "My World");
        assert_eq!(recover(&home).unwrap(), None);
        assert!(!staging.exists());
        assert!(archive.is_file());

        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        assert_eq!(installed.path, saves.join("My World (2)"));
        assert_eq!(
            fs::read(existing.join("level.dat")).unwrap(),
            b"original world"
        );
        assert_eq!(
            fs::read(installed.path.join("region/r.0.0.mca")).unwrap(),
            b"region"
        );
        assert!(!staging.exists());
        assert!(!archive.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_interrupted_extraction_is_never_published_as_ready() {
        for previous in [false, true] {
            let home = temp_home(if previous {
                "partial-with-old"
            } else {
                "partial-first"
            });
            fs::create_dir_all(home.join("resources.unpacking")).unwrap();
            fs::write(
                home.join("resources.unpacking/horizon-resources.json"),
                "partial",
            )
            .unwrap();
            fs::write(home.join("resources.unpacking/staged.txt"), "partial").unwrap();
            if previous {
                write_prepared_tree(&home.join("resources.previous"), "old complete tree").unwrap();
            }
            let recovered = recover(&home).unwrap();
            assert!(!home.join("resources/staged.txt").exists());
            if previous {
                assert_eq!(recovered, Some(home.join("resources")));
                assert_eq!(
                    fs::read_to_string(home.join("resources/payload.txt")).unwrap(),
                    "old complete tree"
                );
            } else {
                assert!(recovered.is_none());
                assert!(!home.join("resources").exists());
            }
            assert!(!home.join("resources.unpacking").exists());
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn extraction_refuses_to_mix_new_files_with_an_existing_staging_tree() {
        let home = temp_home("staging-occupied");
        let scratch = home.join("resources.unpacking");
        fs::create_dir(&scratch).unwrap();
        fs::write(scratch.join("existing"), "keep").unwrap();
        let archive = home.join("game.zip");
        write_archive(&archive, &prepared());

        assert!(
            Pack::open(&archive)
                .unwrap()
                .unpack(&scratch, &|| true)
                .is_err()
        );
        assert_eq!(fs::read(scratch.join("existing")).unwrap(), b"keep");
        assert!(!scratch.join("horizon-resources.json").exists());
        assert!(archive.is_file());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_blocked_staging_rename_cleans_the_unpublished_resources() {
        let home = temp_home("blocked-resource-staging");
        let archive = home.join("game.zip");
        let resources = home.join("resources");
        let staged = home.join("resources.staged");
        fs::create_dir(&resources).unwrap();
        fs::write(resources.join("horizon-resources.json"), "installed").unwrap();
        fs::write(&staged, "unrelated file at the staging path").unwrap();
        write_archive(&archive, &prepared());

        assert!(Pack::open(&archive).unwrap().install(&home).is_err());
        assert!(!home.join("resources.unpacking").exists());
        assert_eq!(
            fs::read(&staged).unwrap(),
            b"unrelated file at the staging path"
        );
        assert_eq!(
            fs::read(resources.join("horizon-resources.json")).unwrap(),
            b"installed"
        );
        assert!(archive.is_file());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn cancelling_during_extraction_leaves_the_installed_files_and_archive() {
        let home = temp_home("cancelled-import");
        let resources = home.join("resources");
        fs::create_dir(&resources).unwrap();
        fs::write(resources.join("horizon-resources.json"), b"installed").unwrap();
        let archive = home.join("game.zip");
        let large_size = 3 << 20;
        let mut entries = prepared();
        entries.push(("large-file", "a".repeat(large_size)));
        write_archive(&archive, &entries);
        let staged_file = home.join("resources.unpacking/large-file");
        let cancelled_at = std::cell::Cell::new(None);
        let result = Pack::open(&archive).unwrap().install_guarded(&home, || {
            let copied = fs::metadata(&staged_file).map_or(0, |file| file.len());
            if copied == 0 {
                return true;
            }
            cancelled_at.set(Some(copied));
            false
        });
        assert_eq!(result.unwrap_err().to_string(), "the import was cancelled");
        let copied = cancelled_at.get().expect("cancellation during extraction");
        assert!(copied < large_size as u64);
        assert_eq!(
            fs::read(resources.join("horizon-resources.json")).unwrap(),
            b"installed"
        );
        assert!(archive.is_file());
        assert!(!home.join("resources.unpacking").exists());
        assert!(!home.join("resources.staged").exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_already_consumed_world_archive_is_not_installed_again() {
        let home = temp_home("world-source-consumed");
        let archive = home.join("world.zip");
        write_archive(&archive, &[("level.dat", "world fixture")]);
        let first = Pack::open(&archive).unwrap();
        let second = Pack::open(&archive).unwrap();

        first.install(&home).unwrap();
        assert!(second.install(&home).is_err());
        assert_eq!(fs::read_dir(home.join("saves")).unwrap().count(), 1);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_removed_source_does_not_fail_a_published_import() {
        let home = temp_home("source-removed");
        let archive = home.join("game.zip");
        write_archive(&archive, &prepared());
        let checks = std::cell::Cell::new(0);
        let installed = Pack::open(&archive)
            .unwrap()
            .install_guarded(&home, || {
                checks.set(checks.get() + 1);
                if checks.get() == 2 {
                    fs::remove_file(&archive).unwrap();
                }
                true
            })
            .unwrap();
        assert_eq!(installed.kind, Kind::Resources);
        assert!(installed.path.join("horizon-resources.json").is_file());
        assert!(!archive.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn failed_resource_swap_restores_previous_files_and_preserves_failed_rollback() {
        for refuse_rollback in [false, true] {
            let home = temp_home("swap-error");
            let dest = home.join("resources");
            let staged = home.join("resources.staged");
            let previous = home.join("resources.previous");
            fs::create_dir(&dest).unwrap();
            fs::create_dir(&staged).unwrap();
            fs::write(dest.join("horizon-resources.json"), "old").unwrap();
            fs::write(staged.join("horizon-resources.json"), "new").unwrap();

            let error = publish(&staged, &dest, |from, to| {
                if from == staged || (refuse_rollback && from == previous) {
                    return Err(io::Error::other("simulated rename failure"));
                }
                fs::rename(from, to)
            })
            .unwrap_err();
            assert!(error.to_string().contains("simulated rename failure"));
            assert_eq!(
                fs::read(staged.join("horizon-resources.json")).unwrap(),
                b"new"
            );

            if refuse_rollback {
                assert!(error.to_string().contains("could not restore"));
                assert!(!dest.exists());
                assert_eq!(
                    fs::read(previous.join("horizon-resources.json")).unwrap(),
                    b"old"
                );
                // The next attempt fails as well, this time before it can
                // replace anything. It must still have the backup to restore.
                publish(&staged, &dest, |from, to| {
                    if from == staged {
                        return Err(io::Error::other("simulated retry failure"));
                    }
                    fs::rename(from, to)
                })
                .unwrap_err();
            }
            assert_eq!(
                fs::read(dest.join("horizon-resources.json")).unwrap(),
                b"old"
            );
            assert!(!previous.exists());
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn retry_preserves_a_usable_backup_when_the_live_install_is_damaged() {
        let home = tempfile::tempdir().unwrap();
        let dest = home.path().join("resources");
        let previous = home.path().join("resources.previous");
        let staged = home.path().join("resources.staged");
        write_prepared_tree(&dest, "damaged installation").unwrap();
        fs::remove_file(dest.join("facts.json")).unwrap();
        write_prepared_tree(&previous, "last usable installation").unwrap();
        write_prepared_tree(&staged, "new installation").unwrap();

        let error = publish(&staged, &dest, |from, to| {
            if from == staged {
                return Err(io::Error::other("simulated retry failure"));
            }
            fs::rename(from, to)
        })
        .unwrap_err();

        assert!(error.to_string().contains("simulated retry failure"));
        assert_eq!(
            fs::read_to_string(dest.join("payload.txt")).unwrap(),
            "last usable installation"
        );
        assert!(damaged_prepared(&dest).is_none());
        assert!(staged.is_dir());
    }

    #[test]
    fn a_swap_left_half_done_is_finished() {
        let home = temp_home("swap");
        write_prepared_tree(&home.join("resources.staged"), "new files").unwrap();
        assert_eq!(recover(&home).unwrap(), Some(home.join("resources")));
        assert!(home.join("resources/horizon-resources.json").is_file());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_crash_between_the_two_renames_puts_the_old_tree_back() {
        let home = temp_home("previous");
        write_prepared_tree(&home.join("resources.previous"), "previous files").unwrap();
        assert_eq!(recover(&home).unwrap(), Some(home.join("resources")));
        assert!(home.join("resources/facts.json").is_file());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn recovery_restores_usable_files_when_the_interrupted_copy_is_damaged() {
        for (broken, obstruction) in [
            ("resources.staged", false),
            ("resources", false),
            ("resources", true),
        ] {
            let home = temp_home(broken);
            let previous = home.join("resources.previous");
            let dest = home.join("resources");
            write_prepared_tree(&previous, "last usable installation").unwrap();
            let broken = home.join(broken);
            if obstruction {
                fs::write(&broken, "unrelated data").unwrap();
            } else {
                write_prepared_tree(&broken, "interrupted installation").unwrap();
                fs::write(broken.join("facts.json"), "").unwrap();
            }

            assert_eq!(recover(&home).unwrap(), Some(dest.clone()));
            assert_eq!(
                fs::read_to_string(dest.join("payload.txt")).unwrap(),
                "last usable installation"
            );
            assert!(!previous.exists());
            if obstruction {
                assert_eq!(
                    fs::read(home.join("resources.obstructed")).unwrap(),
                    b"unrelated data"
                );
            }
            assert!(!home.join("resources.staged").exists());
            fs::remove_dir_all(home).unwrap();
        }
    }

    #[test]
    fn recovery_does_not_publish_a_damaged_first_install() {
        let home = temp_home("damaged-first-install");
        let staged = home.join("resources.staged");
        write_prepared_tree(&staged, "interrupted installation").unwrap();
        fs::remove_file(staged.join("items.json")).unwrap();

        assert_eq!(recover(&home).unwrap(), None);
        assert!(!home.join("resources").exists());
        assert!(!staged.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_leftover_scratch_does_not_bury_a_good_tree() {
        let home = temp_home("stale-scratch");
        let dest = home.join("resources");
        write_prepared_tree(&dest, "active installation").unwrap();
        write_prepared_tree(&home.join("resources.staged"), "unpublished installation").unwrap();
        write_prepared_tree(&home.join("resources.previous"), "older installation").unwrap();
        fs::create_dir_all(home.join("resources.unpacking")).unwrap();

        assert!(recover(&home).unwrap().is_none());
        assert!(!home.join("resources.unpacking").exists());
        assert!(!home.join("resources.staged").exists());
        assert!(!home.join("resources.previous").exists());
        assert_eq!(
            fs::read_to_string(dest.join("payload.txt")).unwrap(),
            "active installation"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn replacing_the_game_files_moves_the_old_tree_out_afterwards() {
        let home = temp_home("replace");
        fs::create_dir_all(home.join("resources")).unwrap();
        fs::write(home.join("resources/old.json"), "old").unwrap();
        let archive = home.join("horizon.zip");
        write_archive(&archive, &prepared());
        Pack::open(&archive).unwrap().install(&home).unwrap();
        assert!(home.join("resources/horizon-resources.json").is_file());
        assert!(!home.join("resources/old.json").exists());
        assert!(!home.join("resources.previous").exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn drop_folder_installs_archives_oldest_first_with_a_stable_tie_breaker() {
        use std::time::{Duration, UNIX_EPOCH};

        let scratch = tempfile::tempdir().unwrap();
        for (name, seconds, source) in [
            ("C.ZIP", 300, "newest"),
            ("A.zip", 300, "same-time"),
            ("Z.zip", 200, "oldest"),
        ] {
            let path = scratch.path().join(name);
            let mut entries = prepared();
            entries.push(("source.txt", source.into()));
            write_archive(&path, &entries);
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + seconds)),
                )
                .unwrap();
        }
        let ordered = manual_zips_in_import_order(scratch.path());
        assert_eq!(
            ordered
                .iter()
                .map(|path| path.file_name().unwrap())
                .collect::<Vec<_>>(),
            ["Z.zip", "A.zip", "C.ZIP"]
        );
        assert_eq!(scan(scratch.path()).len(), 3);
        assert_eq!(
            fs::read(scratch.path().join("resources/source.txt")).unwrap(),
            b"newest"
        );
    }

    #[test]
    fn startup_scan_does_not_consume_automatic_download_cache_archives() {
        let mut failures = Vec::new();
        for (index, (name, marker)) in [
            ("horizon-download.zip", "level.dat"),
            (
                "horizon-download-00000000-0000-4000-8000-000000000001.zip",
                "level.dat",
            ),
            ("HORIZON-DOWNLOAD.ZIP", "horizon-resources.json"),
            (
                "horizon-download-ABCDEFAB-0000-4ABC-8ABC-ABCDEFABCDEF.ZiP",
                "horizon-resources.json",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let home = temp_home(&format!("owned-cache-{index}"));
            let cache = home.join(name);
            let body = if marker == "level.dat" {
                "cached world fixture"
            } else {
                r#"{"schema":7}"#
            };
            write_archive(&cache, &[(marker, body)]);
            let automatic = scan(&home);
            let untouched =
                cache.is_file() && !home.join("saves").exists() && !home.join("resources").exists();
            write_archive(
                &home.join("horizon-download-my-world.zip"),
                &[("level.dat", "manual world fixture")],
            );
            let manual = scan(&home);
            let manual_ok = manual.len() == 1 && manual[0].kind == Kind::World;
            fs::remove_dir_all(&home).unwrap();
            if !automatic.is_empty() || !untouched || !manual_ok {
                failures.push(format!("{name}: automatic={}, cache untouched={untouched}, manual accepted={manual_ok}", automatic.len()));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[test]
    fn anything_else_is_refused_without_touching_the_home() {
        let home = temp_home("unknown");
        let archive = home.join("holiday-photos.zip");
        write_archive(&archive, &[("beach.jpg", "x"), ("sunset.jpg", "y")]);
        let error = classify(&archive).unwrap_err().to_string();
        assert!(error.contains("not Horizon's game files"), "{error}");
        assert_eq!(scan(&home).len(), 0);
        // Somebody else's files are not ours to rename or delete.
        assert!(archive.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_archive_that_is_ours_but_will_not_unpack_is_parked() {
        let home = temp_home("parked");
        let archive = home.join("horizon.zip");
        write_archive(
            &archive,
            &[
                ("horizon-resources.json", "{}"),
                (
                    "../outside.txt",
                    "written beside the archive, not inside it",
                ),
            ],
        );
        let error = Pack::open(&archive)
            .unwrap()
            .install(&home)
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes"), "{error}");
        assert!(!home.join("resources").exists());
        assert!(!home.join("outside.txt").exists());
        let first_failure = home.join("horizon.zip.failed");
        fs::write(&first_failure, b"earlier archive").unwrap();
        let original = fs::read(&archive).unwrap();
        // The next launch must not fail in the same place all over again, so it is put aside.
        assert!(scan(&home).is_empty());
        assert!(!archive.exists());
        assert_eq!(fs::read(first_failure).unwrap(), b"earlier archive");
        assert_eq!(
            fs::read(home.join("horizon.zip (2).failed")).unwrap(),
            original
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn failed_export_preserves_an_existing_archive() {
        let home = temp_home("failed-export");
        let source = home.join("World");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("level.dat"), "world metadata").unwrap();
        std::os::unix::fs::symlink("missing", source.join("bad-link")).unwrap();
        let archive = home.join("backup.zip");
        fs::write(&archive, "previous good backup").unwrap();
        assert!(pack(&source, &archive, |_, _| {}).is_err());
        assert_eq!(
            fs::read_to_string(&archive).unwrap(),
            "previous good backup"
        );
        assert_eq!(
            fs::read_dir(&home).unwrap().count(),
            2,
            "temporary export leaked"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn export_does_not_overwrite_source_payload_files() {
        let home = temp_home("export-source-payload");
        let source = home.join("World");
        fs::create_dir_all(source.join("region")).unwrap();
        fs::write(source.join("level.dat"), "world metadata").unwrap();
        let region = source.join("region/r.0.0.mca");
        fs::write(&region, "original chunk data").unwrap();
        assert!(pack(&source, &region, |_, _| {}).is_err());
        assert_eq!(fs::read_to_string(&region).unwrap(), "original chunk data");
        fs::create_dir_all(source.join("resourcepacks")).unwrap();
        let resource_pack = source.join("resourcepacks/look.zip");
        write_archive(&resource_pack, &[("pack.mcmeta", "{}")]);
        let original = fs::read(&resource_pack).unwrap();
        assert!(pack(&source, &resource_pack, |_, _| {}).is_err());
        assert_eq!(fs::read(&resource_pack).unwrap(), original);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn export_inside_source_excludes_its_own_output() {
        let home = temp_home("nested-export");
        let source = home.join("World");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("level.dat"), "world metadata").unwrap();
        let archive = source.join("backup.zip");
        for _ in 0..2 {
            let exported = pack(&source, &archive, |_, _| {}).unwrap();
            assert_eq!(exported.files, 1, "export included its own ZIP");
            let mut zip = ZipArchive::new(File::open(&archive).unwrap()).unwrap();
            assert_eq!(zip.len(), 1);
            assert_eq!(zip.by_index(0).unwrap().name().unwrap(), "World/level.dat");
        }
        if source.join("BACKUP.zip").exists() {
            assert_eq!(
                pack(&source, &source.join("BACKUP.zip"), |_, _| {})
                    .unwrap()
                    .files,
                1
            );
        }
        assert!(pack(&source, &source.join("level.dat"), |_, _| {}).is_err());
        assert_eq!(
            fs::read_to_string(source.join("level.dat")).unwrap(),
            "world metadata"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    #[ignore = "daily"]
    fn export_writes_a_world_with_a_zip64_entry() {
        let scratch = tempfile::tempdir().unwrap();
        let world = scratch.path().join("World");
        fs::create_dir(&world).unwrap();
        let metadata = b"world metadata";
        fs::write(world.join("level.dat"), metadata).unwrap();
        let size = u64::from(u32::MAX) + 1;
        File::create(world.join("large.bin"))
            .unwrap()
            .set_len(size)
            .unwrap();
        let output = scratch.path().join("export.zip");

        let exported = pack(&world, &output, |_, _| {}).unwrap();
        assert_eq!(exported.kind, Kind::World);
        assert_eq!(exported.bytes, size + metadata.len() as u64);
        let mut zip = ZipArchive::new(File::open(output).unwrap()).unwrap();
        let entry = zip.by_name("World/large.bin").unwrap();
        assert_eq!(entry.size(), size);
        assert!(entry.compressed_size() < size);
    }

    #[test]
    fn dot_components_are_normalized_when_detecting_and_installing_a_world() {
        let scratch = tempfile::tempdir().unwrap();
        let archive = scratch.path().join("world.zip");
        write_archive(
            &archive,
            &[
                ("./Outer/./World/level.dat", "metadata"),
                ("./Outer/World/region//r.0.0.mca", "chunks"),
                ("./__MACOSX/junk", "ignored"),
            ],
        );
        let pack = Pack::open(&archive).unwrap();
        assert_eq!(pack.kind, Kind::World);
        let world = pack.install(&scratch.path().join("home")).unwrap().path;
        assert_eq!(fs::read(world.join("level.dat")).unwrap(), b"metadata");
        assert_eq!(fs::read(world.join("region/r.0.0.mca")).unwrap(), b"chunks");
    }

    #[test]
    fn a_pack_with_dot_components_is_rewritten_even_without_a_wrapper() {
        let scratch = tempfile::tempdir().unwrap();
        let archive = scratch.path().join("pack.zip");
        write_archive(
            &archive,
            &[
                ("./pack.mcmeta", "metadata"),
                ("assets/./test/data.txt", "asset"),
            ],
        );
        let pack = Pack::open(&archive).unwrap();
        assert_eq!(pack.kind, Kind::ResourcePack);
        let installed = pack.install(&scratch.path().join("home")).unwrap().path;
        let mut zip = ZipArchive::new(File::open(installed).unwrap()).unwrap();
        assert!(zip.by_name("pack.mcmeta").is_ok());
        assert!(zip.by_name("assets/test/data.txt").is_ok());
    }

    #[test]
    fn archive_detection_does_not_ignore_files_outside_a_nested_world() {
        let scratch = tempfile::tempdir().unwrap();
        let archive = scratch.path().join("world.zip");
        for outside in ["notes.txt", "Outer/notes.txt"] {
            write_archive(
                &archive,
                &[("Outer/World/level.dat", "metadata"), (outside, "notes")],
            );
            assert!(Pack::open(&archive).is_err(), "{outside} is outside the world");
        }

        write_archive(
            &archive,
            &[
                ("Outer/World/level.dat", "metadata"),
                ("Outer/World/region/r.0.0.mca", "chunks"),
                ("Outer/.DS_Store", "ignored"),
            ],
        );
        let pack = Pack::open(&archive).unwrap();
        assert_eq!(pack.kind, Kind::World);
        assert_eq!(pack.prefix, "Outer/World/");
    }

    #[test]
    fn world_with_only_recovery_metadata_round_trips_through_zip() {
        let scratch = tempfile::tempdir().unwrap();
        let world = scratch.path().join("Recovered World");
        fs::create_dir(&world).unwrap();
        fs::write(world.join("level.dat_old"), b"recovery metadata").unwrap();
        let archive = scratch.path().join("backup.zip");
        let exported = pack(&world, &archive, |_, _| {}).unwrap();
        assert_eq!(exported.kind, Kind::World);
        let opened = Pack::open(&archive).unwrap();
        assert_eq!(opened.kind, Kind::World);
        assert_eq!(opened.item, "Recovered World");
        let installed = opened.install(&scratch.path().join("restored")).unwrap();
        assert_eq!(
            fs::read(installed.path.join("level.dat_old")).unwrap(),
            b"recovery metadata"
        );
        assert!(!installed.path.join("level.dat").exists());
    }

    #[test]
    fn a_real_folder_round_trips_through_the_archive() {
        let home = temp_home("round");
        let source = home.join("Horizon-26.3");
        fs::create_dir_all(source.join("assets/minecraft/models")).unwrap();
        for (name, body) in prepared_entries() {
            fs::write(source.join(name), body).unwrap();
        }
        fs::write(source.join("assets/minecraft/models/block.json"), "{}").unwrap();
        fs::write(source.join("session.lock"), "someone is inside").unwrap();
        let archive = home.join("out.zip");
        let packed = pack(&source, &archive, |_, _| {}).unwrap();
        assert_eq!(packed.kind, Kind::Resources);
        assert_eq!(packed.files, prepared_entries().len() + 1);
        fs::remove_dir_all(&source).unwrap();
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();
        assert_eq!(installed.files, prepared_entries().len() + 1);
        assert!(home.join("resources/horizon-resources.json").is_file());
        assert!(
            home.join("resources/assets/minecraft/models/block.json")
                .is_file()
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_manifest_directory_does_not_hide_a_valid_imported_root() {
        let root = temp_home("root-file-type");
        let sibling = root.join("bundle");
        let home = root.join("Documents");
        let imported = home.join("resources");
        fs::create_dir_all(imported.join("horizon-resources.json")).unwrap();
        let invalid_imported = resources_root(&sibling, Some(&home));
        fs::remove_dir(imported.join("horizon-resources.json")).unwrap();
        fs::write(imported.join("horizon-resources.json"), "{}").unwrap();
        fs::create_dir_all(sibling.join("horizon-resources.json")).unwrap();
        let selected = resources_root(&sibling, Some(&home));
        fs::remove_dir(sibling.join("horizon-resources.json")).unwrap();
        fs::write(sibling.join("horizon-resources.json"), "{}").unwrap();
        assert_eq!(resources_root(&sibling, Some(&home)), Some(sibling.clone()));
        #[cfg(unix)]
        {
            let marker = sibling.join("horizon-resources.json");
            fs::remove_file(&marker).unwrap();
            std::os::unix::fs::symlink(imported.join("horizon-resources.json"), &marker).unwrap();
            assert_eq!(resources_root(&sibling, Some(&home)), Some(sibling.clone()));
            fs::remove_file(imported.join("horizon-resources.json")).unwrap();
            assert_eq!(resources_root(&sibling, Some(&home)), None);
            fs::write(imported.join("horizon-resources.json"), "{}").unwrap();
            fs::remove_file(&marker).unwrap();
            std::os::unix::fs::symlink(&imported, &marker).unwrap();
            assert_eq!(
                resources_root(&sibling, Some(&home)),
                Some(imported.clone())
            );
        }
        fs::remove_dir_all(&root).unwrap();
        assert!(
            invalid_imported.is_none() && selected == Some(imported),
            "directory markers selected: imported={invalid_imported:?}, bundle={selected:?}"
        );
    }

    #[test]
    fn the_phone_reads_the_folder_an_import_wrote() {
        let dir = temp_home("root");
        let sibling = dir.join("bundle");
        let home = dir.join("Documents");
        fs::create_dir_all(&home).unwrap();
        // Nothing prepared anywhere: the caller's own fallback, and nothing from here.
        assert_eq!(resources_root(&sibling, None), None);
        assert_eq!(resources_root(&sibling, Some(&home)), None);
        fs::create_dir_all(home.join("resources")).unwrap();
        fs::write(home.join("resources/horizon-resources.json"), "{}").unwrap();
        assert_eq!(
            resources_root(&sibling, Some(&home)),
            Some(home.join("resources"))
        );
        // A desktop never looks there: its working directory is a checkout, not a sandbox.
        assert_eq!(resources_root(&sibling, None), None);
        // Assets shipped in the bundle win, as they always did.
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("horizon-resources.json"), "{}").unwrap();
        assert_eq!(resources_root(&sibling, Some(&home)), Some(sibling));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_keeps_a_successful_repair_when_the_bundled_root_is_damaged() {
        let root = tempfile::tempdir().unwrap();
        let sibling = root.path().join("bundle");
        let home = root.path().join("Documents");
        fs::create_dir(&sibling).unwrap();
        fs::create_dir(&home).unwrap();
        for (name, contents) in prepared_entries() {
            fs::write(sibling.join(name), contents).unwrap();
        }
        let archive = root.path().join("repair.zip");
        write_archive(&archive, &prepared());
        let installed = Pack::open(&archive).unwrap().install(&home).unwrap();

        assert_eq!(resources_root(&sibling, Some(&home)), Some(sibling.clone()));
        fs::write(sibling.join("facts.json"), "").unwrap();
        assert_eq!(
            resources_root(&sibling, Some(&home)),
            Some(installed.path.clone())
        );
        fs::remove_file(installed.path.join("items.json")).unwrap();
        assert_eq!(resources_root(&sibling, Some(&home)), Some(sibling));
    }

    #[test]
    fn an_archive_dropped_in_documents_is_where_the_game_then_looks() {
        let dir = temp_home("usb");
        let home = dir.join("Documents");
        fs::create_dir_all(&home).unwrap();
        write_archive(&home.join("Horizon-26.3.ZIP"), &prepared());
        assert_eq!(scan(&home).len(), 1);
        assert_eq!(
            resources_root(&dir.join("Horizon.app/resources"), Some(&home)).unwrap(),
            home.join("resources")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_are_labels_not_paths() {
        assert_eq!(sanitize("../../etc"), "etc");
        assert_eq!(sanitize(".."), "imported");
        assert_eq!(sanitize(" My World 26.3 "), "My World 26.3");
        // An escape sequence is dropped with the brackets: what is left cannot paint a terminal.
        assert_eq!(sanitize("o\u{1b}[2J"), "o2J");
        assert_eq!(sanitize(&"a".repeat(100)).len(), 64);
        assert_eq!(sanitize(&"𐐀".repeat(64)).len(), 236);
    }
}
