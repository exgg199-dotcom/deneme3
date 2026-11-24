//! `.hpack`, a Horizon modpack (docs/modding/src/reference/registry-index.md): registry mods
//! played together as a profile. A zip holding `horizon-modpack.toml`, which
//! names each mod by id, version and SHA-256 (the files themselves come from
//! the registry), and optionally `overrides/`: settings and resource packs
//! copied into the profile's game folder.
use crate::manifest::STORAGE_DIR;
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// The manifest's file name in a `.hpack`.
pub const MANIFEST: &str = "horizon-modpack.toml";
/// The manifest formats this build reads.
pub const FORMAT: u32 = 1;
const OVERRIDES: &str = "overrides/";
const MAX_MANIFEST: u64 = 1 << 20;
/// Bytes per override file and in all, unpacked.
const MAX_OVERRIDE: u64 = 64 << 20;
const MAX_OVERRIDES: u64 = 256 << 20;
/// The settings files an override may replace.
const SETTINGS: [&str; 3] = ["options.txt", "horizon-graphics.txt", "horizon-extras.txt"];

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    /// `[a-z0-9_]{1,64}`: its profile's folder.
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    /// The Minecraft version it is for.
    pub game: String,
    #[serde(default)]
    pub mods: Vec<PackMod>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PackMod {
    pub id: String,
    pub version: String,
    pub sha256: String,
    /// Turned on in the profile (`horizon-mods.txt`).
    #[serde(default = "on")]
    pub on: bool,
}

fn on() -> bool {
    true
}

#[cfg(feature = "index")]
impl PackMod {
    pub fn requirement(&self) -> crate::index::Requirement {
        crate::index::Requirement {
            id: self.id.clone(),
            version: self.version.clone(),
            sha256: self.sha256.clone(),
        }
    }
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let m: Manifest = toml::from_str(text).map_err(|e| format!("{MANIFEST}: {e}"))?;
        if m.format != FORMAT {
            return Err(format!(
                "{MANIFEST}: format {}; this Horizon reads {FORMAT}",
                m.format
            ));
        }
        let id_ok = |id: &str| {
            (1..=64).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        if !id_ok(&m.id) {
            return Err(format!(
                "{MANIFEST}: id {:?} is not [a-z0-9_]{{1,64}}",
                m.id
            ));
        }
        for (i, pm) in m.mods.iter().enumerate() {
            let hex = pm.sha256.len() == 64
                && pm
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !id_ok(&pm.id) || !hex || pm.version.is_empty() {
                return Err(format!(
                    "{MANIFEST}: mod {:?} needs an id, a version and a lowercase hex sha256",
                    pm.id
                ));
            }
            if m.mods[..i].iter().any(|other| other.id == pm.id) {
                return Err(format!("{MANIFEST}: mod {} is listed twice", pm.id));
            }
        }
        Ok(m)
    }
}

/// A `.hpack` as read from disk: its manifest and its override files, by
/// path under `overrides/`.
#[derive(Debug)]
pub struct Modpack {
    pub manifest: Manifest,
    pub overrides: Vec<String>,
}

