//! Known-pack negotiation uses a bounded verified local snapshot; peer IDs only index it.
use horizon_core::nbt::Tag;
use horizon_protocol::packets::configuration as cfg;
use std::path::Path;

/// A verified snapshot of the prepared core registry data. Server-supplied
/// identifiers only index this map; they never become filesystem paths.
pub(super) struct KnownCore(pub(super) std::collections::HashMap<(String, String), Tag>);

impl KnownCore {
    pub(super) fn load(resources: &Path) -> Option<Self> {
        let root = std::fs::canonicalize(resources).ok()?;
        let mut budget = 64 * 1024 * 1024;
        let manifest = known_json(&root, Path::new("horizon-resources.json"), &mut budget)?;
        if manifest.get("minecraft")?.as_str()? != horizon_protocol::GAME_VERSION {
            return None;
        }
        let catalog = known_json(&root, Path::new("command-registries.json"), &mut budget)?;
        let catalog = catalog.get("by_registry")?.as_object()?;
        let mut entries = std::collections::HashMap::new();
        for &registry in crate::lan::data::SYNCHRONIZED {
            let registry_path = core_identifier_path(registry)?;
            let ids = catalog.get(registry)?.get("ids")?.as_array()?;
            for id in ids {
                let id = id.as_str()?;
                let entry_path = core_identifier_path(id)?;
                let relative = Path::new("data/minecraft")
                    .join(registry_path)
                    .join(format!("{entry_path}.json"));
                let json = known_json(&root, &relative, &mut budget)?;
                if !json.is_object() && !json.is_array() {
                    return None;
                }
                let data = crate::lan::data::json_to_nbt(&json);
                if entries
                    .insert((registry.to_owned(), id.to_owned()), data)
                    .is_some()
                {
                    return None;
                }
            }
        }
        // Empty/partial prepared trees must not advertise a pack they cannot supply.
        if entries.is_empty() {
            return None;
        }
        Some(Self(entries))
    }
}

fn valid_resource_segment(s: &str, allow_slash: bool) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains("//")
        && s.split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && s.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'_' | b'-' | b'.')
                || (allow_slash && b == b'/')
        })
}

/// ResourceLocation's allowed characters alone do not make a safe path:
/// it permits dots and slashes, but file conversion rejects empty, . and .. components.
pub(super) fn core_identifier_path(id: &str) -> Option<&str> {
    let path = id.strip_prefix("minecraft:")?;
    if !valid_resource_segment(path, true) {
        return None;
    }
    (!path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        }))
    .then_some(path)
}

/// Canonical containment is checked before opening, including symlink targets.
/// Both a per-file cap and aggregate budget bound the pre-negotiation snapshot.
pub(super) fn known_json(
    root: &Path,
    relative: &Path,
    budget: &mut usize,
) -> Option<serde_json::Value> {
    use std::io::Read;
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let path = std::fs::canonicalize(root.join(relative)).ok()?;
    if !path.starts_with(root) {
        return None;
    }
    let metadata = std::fs::metadata(&path).ok()?;
    let limit = (*budget).min(16 * 1024 * 1024);
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > limit {
        return None;
    }
    *budget -= bytes.len();
    serde_json::from_slice(&bytes).ok()
}

/// Only acknowledge the exact core pack after verifying all locally expected
/// registry entries. Otherwise the server sends their full data, as before.
pub(super) fn select_known_packs(
    offered: Vec<cfg::KnownPack>,
    resources: &Path,
) -> (Vec<cfg::KnownPack>, Option<KnownCore>) {
    let (namespace, id, version) = crate::lan::data::CORE_PACK;
    let selected: Vec<_> = offered
        .into_iter()
        .filter(|p| p.namespace == namespace && p.id == id && p.version == version)
        .collect();
    if selected.is_empty() {
        return (Vec::new(), None);
    }
    match KnownCore::load(resources) {
        Some(core) => (selected, Some(core)),
        None => (Vec::new(), None),
    }
}

/// Omitted data is legal only for an entry in the pack actually selected in
/// this configuration phase. Preserve complete server-supplied data unchanged.
pub(super) fn fill_known_entries(
    r: &mut cfg::RegistryData,
    core: Option<&KnownCore>,
) -> Result<(), String> {
    for entry in r.entries.iter_mut().filter(|e| e.data.is_none()) {
        let data = core
            .and_then(|core| core.0.get(&(r.registry.clone(), entry.id.clone())))
            .ok_or_else(|| {
                "Server omitted registry data not supplied by a selected known pack".to_owned()
            })?;
        entry.data = Some(data.clone());
    }
    Ok(())
}