impl Modpack {
    pub fn read(path: &Path) -> Result<Modpack, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        let mut text = String::new();
        let entry = zip
            .by_name(MANIFEST)
            .map_err(|_| format!("{MANIFEST}: missing"))?;
        if entry.size() > MAX_MANIFEST {
            return Err(format!("{MANIFEST}: exceeds 1 MiB"));
        }
        entry
            .take(MAX_MANIFEST + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("{MANIFEST}: {e}"))?;
        if text.len() as u64 > MAX_MANIFEST {
            return Err(format!("{MANIFEST}: exceeds 1 MiB"));
        }
        let manifest = Manifest::parse(&text)?;
        let (mut overrides, mut total) = (Vec::new(), 0u64);
        for i in 0..zip.len() {
            let entry = zip.by_index_raw(i).map_err(|e| e.to_string())?;
            let name = entry.name().map_err(|e| e.to_string())?.into_owned();
            if name == MANIFEST || entry.is_dir() {
                continue;
            }
            let Some(rest) = name.strip_prefix(OVERRIDES) else {
                return Err(format!(
                    "{name}: a modpack holds only {MANIFEST} and overrides/"
                ));
            };
            check_override(rest, &manifest)?;
            if entry.size() > MAX_OVERRIDE {
                return Err(format!("{name} is over {} MiB", MAX_OVERRIDE >> 20));
            }
            total += entry.size();
            overrides.push(rest.to_owned());
        }
        if total > MAX_OVERRIDES {
            return Err(format!("overrides are over {} MiB", MAX_OVERRIDES >> 20));
        }
        overrides.sort();
        Ok(Modpack {
            manifest,
            overrides,
        })
    }

    /// Writes the overrides of the pack at `path` into the game folder
    /// `home`, each checked again.
    pub fn extract_overrides(path: &Path, home: &Path) -> Result<Vec<PathBuf>, String> {
        let pack = Modpack::read(path)?;
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        // Inflate and CRC-check every entry before touching profile files.
        std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let staging = loop {
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = home.join(format!(".hpack-install-{}-{id}", std::process::id()));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        };
        let mut rollback_failed = false;
        let result = (|| {
            let mut total = 0;
            let incoming = staging.join("incoming");
            for name in &pack.overrides {
                Self::extract_one(&mut zip, name, &incoming, &mut total)?;
            }
            let mut published: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
            for (index, name) in pack.overrides.iter().enumerate() {
                let target = home.join(name);
                let backup = staging.join(format!("backup-{index}"));
                let mut parked = false;
                let installed = (|| -> std::io::Result<()> {
                    // Archive paths are plain, but a profile may already contain
                    // links. Never follow one to overwrite files outside it.
                    let mut parent = home.to_path_buf();
                    let components: Vec<_> = Path::new(name).components().collect();
                    for component in &components[..components.len() - 1] {
                        parent.push(component.as_os_str());
                        match std::fs::symlink_metadata(&parent) {
                            Ok(metadata) if metadata.file_type().is_dir() => {}
                            Ok(_) => {
                                return Err(std::io::Error::other(
                                    "override parent is not a regular directory",
                                ));
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                std::fs::create_dir(&parent)?
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    if std::fs::symlink_metadata(&target).is_ok() {
                        if !std::fs::symlink_metadata(&target)?.file_type().is_file() {
                            return Err(std::io::Error::other(
                                "override destination is not a regular file",
                            ));
                        }
                        std::fs::rename(&target, &backup)?;
                        parked = true;
                    }
                    std::fs::rename(incoming.join(name), &target)
                })();
                if let Err(error) = installed {
                    let mut rollback_errors = Vec::new();
                    if parked && let Err(e) = std::fs::rename(&backup, &target) {
                        rollback_errors.push(e.to_string());
                    }
                    for (target, backup) in published.iter().rev() {
                        if let Err(e) = std::fs::remove_file(target) {
                            rollback_errors.push(e.to_string());
                        }
                        if let Some(backup) = backup
                            && let Err(e) = std::fs::rename(backup, target)
                        {
                            rollback_errors.push(e.to_string());
                        }
                    }
                    if rollback_errors.is_empty() {
                        return Err(format!("{name}: {error}"));
                    }
                    // Keep backups if filesystem failure also prevented rollback.
                    rollback_failed = true;
                    return Err(format!(
                        "{name}: {error}; restore incomplete; backups at {}: {}",
                        staging.display(),
                        rollback_errors.join("; ")
                    ));
                }
                published.push((target, parked.then_some(backup)));
            }
            Ok(published.into_iter().map(|(target, _)| target).collect())
        })();
        // A backup still present after a failed rollback must remain recoverable.
        if !rollback_failed {
            let _ = std::fs::remove_dir_all(&staging);
        }
        result
    }

    fn extract_one(
        zip: &mut zip::ZipArchive<std::fs::File>,
        name: &str,
        home: &Path,
        total: &mut u64,
    ) -> Result<PathBuf, String> {
        let mut entry = zip
            .by_name(&format!("{OVERRIDES}{name}"))
            .map_err(|e| format!("{name}: {e}"))?;
        let target = home.join(name);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut out =
            std::fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        let copied = std::io::copy(&mut (&mut entry).take(MAX_OVERRIDE + 1), &mut out)
            .map_err(|e| format!("{}: {e}", target.display()))?;
        *total += copied;
        if copied > MAX_OVERRIDE || *total > MAX_OVERRIDES {
            let _ = std::fs::remove_file(&target);
            return Err(if copied > MAX_OVERRIDE {
                format!("{name} is over {} MiB", MAX_OVERRIDE >> 20)
            } else {
                format!("overrides are over {} MiB", MAX_OVERRIDES >> 20)
            });
        }
        Ok(target)
    }
}

/// An override's path under `overrides/`: a settings file, a resource pack
/// zip, or the stored settings of one of the pack's mods. Plain parts only.
fn check_override(path: &str, manifest: &Manifest) -> Result<(), String> {
    let bad = || {
        format!(
            "overrides/{path}: only {}, resourcepacks/<name>.zip and horizon-mods/{STORAGE_DIR}/<a listed mod>/ may be overridden",
            SETTINGS.join(", ")
        )
    };
    let parts: Vec<&str> = path.split('/').collect();
    let plain = |p: &&str| {
        !p.is_empty()
            && !p.starts_with('.')
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-+ ".contains(&b))
    };
    let ok = match parts.as_slice() {
        [file] => SETTINGS.contains(file),
        ["resourcepacks", name] => plain(name) && name.ends_with(".zip"),
        ["horizon-mods", dir, id, rest @ ..] => {
            *dir == STORAGE_DIR
                && manifest.mods.iter().any(|m| m.id == *id)
                && !rest.is_empty()
                && rest.iter().all(plain)
        }
        _ => false,
    };
    if ok { Ok(()) } else { Err(bad()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const PACK: &str = r#"
format = 1
id = "cozy"
name = "Cozy"
version = "1.0.0"
game = "26.3"
[[mods]]
id = "coords_hud"
version = "0.1.0"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

    fn write_pack(dir: &Path, manifest: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("cozy.hpack");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file(MANIFEST, options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        for (name, bytes) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("horizon-hpack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_a_pack_and_writes_its_overrides() {
        let dir = scratch("read");
        let storage = format!("overrides/horizon-mods/{STORAGE_DIR}/coords_hud/corner.txt");
        let path = write_pack(
            &dir,
            PACK,
            &[
                ("overrides/options.txt", b"renderDistance:8\n"),
                ("overrides/resourcepacks/Cozy Look.zip", b"zip"),
                (&storage, b"top-left"),
            ],
        );
        let pack = Modpack::read(&path).unwrap();
        assert_eq!(pack.manifest.mods[0].id, "coords_hud");
        assert!(pack.manifest.mods[0].on);
        assert_eq!(pack.overrides.len(), 3);
        let home = dir.join("home");
        Modpack::extract_overrides(&path, &home).unwrap();
        let options = std::fs::read_to_string(home.join("options.txt")).unwrap();
        assert_eq!(options, "renderDistance:8\n");
        let corner = home.join(format!("horizon-mods/{STORAGE_DIR}/coords_hud/corner.txt"));
        assert!(corner.is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn override_publication_preserves_old_files_on_failure_and_replaces_on_success() {
        for blocked in [false, true] {
            let dir = scratch(if blocked { "rollback" } else { "replace" });
            let path = write_pack(
                &dir,
                PACK,
                &[
                    ("overrides/horizon-graphics.txt", b"new graphics"),
                    ("overrides/options.txt", b"new options"),
                ],
            );
            let home = dir.join("home");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("horizon-graphics.txt"), b"old graphics").unwrap();
            if blocked {
                std::fs::create_dir(home.join("options.txt")).unwrap();
            } else {
                std::fs::write(home.join("options.txt"), b"old options").unwrap();
            }
            let result = Modpack::extract_overrides(&path, &home);
            assert_eq!(result.is_ok(), !blocked);
            assert_eq!(
                std::fs::read(home.join("horizon-graphics.txt")).unwrap(),
                if blocked {
                    b"old graphics"
                } else {
                    b"new graphics"
                }
            );
            if blocked {
                assert!(home.join("options.txt").is_dir());
            } else {
                assert_eq!(
                    std::fs::read(home.join("options.txt")).unwrap(),
                    b"new options"
                );
            }
            assert_eq!(
                std::fs::read_dir(&home).unwrap().count(),
                2,
                "staging leaked"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    #[cfg(unix)]
    fn override_cannot_follow_a_profile_directory_link() {
        let dir = scratch("linked-override");
        let path = write_pack(
            &dir,
            PACK,
            &[("overrides/resourcepacks/look.zip", b"new pack")],
        );
        let home = dir.join("home");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("look.zip"), b"outside original").unwrap();
        std::os::unix::fs::symlink(&outside, home.join("resourcepacks")).unwrap();
        assert!(Modpack::extract_overrides(&path, &home).is_err());
        assert_eq!(
            std::fs::read(outside.join("look.zip")).unwrap(),
            b"outside original"
        );
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_override_keeps_existing_profile_files() {
        let dir = scratch("corrupt-existing");
        let payload = b"unique broken extra settings";
        let path = write_pack(
            &dir,
            PACK,
            &[
                ("overrides/options.txt", b"new options"),
                ("overrides/horizon-graphics.txt", payload),
            ],
        );
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes
            .windows(payload.len())
            .position(|b| b == payload)
            .unwrap();
        bytes[at] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("options.txt"), b"old options").unwrap();
        std::fs::write(home.join("horizon-graphics.txt"), b"old graphics").unwrap();
        assert!(Modpack::extract_overrides(&path, &home).is_err());
        assert_eq!(
            std::fs::read(home.join("options.txt")).unwrap(),
            b"old options"
        );
        assert_eq!(
            std::fs::read(home.join("horizon-graphics.txt")).unwrap(),
            b"old graphics"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_modpack_manifest_is_not_accepted_as_a_valid_prefix() {
        let dir = scratch("manifest-limit");
        let text = format!("{PACK}{}", " ".repeat(MAX_MANIFEST as usize - PACK.len()));
        let path = write_pack(&dir, &text, &[]);
        assert!(
            Modpack::read(&path).is_ok(),
            "exact-limit manifest rejected"
        );
        let text = format!("{text} ");
        let path = write_pack(&dir, &text, &[]);
        assert!(
            Modpack::read(&path).is_err(),
            "oversized manifest was silently truncated"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_what_a_pack_may_not_hold() {
        let dir = scratch("refuse");
        for name in [
            "mods/evil.jar",
            "overrides/../../.bashrc",
            "overrides/horizon-mods/evil.hmod",
            "overrides/resourcepacks/../options.txt",
            "overrides/servers.dat",
            "overrides/horizon-mods/.horizon-mod-data/other_mod/x",
            "overrides/resourcepacks/.hidden.zip",
        ] {
            let path = write_pack(&dir, PACK, &[(name, b"x")]);
            assert!(Modpack::read(&path).is_err(), "{name} was accepted");
        }
        let twice = format!("{PACK}{}", &PACK[PACK.find("[[mods]]").unwrap()..]);
        assert!(Manifest::parse(&twice).unwrap_err().contains("twice"));
        let newer = PACK.replace("format = 1", "format = 2");
        assert!(Manifest::parse(&newer).unwrap_err().contains("format 2"));
        let short = PACK.replace("0000000000000000", "00");
        assert!(Manifest::parse(&short).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
