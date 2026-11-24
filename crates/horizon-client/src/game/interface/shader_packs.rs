//! Shader packs: the one picked on Options > Horizon Settings > Shader Packs, or
//! `HORIZON_SHADER_PACK=<name>` (development; it wins over the pick), runs `shaderpacks/<name>`
//! (a folder or a zip beside `options.txt`) over the world; Horizon's own packs go to
//! `game/modding/native_shaders.rs`. The programs of the current dimension's folder are loaded when the
//! dimension changes (spec 1.2).

use horizon_client::camera::CameraFluid;
use horizon_client::gfx::pack_uniforms::{PackInputs, PlayerStatus};
use horizon_client::mod_list::ModList;
use horizon_client::shader_options::ShaderOptions;
use horizon_client::text::Text;
use horizon_shaders::{BlockIds, Macros, Pack, Properties, dimension_folder_named, load_post};

use super::native_shaders::{clear_refusal, fingerprint, outcome, remember};
use super::{Dim, Game};
use horizon_client_render::render_geometry::EntitySpan;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// The classic pack and its per-pack option file live beside each other.
pub(super) fn classic_options_path(pack: &Path) -> Option<PathBuf> {
    let name = pack.file_name()?.to_str()?;
    Some(pack.with_file_name(horizon_shaders::chosen_file(name)))
}

fn chosen_shader_options(pack: &Path) -> BTreeMap<String, String> {
    classic_options_path(pack)
        .and_then(|file| std::fs::read_to_string(file).ok())
        .map(|text| horizon_shaders::parse_chosen(&text))
        .unwrap_or_default()
}

/// Pack `name`'s options, screens and names in `language`, with the values the player chose.
pub(super) fn read_shader_options(
    name: &str,
    language: &str,
    macros: &Macros,
) -> Option<ShaderOptions> {
    // Native options come from the manifest; the shared screen opens either kind.
    if name.starts_with("horizon:") {
        return None;
    }
    let path = Game::shader_packs_dir().join(name);
    let pack = match Pack::open(&path) {
        Ok(pack) => pack,
        Err(e) => {
            log::warn!("shader pack {name}: {e}");
            return None;
        }
    };
    let options = horizon_shaders::options(&pack);
    let lang = horizon_shaders::Lang::load(&pack, language);
    let chosen = chosen_shader_options(&path);
    let pack = pack.with_chosen(chosen.clone());
    let text = pack.read_text("shaders.properties").unwrap_or_default();
    let properties = Properties::parse(&text, &horizon_shaders::properties_macros(&pack, macros));
    let screens = horizon_shaders::Screens::parse(&properties, &options);
    Some(ShaderOptions::new(
        name.to_owned(),
        options,
        screens,
        lang,
        chosen,
    ))
}

/// Saves the values chosen on the Shader Options page; none chosen removes the file.
fn write_shader_options(options: &ShaderOptions) -> std::io::Result<()> {
    let pack = Game::shader_packs_dir().join(&options.pack);
    let file = classic_options_path(&pack).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "Missing shader file name")
    })?;
    if options.chosen.is_empty() {
        match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    } else {
        let text = horizon_shaders::chosen_text(&options.chosen);
        horizon_client_core::config_file::write_atomic(&file, text.as_bytes())?;
    }
    clear_refusal(&pack);
    Ok(())
}

pub(super) fn persist_shader_options(
    settings: &mut horizon_client::settings::Settings,
    ui: &mut horizon_client::options_ui::OptionsUi,
) -> bool {
    let Some(options) = settings.shader_options.take() else {
        return false;
    };
    let saved = write_shader_options(&options);
    ui.shader_options_save_result(&options, saved.as_ref().err());
    if let Err(error) = saved {
        log::warn!(
            "shader pack {}: could not save settings: {error}",
            options.pack
        );
        settings.shader_options = Some(options);
        return false;
    }
    true
}

pub(super) fn persist_native_shader_options(
    settings: &mut horizon_client::settings::Settings,
    ui: &mut horizon_client::options_ui::OptionsUi,
    index: usize,
    previous: Vec<f64>,
) {
    let Some(pack) = settings.shader_packs.get_mut(index) else {
        return;
    };
    let saved = pack.save_values(&Game::shader_packs_dir());
    ui.native_shader_options_save_result(&pack.file, saved.as_ref().err());
    if let Err(error) = saved {
        log::warn!(
            "shader pack {}: could not save settings: {error}",
            pack.file
        );
        pack.values = previous;
    }
}

/// The pack that runs over the world ("": none): `HORIZON_SHADER_PACK`, else the pick
/// (`active_pack`).
pub(super) fn dev_pack() -> Option<&'static str> {
    static DEV_PACK: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DEV_PACK
        .get_or_init(|| std::env::var("HORIZON_SHADER_PACK").ok())
        .as_deref()
}

pub(super) fn picked_pack(settings: &horizon_client::settings::Settings) -> &str {
    active_pack(
        dev_pack().unwrap_or(&settings.horizon.shader_pack),
        &settings.mods,
    )
}

/// What a renderer readies for the pick, unless it was refused with no version of it left
/// drawing (`native_shaders::remember_look`), is missing or its owner is off.
#[derive(Clone, Copy, Default)]
pub(crate) struct PickedPack {
    /// It draws flat where the device can: the sections are laid out for it.
    pub draws_flat: bool,
    /// Its passes read the solid quads' vertices: they start wide.
    pub starts_wide: bool,
    /// It is one of Horizon's own packs, with its `[frame] blend`: the frame is float.
    pub linear: Option<horizon_shader_api::Blend>,
    /// It is a `shaders/` pack: it draws its SDR picture on the 8-bit frame, HDR output on or
    /// off. With no pack available or one of Horizon's own, the frame is the layered float one
    /// where HDR output is on (docs/design/hdr-linear-light.md, The HUD and menus; the renderer
    /// decides, `FrameRequest::shaders_pack`).
    pub shaders_pack: bool,
    /// Its manifest declares `[ray_tracing]`: the device asks for ray queries (spec #16011).
    pub ray_tracing: bool,
}
impl PickedPack {
    /// What it asks of the renderer's frame.
    pub(crate) fn frame(self) -> horizon_client::gfx::FrameRequest {
        horizon_client::gfx::FrameRequest {
            linear_pack: self.linear.is_some(),
            linear_blend: self.linear == Some(horizon_shader_api::Blend::Linear),
            ray_tracing: self.ray_tracing,
            shaders_pack: self.shaders_pack,
        }
    }
}

pub(super) fn picked_pack_shape(settings: &horizon_client::settings::Settings) -> PickedPack {
    let picked = picked_pack(settings);
    let dir = Game::shader_packs_dir();
    let path = if dev_pack().is_some() {
        dir.join(picked)
    } else {
        horizon_client::shader_pack_list::resolve(&dir, picked).1
    };
    if pack_owned(picked, &path, &settings.mods) && outcome(&path) != Some(false) {
        PickedPack::read(&path).unwrap_or_default()
    } else {
        PickedPack::default()
    }
}

impl PickedPack {
    /// Read the requirements of an installed pack. Incompatible native manifests
    /// request the default frame until the shader loader can report the problem.
    pub(super) fn read(path: &std::path::Path) -> Option<Self> {
        if !path.exists() {
            return None;
        }
        if !horizon_shader_api::package::is_pack(path) {
            return Some(Self {
                draws_flat: true,
                starts_wide: true,
                shaders_pack: true,
                ..Self::default()
            });
        }
        Some(horizon_shader_api::Pack::read_manifest(path).map_or_else(
            |_| Self::default(),
            |manifest| Self {
                draws_flat: manifest.shadow.is_some(),
                linear: Some(manifest.blend),
                ray_tracing: manifest.ray_tracing.is_some(),
                ..Self::default()
            },
        ))
    }
}

pub(super) fn pack_owned(picked: &str, path: &std::path::Path, mods: &ModList) -> bool {
    !picked.is_empty()
        && path
            .file_name()
            .is_none_or(|f| !active_pack(&f.to_string_lossy(), mods).is_empty())
}

// A native mod can bundle a pack as `native-<mod-id>`; its Mods toggle controls it too.
pub(super) fn active_pack<'a>(picked: &'a str, mods: &ModList) -> &'a str {
    if picked.strip_prefix("native-").is_some_and(|id| {
        mods.installed
            .iter()
            .any(|m| m.id == id && m.native.is_some() && mods.disabled.contains(id))
    }) {
        ""
    } else {
        picked
    }
}

/// A selected pack may read resource-pack material maps, whether native or GLSL.
pub(super) fn picks_material_pack(settings: &horizon_client::settings::Settings) -> bool {
    let dev = std::env::var("HORIZON_SHADER_PACK").ok();
    let picked = active_pack(
        dev.as_deref().unwrap_or(&settings.horizon.shader_pack),
        &settings.mods,
    );
    if picked.is_empty() {
        return false;
    }
    let dir = Game::shader_packs_dir();
    let path = if dev.is_some() {
        dir.join(picked)
    } else {
        horizon_client::shader_pack_list::resolve(&dir, picked).1
    };
    path.file_name()
        .is_none_or(|f| !active_pack(&f.to_string_lossy(), &settings.mods).is_empty())
}

use serde_json::{Value, json};

pub(super) const HELP: &[&str] =
    &["shader-pack [list|off|NAME]    list or select a shader pack in shaderpacks/"];
thread_local! {
    static REPORT: RefCell<Option<Value>> = const { RefCell::new(None) };
    // Automatic disable clears the selection and the live renderer report.
    // Keep the reason available to the launcher until the next explicit pick.
    static LAST_DISABLED: RefCell<Option<Value>> = const { RefCell::new(None) };
    static OPTIONS_REVISION: Cell<u64> = const { Cell::new(0) };
}

fn clear_shader_report() {
    REPORT.with_borrow_mut(Option::take);
}

fn clear_pending_shader_report(name: &str, dimension: i32) {
    REPORT.with_borrow_mut(|report| {
        if report.as_ref().is_some_and(|value| {
            value["pending"] == true && value["pack"] == name && value["dimension"] == dimension
        }) {
            *report = None;
        }
    });
}

fn shader_failure_notes(reason: &str, details: &[String]) -> Vec<String> {
    std::iter::once(reason)
        .filter(|reason| !reason.is_empty())
        .chain(details.iter().map(String::as_str).filter(|note| *note != reason))
        .map(str::to_owned)
        .collect()
}

pub(super) fn record_report(name: &str, dimension: i32, programs: &[String], notes: &[String]) {
    record_shader_report(name, dimension, programs, notes, false);
}

fn record_shader_report(
    name: &str,
    dimension: i32,
    programs: &[String],
    notes: &[String],
    pending: bool,
) {
    // The summary refusal alone cannot diagnose a pack: keep the parser/compiler
    // notes in the exported game log as well as the in-game report.
    for note in notes {
        log::info!("shader pack {name} (dimension {dimension}): {note}");
    }
    REPORT.with_borrow_mut(|report| {
        *report = Some(json!({
            "pack": name, "dimension": dimension, "programs": programs, "notes": notes,
            "pending": pending,
        }))
    });
}
pub(super) fn record_reload_error(
    name: &str,
    dimension: i32,
    errors: &[String],
    previous_active: bool,
) {
    let previous: Vec<String> = REPORT.with_borrow(|report| {
        report
            .as_ref()
            .filter(|r| previous_active && r["pack"].as_str() == Some(name))
            .and_then(|r| r["programs"].as_array())
            .map(|programs| {
                programs
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    });
    let mut notes = errors.to_vec();
    if !previous.is_empty() {
        notes.insert(
            0,
            "Reload failed; the previous shader programs remain active.".into(),
        );
    }
    record_report(name, dimension, &previous, &notes);
}
#[path = "shader_packs/selection.rs"]
mod selection;

fn failed_shader_report(key: &str, file: &str) -> bool {
    REPORT.with_borrow(|report| {
        report.as_ref().is_some_and(|report| {
            let name = report["pack"].as_str();
            (name == Some(key) || name == Some(file))
                && report["pending"] != true
                && report["programs"].as_array().is_some_and(Vec::is_empty)
        })
    })
}

/// Selecting a pack again is an explicit request to retry its cached failure.
pub(super) fn retry_selected_shader(settings: &horizon_client::settings::Settings) -> bool {
    let name = &settings.horizon.shader_pack;
    if name.is_empty() {
        return false;
    }
    let (key, path) = horizon_client::shader_pack_list::resolve(&Game::shader_packs_dir(), name);
    clear_refusal(&path)
        || failed_shader_report(
            &key,
            path.file_name()
                .and_then(|file| file.to_str())
                .unwrap_or(name),
        )
}

fn listed_pack<'a>(
    packs: &'a [horizon_client::shader_pack_list::Entry],
    name: &str,
) -> Option<&'a horizon_client::shader_pack_list::Entry> {
    packs
        .iter()
        .find(|pack| pack.key == name)
        .or_else(|| packs.iter().find(|pack| pack.file == name))
}

/// Runtime state for polling without opening any pack files.
fn shader_status(settings: &horizon_client::settings::Settings) -> Value {
    let picked = &settings.horizon.shader_pack;
    let selected_pack = listed_pack(&settings.shader_packs, picked);
    let selected = (!picked.is_empty())
        .then(|| selected_pack.map_or(picked.as_str(), |pack| pack.key.as_str()));
    let report = REPORT.with_borrow(|report| {
        report
            .as_ref()
            .filter(|r| {
                let name = r["pack"].as_str();
                name == selected
                    || selected_pack.is_some_and(|pack| name == Some(pack.file.as_str()))
            })
            .cloned()
    });
    let last_disabled = selected
        .is_none()
        .then(|| LAST_DISABLED.with_borrow(Clone::clone))
        .flatten();
    json!({"selected": selected, "runtime": report, "last_disabled": last_disabled})
}

pub(super) fn command(
    settings: &mut horizon_client::settings::Settings,
    words: &[&str],
    line: &str,
) -> horizon_client::Result<Option<(Value, bool)>> {
    let Some(verb @ ("shader-pack" | "shader-pack-set" | "shader-pack-status")) =
        words.first().copied()
    else {
        return Ok(None);
    };
    if verb == "shader-pack-status" {
        return Ok(Some((shader_status(settings), false)));
    }
    let root = std::env::current_dir()?;
    let directory = root.join("shaderpacks");
    let packs = horizon_client::shader_pack_list::scan(&directory);
    let arg = line.trim_start().strip_prefix(verb).unwrap_or("").trim();
    let find_pack = |name: &str| listed_pack(&packs, name);
    let canonical =
        |name: &str| find_pack(name).map_or_else(|| name.to_owned(), |pack| pack.key.clone());
    let current = canonical(&settings.horizon.shader_pack);
    let replacement = selection::change(verb, arg, &current, canonical)?;
    let mut changed = false;
    if let Some(name) = replacement {
        let pack = find_pack(&name);
        if !name.is_empty() && pack.is_none() {
            return Err(format!("no shader pack {name} in shaderpacks/").into());
        }
        let path = pack.map(|pack| directory.join(&pack.file));
        let save = verb == "shader-pack" || current != name;
        if save {
            settings
                .horizon
                .select_shader_pack(&root, name)
                .map_err(|error| format!("could not save the shader selection: {error}"))?;
        }
        let retried = path.as_deref().is_some_and(clear_refusal)
            || pack.is_some_and(|pack| failed_shader_report(&pack.key, &pack.file));
        changed = save || retried;
        if changed {
            LAST_DISABLED.with_borrow_mut(Option::take);
            REPORT.with_borrow_mut(Option::take);
        }
    }
    let list: Vec<Value> = packs
        .iter()
        .map(|pack| json!({"name": pack.file, "key": pack.key, "title": pack.title, "problem": null}))
        .collect();
    settings.shader_packs = packs;
    let mut response = shader_status(settings);
    response["packs"] = json!(list);
    Ok(Some((response, changed)))
}

/// Prefer the connection's current block tags, including a local integrated server's.
/// Only an absent synced registry falls back to the world's/base table: an empty synced
/// registry deliberately removes all tags.
fn resolve_block_tags(
    ids: &BlockIds,
    fallback: &HashMap<String, HashSet<String>>,
    synced: Option<(
        &horizon_protocol::registry::Builtin,
        &horizon_protocol::registry::Dynamic,
    )>,
) -> BlockIds {
    let tags = synced.and_then(|(builtin, dynamic)| {
        let names = builtin.get("minecraft:block")?;
        let tags = dynamic.tags.get("minecraft:block")?;
        Some(
            tags.iter()
                .map(|(tag, members)| {
                    (
                        tag.clone(),
                        members
                            .iter()
                            .filter_map(|&id| names.name(id))
                            .map(str::to_owned)
                            .collect(),
                    )
                })
                .collect(),
        )
    });
    ids.with_block_tags(tags.as_ref().unwrap_or(fallback))
}

#[cfg(test)]
mod block_tag_source_tests {
    use super::*;
    use horizon_protocol::registry::{Builtin, Dynamic, Ids};

    #[test]
    fn synced_block_tags_replace_world_fallback_for_every_connection() {
        let ids = BlockIds::parse(&Properties::parse("block.54=%sample", &Macros::standard()));
        let fallback = HashMap::from([(
            "minecraft:sample".into(),
            HashSet::from(["minecraft:oak_log".into()]),
        )]);
        let builtin = Builtin {
            registries: HashMap::from([(
                "minecraft:block".into(),
                Ids::from_names(vec![
                    "minecraft:oak_log".into(),
                    "minecraft:spruce_log".into(),
                ]),
            )]),
        };
        let mut dynamic = Dynamic::default();
        dynamic.tags.insert(
            "minecraft:block".into(),
            HashMap::from([("minecraft:sample".into(), vec![1, -1, 999])]),
        );
        let local = resolve_block_tags(&ids, &fallback, None);
        assert_eq!(local.id("minecraft:oak_log", |_| None), Some(54));
        // Integrated and external connections use this same synced source; unknown
        // network IDs are ignored instead of selecting an unrelated fallback member.
        let synced = resolve_block_tags(&ids, &fallback, Some((&builtin, &dynamic)));
        assert_eq!(synced.id("minecraft:oak_log", |_| None), None);
        assert_eq!(synced.id("minecraft:spruce_log", |_| None), Some(54));
        dynamic
            .tags
            .insert("minecraft:block".into(), HashMap::new());
        let empty = resolve_block_tags(&ids, &fallback, Some((&builtin, &dynamic)));
        assert_eq!(empty.id("minecraft:oak_log", |_| None), None);
        assert_eq!(empty.id("minecraft:spruce_log", |_| None), None);
        dynamic.tags.clear();
        let absent = resolve_block_tags(&ids, &fallback, Some((&builtin, &dynamic)));
        assert_eq!(absent.id("minecraft:oak_log", |_| None), Some(54));
    }
}

/// The ids a pack gives entities and block entities (4.1, 5.3), for the spans of the entity
/// geometry.
#[cfg_attr(test, derive(Default))]
pub(super) struct PackIds {
    entities: HashMap<String, i32>,
    blocks: BlockIds,
    /// `blockEntityId` by block state, filled as states are drawn.
    block_states: RefCell<HashMap<u32, i32>>,
    /// The id of a name the file lacks: 65535, or 0 without the file.
    missing_entity: i32,
    missing_block: i32,
    /// `item.properties` (4.1) and the id of a held item the file lacks, or of an empty hand:
    /// −1, or 0 without the file (5.3).
    items: HashMap<String, i32>,
    missing_item: i32,
    /// `currentRenderedItemId` of each block item (`StateFacts::item` of its block's default
    /// state): that state's `block.properties` id, or 0.
    block_items: HashMap<String, i32>,
    /// Whether the pack has `item.properties` (`currentRenderedItemId` of an item it lacks).
    item_file: bool,
    /// `oldHandLight` (3.2): the first held item and light are those of the brighter hand.
    old_hand_light: bool,
    /// `vignette`: `false` hides the game's vignette over the pack's image.
    vignette: bool,
    /// `oldLighting`: `true` keeps the game's face shading in the block colours.
    old_lighting: bool,
    /// `separateAo`: `true` passes AO factor in vertex alpha and keeps pure tint in RGB.
    pub(super) separate_ao: bool,
}

pub(super) struct ClassicPack {
    path: std::path::PathBuf,
    stamp: Option<u64>,
    /// The named level used to resolve dimension.properties. Legacy slots can be shared.
    dimension_name: &'static str,
    /// IDs waiting for a successful GPU report. When active, the game owns them.
    pending_ids: Option<PackIds>,
    /// This dimension reached normal rendering without starting a GPU compile.
    refused: bool,
    /// The source was a ZIP when loaded, so periodic reload checks are cheap.
    watch_zip: bool,
    /// A subsequent target size can succeed even with the same shader source.
    failed_target_size: Option<[u32; 2]>,
}

impl ClassicPack {
    pub(super) fn refusal(
        path: &std::path::Path,
        stamp: Option<u64>,
        dimension_name: &'static str,
    ) -> Self {
        Self {
            path: path.to_owned(),
            stamp,
            dimension_name,
            pending_ids: None,
            refused: true,
            watch_zip: false,
            failed_target_size: None,
        }
    }

    pub(super) fn watched_stamp(&self, path: &std::path::Path) -> Option<Option<u64>> {
        // Poll metadata for running ZIPs. Walking every file in a directory pack
        // would put regular filesystem traversal on the render thread.
        let running_zip = self.pending_ids.is_none() && self.watch_zip;
        (self.path == path && (self.refused || running_zip)).then_some(self.stamp)
    }

    fn retry_for_target(&self, size: [u32; 2]) -> bool {
        self.failed_target_size.is_some_and(|failed| failed != size)
    }

    fn matches_report(
        &self,
        path: &std::path::Path,
        stamp: Option<u64>,
        dimension_name: &str,
        active: bool,
    ) -> bool {
        !self.refused
            && self.path == path
            && self.stamp == stamp
            && self.dimension_name == dimension_name
            && (!active || self.pending_ids.is_some())
    }
}

#[test]
fn classic_pack_accepts_a_gpu_failure_after_activation() {
    use std::path::Path;

    let path = Path::new("shader.zip");
    let dimension = "minecraft:overworld";
    let mut pack = ClassicPack {
        path: path.into(),
        stamp: Some(1),
        dimension_name: dimension,
        pending_ids: Some(PackIds::default()),
        refused: false,
        watch_zip: true,
        failed_target_size: None,
    };
    assert!(pack.matches_report(path, Some(1), dimension, false));
    assert!(pack.matches_report(path, Some(1), dimension, true));
    assert!(!pack.matches_report(path, Some(1), "server:island", true));
    assert!(pack.pending_ids.take().is_some());
    assert!(pack.matches_report(path, Some(1), dimension, false));
    assert!(!pack.matches_report(path, Some(1), "server:island", false));
    assert!(!pack.matches_report(path, Some(1), dimension, true));
    assert!(!pack.matches_report(path, Some(2), dimension, false));
    assert!(!pack.matches_report(Path::new("other.zip"), Some(1), dimension, false));
}

#[cfg(all(test, unix))]
#[test]
fn replacing_a_running_classic_zip_schedules_a_reload() {
    use super::native_shaders::NativeShaders;
    use std::fs;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("shader.zip");
    let replacement = directory.path().join("replacement.zip");
    fs::write(&path, b"old").unwrap();
    let stamp = fingerprint(&path);
    assert!(stamp.is_some());
    let mut state = NativeShaders::default();
    state.classic = Some(ClassicPack {
        path: path.clone(),
        stamp,
        dimension_name: "minecraft:overworld",
        pending_ids: None,
        refused: false,
        watch_zip: true,
        failed_target_size: None,
    });
    assert_eq!(state.check_classic_source(&path, true, false), None);

    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&replacement, b"new").unwrap();
    fs::File::open(&replacement)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    fs::rename(replacement, &path).unwrap();

    // Start with an expired polling interval and the same accepted pack.
    let mut ready = NativeShaders::default();
    ready.classic = state.classic.take();
    let changed = fingerprint(&path);
    assert_ne!(stamp, changed);
    assert_eq!(
        ready.check_classic_source(&path, true, false),
        Some(changed)
    );
}

#[derive(Clone, PartialEq, Eq)]
struct LoadKey {
    path: std::path::PathBuf,
    stamp: Option<u64>,
    resources: std::path::PathBuf,
    dimension: i32,
    dimension_name: String,
    context: (u64, u64),
    options_revision: u64,
}
struct ParsedPack {
    post: horizon_shaders::Post,
    ids: BlockIds,
    entity_ids: PackIds,
}
struct ParserCompletion {
    report_name: String,
    key: LoadKey,
    result: Result<ParsedPack, String>,
}

#[derive(Default)]
pub(super) struct ParserInbox(Arc<Mutex<Option<ParserCompletion>>>);

impl ParserInbox {
    fn lock(&self) -> MutexGuard<'_, Option<ParserCompletion>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct PendingLoad {
    inbox: Weak<Mutex<Option<ParserCompletion>>>,
    report_name: String,
    worker: Option<std::thread::JoinHandle<()>>,
    key: LoadKey,
    result: std::sync::mpsc::Receiver<Result<ParsedPack, String>>,
}
thread_local! {
    // One parser at a time. A changed selection waits for obsolete work to
    // finish; it never spawns an unbounded group of large-stack workers.
    static PENDING_LOAD: RefCell<Option<PendingLoad>> = const { RefCell::new(None) };
}
pub(super) fn shutdown_parser() -> bool {
    PENDING_LOAD.with_borrow_mut(|pending| {
        if pending.as_ref().is_some_and(|load| {
            load.worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        }) {
            return false;
        }
        if let Some(load) = pending.take()
            && let Some(worker) = load.worker
        {
            let _ = worker.join();
        }
        // A fresh engine in the launcher has no live shader programs. Keep
        // the last failure separately so it is still available after Quit.
        REPORT.with_borrow_mut(Option::take);
        true
    })
}
fn parser_busy() -> bool {
    PENDING_LOAD.with_borrow(|pending| pending.is_some())
}
// Any renderer can retire the worker. Only its owner receives the result.
fn complete_parser() {
    PENDING_LOAD.with_borrow_mut(|pending| {
        let Some(load) = pending.as_ref() else { return };
        if load
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return;
        }
        let result = match load.result.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Shader parser worker failed".into())
            }
        };
        let load = pending.take().unwrap();
        if let Some(inbox) = load.inbox.upgrade() {
            *inbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(ParserCompletion {
                    report_name: load.report_name,
                    key: load.key,
                    result,
                });
        } else {
            clear_pending_shader_report(&load.report_name, load.key.dimension);
        }
    })
}
const SOURCE_PANIC: &str = "Shader source compiler panicked: ";
fn parse_source<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    horizon_client::crash::contain(work)
        .unwrap_or_else(|panic| Err(format!("{SOURCE_PANIC}{panic}")))
}

fn start_parser(
    inbox: &ParserInbox,
    name: &str,
    key: LoadKey,
    inputs: horizon_shaders::GameInputs,
    macros: Macros,
    resources: horizon_assets::resource_source::ResourceSource,
) -> Result<(), String> {
    let path = key.path.clone();
    let dimension = key.dimension;
    let dimension_name = key.dimension_name.clone();
    let options = chosen_shader_options(&path);
    let (sender, result) = std::sync::mpsc::channel();
    let _worker =
        std::thread::Builder::new()
            .name("Shader source".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let parsed =
                    parse_source(|| {
                        let pack = Pack::open(&path)
                            .map_err(|error| error.to_string())?
                            .with_chosen(options);
                        let folder = dimension_folder_named(&pack, &dimension_name, dimension);
                        let properties =
                            Properties::from_pack(&pack, &folder, &macros).in_dimension(dimension);
                        let feature_notes = Macros::feature_notes(&properties);
                        let mut post = load_post(&pack, &folder, &properties, &macros, &inputs);
                        let read = |namespace: &str, path: &str| {
                            resources.read(&format!("assets/{namespace}/{path}")).ok()
                        };
                        post.notes.extend(horizon_shaders::resolve_resources(
                            &mut post.textures,
                            &read,
                        ));
                        post.notes.extend(feature_notes);
                        post.notes.extend(properties.notes.iter().map(|note| {
                            format!("shaders.properties:{}: {}", note.line, note.message)
                        }));
                        let ids = BlockIds::parse(&Properties::parse(
                            &pack.read_text("block.properties").unwrap_or_default(),
                            &macros,
                        ));
                        post.notes.extend(
                            ids.notes
                                .iter()
                                .map(|note| format!("block.properties: {note}")),
                        );
                        let entity_text = pack.read_text("entity.properties");
                        let entity_ids = BlockIds::parse_kind(
                            &Properties::parse(entity_text.as_deref().unwrap_or_default(), &macros),
                            "entity",
                        );
                        let item_text = pack.read_text("item.properties");
                        let item_ids = BlockIds::parse_kind(
                            &Properties::parse(item_text.as_deref().unwrap_or_default(), &macros),
                            "item",
                        );
                        let entity_ids = PackIds {
                            block_items: HashMap::new(),
                            item_file: item_text.is_some(),
                            items: item_ids.plain(),
                            missing_item: if item_text.is_some() { -1 } else { 0 },
                            old_hand_light: properties
                                .get("oldHandLight")
                                .is_none_or(|value| value.trim() != "false"),
                            vignette: properties
                                .get("vignette")
                                .is_none_or(|v| v.trim() != "false"),
                            old_lighting: properties
                                .get("oldLighting")
                                .is_some_and(|v| v.trim() == "true"),
                            separate_ao: properties
                                .get("separateAo")
                                .is_some_and(|v| v.trim() == "true"),
                            entities: entity_ids.plain(),
                            missing_entity: if entity_text.is_some() { 65535 } else { 0 },
                            missing_block: if ids.is_empty() { 0 } else { 65535 },
                            blocks: ids.clone(),
                            block_states: RefCell::default(),
                        };
                        Ok(ParsedPack {
                            post,
                            ids,
                            entity_ids,
                        })
                    });
                let _ = sender.send(parsed);
            })
            .map_err(|error| format!("Could not start shader parser: {error}"))?;
    PENDING_LOAD.with_borrow_mut(|pending| {
        *pending = Some(PendingLoad {
            inbox: Arc::downgrade(&inbox.0),
            report_name: name.to_owned(),
            key,
            result,
            worker: Some(_worker),
        })
    });
    Ok(())
}

impl Game<'_> {
    pub(super) fn shader_pack_control(
        &mut self,
        words: &[&str],
        line: &str,
    ) -> horizon_client::Result<Option<Value>> {
        let Some((response, changed)) = command(&mut self.settings, words, line)? else {
            return Ok(None);
        };
        if changed {
            self.reset_shader_selection();
        }
        Ok(Some(response))
    }

    pub(super) fn reset_shader_selection(&mut self) {
        if let Some(renderer) = &mut self.renderer {
            renderer.drop_shader_packs();
        }
        self.native_shaders = super::native_shaders::NativeShaders::picking("");
        if self.clear_shader_mesh_state() {
            self.all_sections_changed();
        }
    }

    /// Settle shader jobs without rendering, including while the launcher covers the game.
    pub(super) fn poll_shader_workers(&mut self) {
        complete_parser();
        let selected = picked_pack(&self.settings).to_owned();
        if !selected.is_empty() {
            // In-game settings can pick a shader without the launcher command.
            LAST_DISABLED.with_borrow_mut(Option::take);
        }
        let selected_path = self.native_shaders.resolve_path(
            &Self::shader_packs_dir(),
            &selected,
            dev_pack().is_some(),
        );
        let selection_changed = if selected.is_empty() {
            !self.native_shaders.picked.is_empty()
        } else {
            std::path::Path::new(&self.native_shaders.picked) != selected_path
        };
        let dimension = match self.dim {
            Dim::Overworld | Dim::Extra(_) => 0,
            Dim::Nether => -1,
            Dim::End => 1,
        };
        let dimension_name = self.dim.id();
        let mut failed = None;
        if let Some(renderer) = &mut self.renderer {
            // A selection or dimension transition stops old programs before consuming
            // their report. Otherwise an old failure can quarantine the new selection,
            // including while the launcher covers the game before its next world frame.
            if selection_changed
                || self
                    .native_shaders
                    .classic
                    .as_ref()
                    .is_some_and(|pack| pack.dimension_name != dimension_name)
                || renderer
                    .shader_pack_dimension()
                    .is_some_and(|old| old != dimension)
            {
                renderer.drop_shader_packs();
                REPORT.with_borrow_mut(Option::take);
                self.pack_ids = None;
                self.native_shaders.classic = None;
            }
            if let Some(report) = renderer.take_shader_report() {
                let (dimension, programs, notes) =
                    (report.dimension, report.programs, report.notes);
                let active = !programs.is_empty();
                if let Some(mut classic) = self.native_shaders.classic.take()
                    && classic.matches_report(
                        &selected_path,
                        fingerprint(&selected_path),
                        dimension_name,
                        active,
                    )
                {
                    if active {
                        remember(&classic.path, classic.stamp, true);
                        self.pack_ids = classic.pending_ids.take();
                        self.native_shaders.classic = Some(classic);
                    } else if let Some(size) = report.failed_target_size {
                        self.pack_ids = None;
                        classic.refused = true;
                        classic.failed_target_size = Some(size);
                        self.native_shaders.classic = Some(classic);
                    } else {
                        self.pack_ids = None;
                        failed = Some((
                            selected.clone(),
                            classic.path,
                            classic.stamp,
                            dimension,
                            notes.clone(),
                        ));
                    }
                    record_report(&selected, dimension, &programs, &notes);
                } else {
                    renderer.drop_shader_packs();
                    REPORT.with_borrow_mut(Option::take);
                    self.pack_ids = None;
                    self.native_shaders.classic = None;
                }
            }
            let mut ready = self.shader_parser.lock();
            if let Some(load) = ready.as_ref() {
                let desired = LoadKey {
                    stamp: fingerprint(&selected_path),
                    path: selected_path,
                    resources: self.options.resources.clone(),
                    options_revision: OPTIONS_REVISION.with(Cell::get),
                    dimension,
                    dimension_name: dimension_name.to_owned(),
                    context: renderer.shader_context_key(),
                };
                if load.key != desired {
                    if self.native_shaders.classic.is_none() {
                        clear_pending_shader_report(&load.report_name, load.key.dimension);
                    }
                    *ready = None;
                }
            }
        }
        if let Some((name, path, stamp, dimension, notes)) = failed {
            // A compile or resize failure uses the same fallback as a source failure.
            let reason = notes.first().map_or("The shader pack could not be loaded.", String::as_str);
            self.shader_pack_failed(&name, &path, stamp, dimension, reason, &notes);
        }
    }

    /// Keep the mesher's three classic shader flags in sync as one change.
    fn update_classic_mesh_flags(&mut self) -> bool {
        let (shader_pack, lighting, separate_ao) =
            self.pack_ids.as_ref().map_or((false, false, false), |ids| {
                (true, !ids.old_lighting, ids.separate_ao)
            });
        let changed = (shader_pack, lighting, separate_ao)
            != (
                self.streaming.shader_pack,
                self.streaming.shader_pack_lighting,
                self.streaming.separate_ao,
            );
        self.streaming.shader_pack = shader_pack;
        self.streaming.shader_pack_lighting = lighting;
        self.streaming.separate_ao = separate_ao;
        changed
    }

    /// Drop mesh settings from a pack that no longer draws. The caller invalidates
    /// the existing meshes, or folds that into a resource reload already in progress.
    pub(super) fn clear_shader_mesh_state(&mut self) -> bool {
        clear_shader_report();
        self.pack_ids = None;
        let classic_changed = self.update_classic_mesh_flags();
        let translucent_changed = !self.streaming.translucent_layers.is_empty();
        if translucent_changed {
            self.streaming.translucent_layers = Arc::from([]);
        }
        classic_changed || translucent_changed
    }

    /// Loads the pack for the current dimension when it changed, and returns this frame's
    /// inputs to the pack's uniforms.
    pub(super) fn shader_pack_inputs(
        &mut self,
        camera: glam::DVec3,
        partial: f32,
        sky: &horizon_client::environment::Sky,
    ) -> PackInputs {
        self.poll_shader_workers();
        // A shader API 0.2 pack whose terrain takes the quads' looks (their tint, shade, ambient
        // occlusion, face and light level apart): every quad carries its look, so the world
        // meshes again once when the renderer's sections start or stop carrying them, and never
        // otherwise.
        let looks = self.renderer.as_ref().is_some_and(|r| r.terrain_looks());
        if self.streaming.looks != looks {
            self.streaming.looks = looks;
            self.all_sections_changed();
        }
        let selected = picked_pack(&self.settings).to_owned();
        let path = self.native_shaders.resolve_path(
            &Self::shader_packs_dir(),
            &selected,
            dev_pack().is_some(),
        );
        let file = path.file_name().unwrap_or_default().to_string_lossy();
        let picked = if selected.is_empty() || active_pack(&file, &self.settings.mods).is_empty() {
            String::new()
        } else {
            path.to_string_lossy().into_owned()
        };
        if self.native_shaders.picked != picked {
            self.native_shaders.select(&picked);
            if let Some(renderer) = &mut self.renderer {
                renderer.drop_shader_packs();
            }
            if self.clear_shader_mesh_state() {
                self.all_sections_changed();
            }
        } else if self.update_classic_mesh_flags() {
            self.all_sections_changed();
        }
        if picked.is_empty() {
            return PackInputs::default();
        }
        if !horizon_client::sprites::labpbr_maps() {
            horizon_client::sprites::set_labpbr_maps(true);
            self.reload_packs();
            return PackInputs::default();
        }
        let name = selected.clone();
        let dimension = match self.dim {
            Dim::Overworld | Dim::Extra(_) => 0,
            Dim::Nether => -1,
            Dim::End => 1,
        };
        let dimension_name = self.dim.id();
        let native = (dev_pack().is_none() && selected.starts_with("horizon:"))
            || horizon_shader_api::package::is_pack(&path);
        let waiting_for_parser = parser_busy() && self.shader_parser.lock().is_none();
        if native {
            // One of Horizon's own packs (game/modding/native_shaders.rs).
            self.tick_native_shader_pack(&path);
        } else if self.screenshot_sequence.is_none()
            && let Some(renderer) = &mut self.renderer
            && let Some(stamp) = self.native_shaders.check_classic_source(
                &path,
                renderer.shader_pack_dimension() == Some(dimension)
                    && !self.native_shaders.classic.as_ref().is_some_and(|classic| {
                        classic.retry_for_target(renderer.shader_pack_target_size())
                    }),
                waiting_for_parser,
            )
        {
            // A replaced source supersedes the programs or fallback loaded for this dimension.
            if renderer.shader_pack_dimension() == Some(dimension) {
                renderer.drop_shader_packs();
                self.native_shaders.classic = None;
                self.pack_ids = None;
            }
            // Lava uses the solid terrain program. Supply its sprite layers before
            // taking the renderer inputs handed to the background source parser.
            renderer.set_pack_lava_layers(
                ["minecraft:block/lava_still", "minecraft:block/lava_flow"]
                    .into_iter()
                    .filter_map(|f| self.pack.render.sprite_layers.get(f).map(|&l| l as u16))
                    .collect(),
            );
            let key = LoadKey {
                path: path.clone(),
                stamp,
                resources: self.options.resources.clone(),
                options_revision: OPTIONS_REVISION.with(Cell::get),
                dimension,
                dimension_name: dimension_name.to_owned(),
                context: renderer.shader_context_key(),
            };
            let ready = self
                .shader_parser
                .lock()
                .take()
                .and_then(|parsed| (parsed.key == key).then_some(parsed.result));
            if let Some(result) = ready {
                match result {
                    Ok(mut parsed) => {
                        parsed.ids = resolve_block_tags(
                            &parsed.ids,
                            &world_items!(self).block_tags,
                            self.remote
                                .as_ref()
                                .map(|r| (r.builtin.as_ref(), r.registries.as_ref())),
                        );
                        parsed.entity_ids.blocks = parsed.ids.clone();
                        let biomes = world_biomes!(self);
                        let names: Vec<&str> = biomes
                            .list
                            .iter()
                            .map(|biome| biome.name.as_str())
                            .collect();
                        parsed.post.custom.set_biomes(&names);
                        let table = state_material_table(&self.pack, &parsed.ids);
                        parsed.entity_ids.block_items = block_item_ids(
                            self.res.facts(),
                            self.pack.assets.registry,
                            &parsed.ids,
                        );
                        renderer.set_separate_ao(parsed.entity_ids.separate_ao);
                        self.pack_ids = None;
                        self.native_shaders.classic = None;
                        match renderer.set_shader_pack_at_frame_boundary(
                            dimension,
                            parsed.post,
                            &table,
                        ) {
                            Ok(report) => {
                                self.native_shaders.classic = Some(ClassicPack {
                                    path: path.clone(),
                                    stamp,
                                    dimension_name,
                                    pending_ids: Some(parsed.entity_ids),
                                    refused: false,
                                    watch_zip: path.is_file()
                                        && path
                                            .extension()
                                            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip")),
                                    failed_target_size: None,
                                });
                                if let Some((programs, notes)) = report {
                                    record_shader_report(&name, dimension, &programs, &notes, true);
                                }
                            }
                            Err(refused) => {
                                // The pack lacks programs or inputs for this dimension.
                                // The next named dimension can use a different shader folder.
                                self.native_shaders.classic =
                                    Some(ClassicPack::refusal(&path, stamp, dimension_name));
                                let notes = shader_failure_notes(&refused.reason, &refused.notes);
                                record_report(&name, dimension, &[], &notes);
                            }
                        }
                    }
                    Err(_) if fingerprint(&path) != stamp => {
                        clear_pending_shader_report(&name, dimension);
                        self.pack_ids = None;
                        self.native_shaders.classic = None;
                        renderer.drop_shader_packs();
                    }
                    Err(error) => {
                        let report = if error.starts_with(SOURCE_PANIC) {
                            self.shader_pack_failed(
                                &name,
                                &path,
                                stamp,
                                dimension,
                                &error,
                                std::slice::from_ref(&error),
                            )
                        } else {
                            self.pack_ids = None;
                            self.native_shaders.classic =
                                Some(ClassicPack::refusal(&path, stamp, dimension_name));
                            let _ = renderer.set_shader_pack(dimension, None, &[]);
                            true
                        };
                        if report {
                            record_report(&name, dimension, &[], &[error]);
                        }
                    }
                }
            } else if !parser_busy() {
                let inputs = renderer.pack_inputs();
                let mut macros = Macros::standard();
                renderer.define_pack_macros(&mut macros);
                match start_parser(
                    &self.shader_parser,
                    &name,
                    key,
                    inputs,
                    macros,
                    self.pack.source.clone(),
                ) {
                    Ok(()) => record_shader_report(
                        &name,
                        dimension,
                        &[],
                        &["Processing shader sources in the background. Normal rendering remains active until ready.".into()],
                        true,
                    ),
                    Err(error) => {
                        self.native_shaders.classic =
                            Some(ClassicPack::refusal(&path, stamp, dimension_name));
                        let _ = renderer.set_shader_pack(dimension, None, &[]);
                        record_report(&name, dimension, &[], &[error]);
                    }
                }
            }
        }
        let lerp = |(old, new): (f32, f32)| old + (new - old) * partial;
        let at = camera.floor().as_ivec3();
        let light = self.world.light(at.x, at.y, at.z);
        let biome = self
            .world
            .chunks
            .get(&(at.x >> 4, at.z >> 4))
            .map(|c| c.biome(at.x, at.y, at.z));
        let (held_items, held_light) = self.held_for_pack();
        let status = self.pack_player_status();
        let info = biome.map(|b| world_biomes!(self).get(b));
        let (end_flash_intensity, end_flash_position) = self.end_flash_for_pack(partial);
        let dimension = self.remote.as_ref().map(|r| &r.dimension);
        PackInputs {
            // `Level.getDefaultClockTime`: 0 in a dimension type with no default clock (the
            // Nether's has none), as Iris 1.11.7 reads `worldTime` there (probe pack).
            day_time: if dimension.is_some_and(|d| d.default_clock.is_none()) {
                0
            } else {
                self.world.day_time
            },
            rain: lerp(self.world.weather.rain_level),
            thunder: lerp(self.world.weather.thunder_level),
            eye_in: match self.camera_fluid(camera.to_array()) {
                CameraFluid::None => 0,
                CameraFluid::Water => 1,
                CameraFluid::Lava => 2,
                CameraFluid::PowderSnow => 3,
            },
            blindness: f32::from(u8::from(self.potions.effects.get("blindness").is_some())),
            eye_light: [i32::from(light & 15), i32::from(light >> 4)],
            hide_gui: self.ui.hide_gui,
            brightness: self.settings.gamma,
            biome: info.map_or(-1, |b| {
                let names = world_biomes!(self).list.iter().map(|b| b.name.as_str());
                horizon_shaders::biome_pack_id(&b.name, names)
            }),
            biome_known: biome.is_some(),
            biome_category: info.map_or(0, |b| horizon_shaders::biome_category(&b.name)),
            biome_precipitation: info.map_or(0, |b| {
                horizon_shaders::biome_precipitation(b.precipitation, b.temperature)
            }),
            temperature: info.map_or(0.0, |b| b.temperature),
            rainfall: info.map_or(0.0, |b| b.downfall),
            held_items,
            held_light,
            eye_position: if self.camera_type.is_first_person() {
                camera
            } else {
                self.player.as_ref().map_or(camera, |p| {
                    let alpha = self.clock.camera_fraction();
                    let feet = self.previous_position.lerp(p.position, alpha);
                    glam::DVec3::new(
                        feet.x,
                        feet.y
                            + f64::from(
                                self.view.eye_height_old
                                    + (self.view.eye_height - self.view.eye_height_old)
                                        * alpha as f32,
                            ),
                        feet.z,
                    )
                })
            },
            player_look: glam::DVec3::from_array(self.forward()).as_vec3(),
            elytra_flying: self.player.as_ref().is_some_and(|p| p.fall_flying),
            player_mood: self.ambience.moodiness,
            heavy_fog: self.boss_overlay.should_create_world_fog(),
            end_flash_position,
            end_flash_intensity,
            right_handed: !self.settings.main_hand_left,
            on_ground: self.player.as_ref().is_some_and(|p| p.ground.on_ground),
            min_y: self.world.shape.min_y,
            height: self.world.shape.height,
            logical_height: self.world.shape.logical_height,
            has_skylight: self.world.shape.sky_light,
            has_ceiling: self.world.shape.has_ceiling,
            cloud_height: horizon_client::gfx::pack_uniforms::cloud_height(sky),
            // Vanilla's drift in float, times its 0.030000001F blocks a tick.
            cloud_time: horizon_client::clouds::drift(
                self.world.game_time,
                f64::from(partial),
                self.pack.clouds.0,
            ) as f32
                * 0.030_000_001,
            mod_params: self.native_shader_params_now(partial),
            status,
            ambient_light: dimension.map_or(0.0, |d| d.ambient_light),
            still_ticks: self.clock.still().then(|| self.visual_ticks()),
            frame_timing: None,
        }
    }

    /// The local player's state for Iris's status uniforms, read as the HUD reads it.
    fn pack_player_status(&mut self) -> PlayerStatus {
        let spectator = self.spectator();
        let survival = !self.creative && !spectator;
        let invisible = self.own_invisible();
        let riding = self.riding.is_some();
        let Some(p) = &mut self.player else {
            return PlayerStatus::default();
        };
        PlayerStatus {
            survival,
            health: p.health.health(),
            max_health: p.attribute("minecraft:max_health").unwrap_or(20.0) as f32,
            food: p.health.food.food as f32,
            air: p.air as f32,
            armor: p
                .attribute("minecraft:armor")
                .or_else(|_| p.attribute("armor"))
                .unwrap_or(0.0) as f32,
            spectator,
            riding,
            feet_in_water: p.fluids.was_touching_water,
            swimming: p.pose == horizon_client::pose::Pose::Swimming,
            burning: p.fire_ticks > 0 && !spectator,
            hurt: p.health.hurt_animation_time > 0,
            invisible,
            sneaking: p.pose == horizon_client::pose::Pose::Crouching,
            sprinting: p.sprinting,
        }
    }

    /// Tag-dependent ID tables and cached block entity IDs must be rebuilt after a
    /// world's data packs or a remote server replace their tags.
    pub(super) fn invalidate_tagged_shader_ids(&mut self) {
        if self
            .pack_ids
            .as_ref()
            .is_some_and(|ids| ids.blocks.has_block_tags())
            || self
                .native_shaders
                .classic
                .as_ref()
                .and_then(|pack| pack.pending_ids.as_ref())
                .is_some_and(|ids| ids.blocks.has_block_tags())
        {
            clear_shader_report();
            self.pack_ids = None;
            self.native_shaders.classic = None;
            if let Some(renderer) = &mut self.renderer {
                renderer.drop_shader_packs();
            }
        }
    }

    /// The Shader Options page opened: the picked pack's options, screens and names.
    pub(super) fn open_shader_options(&mut self) {
        let mut macros = Macros::standard();
        if let Some(renderer) = &self.renderer {
            renderer.define_pack_macros(&mut macros);
        }
        let name = &self.settings.horizon.shader_pack;
        if self
            .settings
            .shader_options
            .as_ref()
            .is_some_and(|options| options.pack == *name && options.changed)
        {
            return;
        }
        self.settings.shader_options = read_shader_options(name, &self.settings.language, &macros);
    }

    /// The Shader Options page closed with changes: they are saved, and the pack loads again.
    pub(super) fn save_shader_options(&mut self) {
        if !persist_shader_options(&mut self.settings, &mut self.ui.options) {
            return;
        }
        OPTIONS_REVISION.with(|revision| revision.set(revision.get().wrapping_add(1)));
        clear_shader_report();
        self.pack_ids = None;
        self.native_shaders.classic = None;
        if let Some(renderer) = &mut self.renderer {
            renderer.drop_shader_packs();
        }
    }

    /// Turns off the pack `name` after loading it panicked or failed: nothing runs in its place,
    /// it is no longer the picked pack (so the next start does not load it again), and the chat
    /// says so.
    fn shader_pack_failed(
        &mut self,
        name: &str,
        path: &std::path::Path,
        stamp: Option<u64>,
        dimension: i32,
        error: &str,
        notes: &[String],
    ) -> bool {
        if fingerprint(path) != stamp {
            clear_shader_report();
            self.pack_ids = None;
            self.native_shaders.classic = None;
            if let Some(renderer) = &mut self.renderer {
                renderer.drop_shader_packs();
            }
            return false;
        }
        remember(path, stamp, false);
        log::error!("shader pack {name}: turned off: {error}");
        self.pack_ids = None;
        self.native_shaders.classic = None;
        if let Some(renderer) = &mut self.renderer {
            // Marks this dimension as loaded, without a pack, so it is not tried again (without
            // a pack this can't fail).
            let _ = renderer.set_shader_pack(dimension, None, &[]);
        }
        if !name.is_empty() && dev_pack().is_none() && self.settings.horizon.shader_pack == name {
            let mut notes = shader_failure_notes(error, notes);
            if let Err(error) = self
                .settings
                .horizon
                .select_shader_pack(&Self::game_dir(), String::new())
            {
                // The failed shader must stay off for this session even if
                // the disk cannot record the change for the next launch.
                self.settings.horizon.shader_pack.clear();
                notes.insert(1.min(notes.len()), format!("Could not save the Off selection: {error}"));
            }
            LAST_DISABLED.with_borrow_mut(|report| {
                *report = Some(json!({"pack": name, "dimension": dimension, "notes": notes}));
            });
        }
        self.system_message(Text::colored(
            format!("Shader pack {name} could not be loaded and was turned off"),
            0xFF5555,
        ));
        true
    }

    /// Whether the game's vignette draws: the option, unless the loaded pack turns it off.
    pub(super) fn vignette_shown(&self) -> bool {
        self.settings.vignette && self.pack_ids.as_ref().is_none_or(|ids| ids.vignette)
    }

    /// `currentRenderedItemId` of a held `item`, [`rendered_item_id`]'s; 0 without a pack.
    pub(super) fn rendered_item_id(&self, item: &str) -> i32 {
        self.pack_ids.as_ref().map_or(0, |ids| {
            rendered_item_id(
                ids.block_items.get(item).copied(),
                ids.items.get(item).copied(),
                ids.item_file,
            )
        })
    }

    /// `heldItemId(2)` and `heldBlockLightValue(2)`: main hand, then off hand (5.2, 5.3).
    fn held_for_pack(&mut self) -> ([i32; 2], [i32; 2]) {
        if self.pack_ids.is_none() {
            return ([0; 2], [0; 2]);
        }
        let lum = self.luminance();
        let Some(ids) = &self.pack_ids else {
            return ([0; 2], [0; 2]);
        };
        let hands = [self.selected, super::shield::OFFHAND_SLOT].map(|slot| {
            let item = self.stack_in(slot).map(|s| &*s.item);
            let id = item.and_then(|i| ids.items.get(i).copied());
            (
                id.unwrap_or(ids.missing_item),
                item.map_or(0, |i| i32::from(lum.of(i))),
            )
        });
        let first = if ids.old_hand_light && hands[1].1 > hands[0].1 {
            hands[1]
        } else {
            hands[0]
        };
        ([first.0, hands[1].0], [first.1, hands[1].1])
    }
}

impl Game<'_> {
    pub(super) fn mark_rendered_item(&self, start: usize, end: usize, item: &str) {
        if self.pack_ids.is_some() && self.emitting_entity_items.get() {
            EntitySpan::item_range(
                &mut self.entity_spans.borrow_mut(),
                start,
                end,
                self.rendered_item_id(item),
            );
        }
    }

    /// Starts a span of the entity geometry at vertex `start` for entity type `name`
    /// (`minecraft:zombie`; "" for one the pack cannot name). Nothing without a pack.
    pub(super) fn mark_entity(&self, start: usize, name: &str) {
        self.mark_entity_part(start, name, false);
    }

    /// Starts a span for entity geometry that blends (name tags, shadows).
    pub(super) fn mark_translucent_entity(&self, start: usize, name: &str) {
        self.mark_entity_part(start, name, true);
    }

    fn mark_entity_part(&self, start: usize, name: &str, translucent: bool) {
        let id = match &self.pack_ids {
            Some(ids) => ids
                .entities
                .get(name)
                .copied()
                .unwrap_or(ids.missing_entity),
            None if native_spans() => name_id(name),
            None => return,
        };
        self.push_span(start, "ENTITIES", id);
        if let Some(last) = self.entity_spans.borrow_mut().last_mut() {
            last.translucent = translucent;
            if let Some(gpu) = self.skin_frame.borrow_mut().spans.last_mut() {
                gpu.translucent = translucent;
            }
        }
    }

    /// Starts a span for the block entity of block state `state`.
    pub(super) fn mark_block_entity(&self, start: usize, state: u32) {
        let Some(ids) = &self.pack_ids else {
            if native_spans() {
                self.push_span(start, "BLOCK_ENTITIES", state as i32);
            }
            return;
        };
        let id = *ids
            .block_states
            .borrow_mut()
            .entry(state)
            .or_insert_with(|| {
                let registry = self.pack.assets.registry;
                registry
                    .state(horizon_world::registry::BlockStateId(state))
                    .and_then(|s| {
                        ids.blocks
                            .id(&s.name, |k| s.properties.get(k).map(String::as_str))
                    })
                    .unwrap_or(ids.missing_block)
            });
        self.push_span(start, "BLOCK_ENTITIES", id);
        let emission = self
            .res
            .facts()
            .get(horizon_world::registry::BlockStateId(state))
            .emission;
        if let Some(last) = self.entity_spans.borrow_mut().last_mut() {
            last.emissive = emission > 0;
            if let Some(gpu) = self.skin_frame.borrow_mut().spans.last_mut() {
                gpu.emissive = emission > 0;
            }
        }
    }

    /// Starts a span for mod model copies (`WorldDrawContext::draw_model_instances`) under a
    /// pack: block entities, as `BlockEntityDrawContext::model` draws the same model, with the
    /// id of a block the pack doesn't list, since no block owns them. Nothing without a pack.
    pub(super) fn mark_model_copies(&self, start: usize) {
        if let Some(ids) = &self.pack_ids {
            self.push_span(start, "BLOCK_ENTITIES", ids.missing_block);
        }
    }

    /// Starts the span of the local player's model; `hidden` in first person, where only a
    /// pack's shadow pass draws it.
    pub(super) fn mark_player(&self, start: usize, hidden: bool) {
        self.mark_entity(start, "minecraft:player");
        if (self.pack_ids.is_some() || native_spans())
            && let Some(last) = self.entity_spans.borrow_mut().last_mut()
        {
            last.player = true;
            last.hidden = hidden;
            if let Some(gpu) = self.skin_frame.borrow_mut().spans.last_mut() {
                gpu.player = true;
                gpu.hidden = hidden;
            }
        }
    }

    /// Marks vertices `start..end` (one entity) as hidden from the camera, for an entity
    /// Horizon's culling would drop but a pack's shadow pass still draws (`EntitySpan::hide`).
    pub(super) fn hide_spans_from(&self, start: usize, end: usize) -> bool {
        EntitySpan::hide(&mut self.entity_spans.borrow_mut(), start, end)
    }

    /// Starts a span of geometry that is not an entity (`PARTICLES`, `DESTROY`...).
    pub(super) fn mark_stage(&self, start: usize, stage: &str) {
        if self.pack_ids.is_some() || native_spans() {
            self.push_span(start, stage, 0);
        }
    }

    /// Forgets the last frame's marks.
    pub(super) fn clear_entity_marks(&self) {
        self.entity_spans.borrow_mut().clear();
        self.entity_motion.borrow_mut().clear();
        self.entity_motion_posed.borrow_mut().clear();
        self.native_shaders.entity_runs.borrow_mut().clear();
        self.native_shaders.posed_runs.borrow_mut().clear();
    }

    fn push_span(&self, start: usize, stage: &str, id: i32) {
        let mut spans = self.entity_spans.borrow_mut();
        let span = EntitySpan {
            start: start as u32,
            stage: horizon_shaders::render_stage(stage),
            id,
            ..Default::default()
        };
        if self.pack_ids.is_some() && self.skinning() {
            let mut skin = self.skin_frame.borrow_mut();
            let gpu = EntitySpan {
                start: skin.vertices,
                ..span
            };
            match skin.spans.last_mut() {
                Some(last) if last.start == gpu.start => *last = gpu,
                _ => skin.spans.push(gpu),
            }
        }
        // A span that drew nothing gives way to the next.
        match spans.last_mut() {
            Some(last) if last.start == span.start => *last = span,
            _ => spans.push(span),
        }
    }
}

/// `currentRenderedItemId` of an item (Iris 1.11.7, observed with probe packs): a block item
/// (`block`: its block's `block.properties` id, 0 when the file lacks it) reads that, whatever
/// `item.properties` says of it; any other item its `item.properties` id (`item`), and one the
/// file lacks 65535, or 0 when the pack has no `item.properties` (`item_file`).
fn rendered_item_id(block: Option<i32>, item: Option<i32>, item_file: bool) -> i32 {
    block.or(item).unwrap_or(if item_file { 65535 } else { 0 })
}

/// The block items' `currentRenderedItemId`: for each block's default state, the item it gives
/// (`StateFacts::item`) takes the state's id in `ids`, or 0. A block named as its item wins over
/// another block giving that item.
fn block_item_ids(
    facts: &horizon_client::blocks::BlockFacts,
    registry: &horizon_world::registry::BlockRegistry,
    ids: &BlockIds,
) -> HashMap<String, i32> {
    let mut out = HashMap::new();
    for (i, state) in facts.states.iter().enumerate() {
        let id = horizon_world::registry::BlockStateId(i as u32);
        if &*state.item == "minecraft:air" || registry.default_state(&state.name) != Some(id) {
            continue;
        }
        let value = registry
            .state(id)
            .and_then(|s| ids.id(&s.name, |k| s.properties.get(k).map(String::as_str)))
            .unwrap_or(0);
        if state.item == state.name {
            out.insert(state.item.to_string(), value);
        } else {
            out.entry(state.item.to_string()).or_insert(value);
        }
    }
    out
}

/// Spans for a native mod (`native_render::RenderNeeds::entity_spans`), without a pack.
fn native_spans() -> bool {
    horizon_client_render::render_slots::needs().entity_spans
}

/// A span's id for entity type `name` without a pack: stable, so a mod can match runs from one
/// frame to the next.
fn name_id(name: &str) -> i32 {
    // FNV-1a.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    hash as i32
}

/// Exact state identities for the GLSL terrain adapter; distinct from native material
/// tags, which are indexed by sprite layer (`block_id_table`).
fn state_material_table(pack: &super::PackResources, ids: &BlockIds) -> Vec<i32> {
    state_block_id_table(pack.assets.registry, ids, |id| {
        pack.context.tables.facts(id).map_or(0, |f| f.emission)
    })
}
/// Exact model/fluid identities and emitted light for each source block state.
fn state_block_id_table(
    registry: &horizon_world::registry::BlockRegistry,
    ids: &BlockIds,
    emission: impl Fn(horizon_world::registry::BlockStateId) -> u8,
) -> Vec<i32> {
    use horizon_world::registry::BlockStateId;
    let materials = terrain_state_ids(registry, ids);
    let stride = horizon_client_render::gfx::TERRAIN_STATE_WORDS;
    let mut table = vec![-1; registry.len() * stride];
    for i in 0..registry.len() {
        table[stride * i] = materials[2 * i];
        table[stride * i + 1] = 0;
        table[stride * i + 2] = materials[2 * i + 1];
        table[stride * i + 3] = 1;
        table[stride * i + 4] = i32::from(emission(BlockStateId(i as u32)).min(15));
    }
    table
}

/// Model/fluid material identities, before packing the renderer's five-word state records.
fn terrain_state_ids(
    registry: &horizon_world::registry::BlockRegistry,
    ids: &BlockIds,
) -> Vec<i32> {
    use horizon_world::{
        fluid::{FluidKind, FluidState},
        registry::BlockStateId,
    };
    let material = |state: &horizon_world::chunk::BlockState| {
        ids.id(&state.name, |k| state.properties.get(k).map(String::as_str))
            .unwrap_or(-1)
    };
    (0..registry.len())
        .flat_map(|i| {
            let Some(state) = registry.state(BlockStateId(i as u32)) else {
                return [-1, -1];
            };
            let fluid = FluidState::from_block(state).unwrap_or(FluidState::EMPTY);
            let name = match fluid.kind() {
                FluidKind::Water | FluidKind::FlowingWater => "minecraft:water",
                FluidKind::Lava | FluidKind::FlowingLava => "minecraft:lava",
                FluidKind::Empty => return [material(state), -1],
                FluidKind::Mod(_) | FluidKind::ModFlowing(_) => {
                    return [material(state), material(state)];
                }
            };
            let fluid = if state.name == name {
                Some(state)
            } else {
                registry
                    .default_state(name)
                    .and_then(|id| registry.state(id))
            };
            [material(state), fluid.map_or(-1, material)]
        })
        .collect()
}

#[cfg(test)]
mod terrain_source_tests {
    use super::*;

    #[test]
    fn terrain_source_cpu_state_filters_fluid_owners_and_pack_reload() {
        let rows = serde_json::json!({"defaults":{"minecraft:air":0,"fixture:signal":1,"minecraft:water":3,"minecraft:lava":5},"states":[
            ["",true,false,"","minecraft:air"],
            ["",false,false,"",{"id":"fixture:signal","properties":{"lit":"false","waterlogged":"false"}}],
            ["",false,true,"",{"id":"fixture:signal","properties":{"lit":"true","waterlogged":"true"}}],
            ["",false,true,"",{"id":"minecraft:water","properties":{"level":"0"}}],
            ["",false,true,"",{"id":"minecraft:water","properties":{"level":"1"}}],
            ["",false,true,"",{"id":"minecraft:lava","properties":{"level":"0"}}]
        ]});
        let registry =
            horizon_world::registry::BlockRegistry::from_facts_json(rows.to_string().as_bytes())
                .unwrap();
        let parse = |text| BlockIds::parse(&Properties::parse(text, &Macros::standard()));
        let ids = parse(
            "block.31=fixture:signal:lit=false\nblock.47=fixture:signal:lit=true\nblock.59=minecraft:water:level=0\nblock.61=minecraft:water:level=1\nblock.73=minecraft:lava\n",
        );
        assert_eq!(
            terrain_state_ids(&registry, &ids),
            [-1, -1, 31, -1, 47, 59, 59, 59, 61, 61, 73, 73]
        );
        let changed = parse("block.211=fixture:signal:lit=true\nblock.223=minecraft:water\n");
        assert_eq!(
            terrain_state_ids(&registry, &changed),
            [-1, -1, -1, -1, 211, 223, 223, 223, 223, 223, -1, -1]
        );
        assert_eq!(terrain_state_ids(&registry, &BlockIds::default()), [-1; 12]);
    }
}

/// Sprite material IDs for native packs: named blocks win, then the most common
/// model use. GLSL uses the exact state table above instead.
/// Votes from distinct blocks, so adding states cannot change a shared sprite's material.
#[derive(Default)]
struct BlockIdVotes<'a>(BTreeMap<(u32, i32), (bool, std::collections::BTreeSet<&'a str>)>);

impl<'a> BlockIdVotes<'a> {
    fn add(&mut self, layer: u32, id: i32, name: &'a str, sprite: Option<&str>) {
        // Containers share the still sprite. Its fluid owns it just as a block
        // named exactly like its sprite does; a lower container id must not win.
        let owner = sprite
            .and_then(|s| s.strip_prefix("block/"))
            .map(|s| match s {
                "lava_still" | "lava_flow" => "lava",
                "water_still" | "water_flow" => "water",
                _ => s,
            });
        let named = owner == Some(name.strip_prefix("minecraft:").unwrap_or(name));
        let v = self.0.entry((layer, id)).or_default();
        v.0 |= named;
        v.1.insert(name);
    }

    fn write(self, table: &mut [i32]) {
        let mut best = BTreeMap::new();
        for ((layer, id), (named, blocks)) in self.0 {
            let candidate = (named, blocks.len(), -id);
            let b = best.entry(layer).or_insert(candidate);
            if candidate > *b {
                *b = candidate;
            }
        }
        for (layer, (_, _, id)) in best {
            if let Some(slot) = table.get_mut(layer as usize * 2) {
                *slot = -id;
            }
        }
    }
}

#[cfg(test)]
mod block_id_tests {
    use super::BlockIdVotes;

    #[test]
    fn fluid_sprites_keep_their_fluid_material_when_a_container_uses_them() {
        let mut votes = BlockIdVotes::default();
        // Our runtime probe: cauldron 17, lava 29. The lower container id used to
        // win the still sprite's tie, while the flow sprite already read 29.
        for (layer, sprite) in [(0, "lava_still"), (1, "lava_flow")] {
            votes.add(
                layer,
                29,
                "minecraft:lava",
                Some(&format!("block/{sprite}")),
            );
        }
        votes.add(0, 17, "minecraft:lava_cauldron", Some("block/lava_still"));
        // Water's still sprite is also shared by containers and waterlogged models.
        votes.add(2, 31, "minecraft:water", Some("block/water_still"));
        votes.add(2, 17, "minecraft:water_cauldron", Some("block/water_still"));
        votes.add(
            2,
            17,
            "minecraft:other_container",
            Some("block/water_still"),
        );
        let mut table = [-1; 8];
        votes.write(&mut table);
        assert_eq!(table, [29, -1, 29, -1, 31, -1, -1, -1]);
    }

    #[test]
    fn shared_model_sprites_still_use_distinct_blocks_and_the_named_owner() {
        let mut votes = BlockIdVotes::default();
        for _ in 0..20 {
            votes.add(0, 3, "minecraft:many_states", Some("block/shared"));
        }
        votes.add(0, 9, "minecraft:one", Some("block/shared"));
        votes.add(0, 9, "minecraft:two", Some("block/shared"));
        votes.add(1, 20, "minecraft:oak_leaves", Some("block/oak_leaves"));
        votes.add(1, 3, "minecraft:one", Some("block/oak_leaves"));
        votes.add(1, 3, "minecraft:two", Some("block/oak_leaves"));
        let mut table = [-1; 4];
        votes.write(&mut table);
        assert_eq!(table, [9, -1, 20, -1]);
    }
}

/// `mc_Entity` (x, y) of each block sprite layer, as (id, fluid) pairs: terrain vertices carry
/// their sprite layer, not their block. A layer takes the id of the block named like its sprite
/// (`block/oak_leaves` for `oak_leaves`, and still/flow sprites for their fluid), else the id
/// most blocks drawing it share (ties: the lowest); -1 when no block drawing it has one.
/// y is 1 for the fluid sprites, 0 otherwise (`fluid_pairs`).
pub(super) fn block_id_table(pack: &super::PackResources, ids: &BlockIds) -> Vec<i32> {
    let layers = pack
        .render
        .sprite_layers
        .values()
        .max()
        .map_or(0, |l| l + 1) as usize;
    let sprite = |name: &str| pack.render.sprite_layers.get(name).copied();
    let fluids = ["water_still", "water_flow", "lava_still", "lava_flow"]
        .map(|f| sprite(&format!("minecraft:block/{f}")));
    let mut table = fluid_pairs(layers, fluids.into_iter().flatten());
    if ids.is_empty() {
        return table;
    }
    let registry = pack.assets.registry;
    let catalog = &pack.render.model_catalog;
    // (layer, id) -> (a block named like the sprite, the blocks).
    let mut votes = BlockIdVotes::default();
    for i in 0..registry.len() {
        let state_id = horizon_world::registry::BlockStateId(i as u32);
        let Some(state) = registry.state(state_id) else {
            continue;
        };
        let Some(id) = ids.id(&state.name, |k| state.properties.get(k).map(String::as_str)) else {
            continue;
        };
        let block = state.name.strip_prefix("minecraft:").unwrap_or(&state.name);
        let mut vote = |layer: u32| {
            votes.add(
                layer,
                id,
                &state.name,
                pack.layer_names.get(&layer).map(String::as_str),
            );
        };
        // Fluids draw from their own sprites.
        let fluid = match block {
            "water" => ["water_still", "water_flow"],
            "lava" => ["lava_still", "lava_flow"],
            _ => ["", ""],
        };
        for f in fluid.iter().filter(|f| !f.is_empty()) {
            if let Some(l) = sprite(&format!("minecraft:block/{f}")) {
                vote(l);
            }
        }
        let origin = horizon_core::position::BlockPos::new(0, 0, 0);
        for (_, model) in catalog.collect(state_id, origin).unwrap_or_default() {
            for face in &model.faces {
                vote(face.layer);
            }
        }
    }
    votes.write(&mut table);
    table
}

/// `layers` (id, fluid) pairs before the ids are known: id -1, and `mc_Entity.y` 1 for the
/// `fluids` layers, 0 for the others. Iris 1.11.7, with probe packs of ours (#10452), gave
/// terrain y 1 on water and lava, and 0 on every other block, whether `block.properties`
/// maps it or not.
fn fluid_pairs(layers: usize, fluids: impl IntoIterator<Item = u32>) -> Vec<i32> {
    let mut table: Vec<i32> = std::iter::repeat_n([-1, 0], layers).flatten().collect();
    for l in fluids {
        if let Some(slot) = table.get_mut(l as usize * 2 + 1) {
            *slot = 1;
        }
    }
    table
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    #[test]
    fn source_parser_panics_become_errors_without_losing_load_errors() {
        let error = parse_source::<()>(|| panic!("broken source")).unwrap_err();
        assert!(error.starts_with(SOURCE_PANIC));
        assert!(error.contains("broken source"));
        assert_eq!(
            parse_source::<()>(|| Err("missing pack".into())).unwrap_err(),
            "missing pack"
        );
        assert_eq!(parse_source(|| Ok(7)), Ok(7));
    }
    #[test]
    fn shutdown_waits_for_parser_and_discards_its_result() {
        let (release, wait) = std::sync::mpsc::channel();
        let (send, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            wait.recv().unwrap();
            send.send(Err("cancelled result".into())).unwrap();
        });
        PENDING_LOAD.with_borrow_mut(|pending| {
            *pending = Some(PendingLoad {
                inbox: Weak::new(),
                report_name: "shutdown.zip".into(),
                key: LoadKey {
                    path: "shutdown.zip".into(),
                    stamp: None,
                    resources: "resources".into(),
                    dimension: 0,
                    dimension_name: "minecraft:overworld".into(),
                    context: (0, 0),
                    options_revision: 0,
                },
                result,
                worker: Some(worker),
            })
        });
        assert!(!shutdown_parser());
        release.send(()).unwrap();
        while PENDING_LOAD.with_borrow(|pending| {
            !pending
                .as_ref()
                .unwrap()
                .worker
                .as_ref()
                .unwrap()
                .is_finished()
        }) {
            std::thread::yield_now();
        }
        assert!(shutdown_parser());
        assert!(!parser_busy());
        assert!(shutdown_parser());
    }
    #[test]
    fn block_materials_use_exact_states_and_waterlogged_fluid_identity() {
        use horizon_core::nbt::Tag;
        use horizon_world::{chunk::BlockState, registry::BlockRegistry};
        use std::collections::BTreeMap;
        let mut states = Vec::new();
        let mut defaults = BTreeMap::new();
        let mut add = |name: &str, properties: BTreeMap<String, String>| {
            let id = states.len() as i32;
            defaults.entry(name.into()).or_insert(Tag::Int(id));
            states.push(
                BlockState {
                    name: name.into(),
                    properties: properties.into(),
                }
                .to_nbt(),
            );
        };
        add(
            "minecraft:oak_log",
            BTreeMap::from([("axis".into(), "x".into())]),
        );
        add(
            "minecraft:oak_log",
            BTreeMap::from([("axis".into(), "y".into())]),
        );
        add(
            "minecraft:oak_stairs",
            BTreeMap::from([("waterlogged".into(), "true".into())]),
        );
        add("minecraft:air", BTreeMap::new());
        for name in ["minecraft:water", "minecraft:lava"] {
            for level in 0..16 {
                add(name, BTreeMap::from([("level".into(), level.to_string())]));
            }
        }
        let registry = BlockRegistry::from_nbt(&Tag::Compound(BTreeMap::from([
            ("DataVersion".into(), Tag::Int(horizon_world::DATA_VERSION)),
            ("states".into(), Tag::List(states)),
            ("defaults".into(), Tag::Compound(defaults)),
        ])))
        .unwrap();
        let ids = BlockIds::parse(&Properties::parse(
            "block.101=minecraft:oak_log:axis=x\nblock.202=minecraft:oak_log:axis=y\nblock.303=minecraft:water:level=0\nblock.404=minecraft:water:level=7\nblock.505=minecraft:oak_stairs",
            &Macros::standard(),
        ));
        assert!(ids.notes.is_empty(), "{:?}", ids.notes);
        let table = state_block_id_table(&registry, &ids, |id| if id.0 == 0 { 12 } else { 3 });
        assert_eq!(table.len(), registry.len() * 5);
        assert!(
            table
                .chunks_exact(5)
                .all(|state| state[1] == 0 && state[3] == 1)
        );
        assert_eq!(&table[..10], &[101, 0, -1, 1, 12, 202, 0, -1, 1, 3]);
        assert_eq!(&table[10..15], &[505, 0, 303, 1, 3]);
        assert_eq!(&table[(4 + 7) * 5..(4 + 8) * 5], &[404, 0, 404, 1, 3]);
    }
    #[test]
    fn shader_failure_notes_keep_the_reason_ahead_of_diagnostics() {
        let reason = "Required shader programs are unavailable";
        let mut details: Vec<_> = (0..10).map(|index| format!("Diagnostic {index}")).collect();
        details.push(reason.into());
        let notes = shader_failure_notes(reason, &details);
        assert_eq!(notes.first().map(String::as_str), Some(reason));
        assert_eq!(&notes[1..], &details[..10]);
    }

    #[test]
    fn discarding_obsolete_shader_work_only_clears_its_pending_report() {
        record_shader_report("B.zip", -1, &[], &["Parsing".into()], true);
        clear_pending_shader_report("A.zip", -1);
        clear_pending_shader_report("B.zip", 0);
        REPORT.with_borrow(|report| assert!(report.is_some()));
        clear_pending_shader_report("B.zip", -1);
        REPORT.with_borrow(|report| assert!(report.is_none()));

        record_report("B.zip", 0, &["final".into()], &[]);
        clear_pending_shader_report("B.zip", 0);
        REPORT.with_borrow(|report| {
            assert_eq!(report.as_ref().unwrap()["programs"], json!(["final"]));
        });

        clear_shader_report();
        REPORT.with_borrow(|report| assert!(report.is_none()));
    }

    #[test]
    fn shader_runtime_reports_distinguish_pending_work_from_empty_results() {
        record_shader_report("A.zip", 0, &[], &["Compiling".into()], true);
        REPORT.with_borrow(|report| {
            let report = report.as_ref().unwrap();
            assert_eq!(report["pending"], true);
            assert_eq!(report["programs"], json!([]));
        });
        record_report("A.zip", 0, &[], &["Unsupported program".into()]);
        REPORT.with_borrow_mut(|report| {
            let report = report.take().unwrap();
            assert_eq!(report["pending"], false);
            assert_eq!(report["programs"], json!([]));
        });
    }

    #[test]
    fn failed_native_reload_reports_the_active_previous_programs() {
        record_report("Example.hshader", 0, &["sky".into(), "post".into()], &[]);
        record_reload_error("Example.hshader", 0, &["bad shader".into()], true);
        REPORT.with_borrow(|report| {
            let report = report.as_ref().unwrap();
            assert_eq!(report["programs"], json!(["sky", "post"]));
            assert!(
                report["notes"][0]
                    .as_str()
                    .unwrap()
                    .contains("remain active")
            );
        });
        REPORT.with_borrow_mut(Option::take);
        record_reload_error("Other.hshader", 0, &["bad initial shader".into()], false);
        REPORT.with_borrow(|report| assert_eq!(report.as_ref().unwrap()["programs"], json!([])));
        REPORT.with_borrow_mut(Option::take);
    }
    #[test]
    fn failed_reload_does_not_report_programs_the_renderer_dropped() {
        record_report("Example.hshader", 0, &["sky".into()], &[]);
        record_reload_error("Example.hshader", 0, &["manifest refused".into()], false);
        REPORT.with_borrow(|report| {
            let report = report.as_ref().unwrap();
            assert_eq!(report["programs"], json!([]));
            assert_eq!(report["notes"], json!(["manifest refused"]));
        });
        LAST_DISABLED.with_borrow_mut(|failure| *failure = Some(json!({"pack": "Failed.zip"})));
        assert!(shutdown_parser());
        REPORT.with_borrow(|report| assert!(report.is_none()));
        LAST_DISABLED.with_borrow_mut(|failure| {
            assert_eq!(failure.take(), Some(json!({"pack": "Failed.zip"})));
        });
    }

    #[test]
    fn parser_results_are_nonblocking_and_reach_only_the_owner() {
        let owner = ParserInbox::default();
        let other = ParserInbox::default();
        let (send, result) = std::sync::mpsc::channel();
        let key = LoadKey {
            path: "test.zip".into(),
            stamp: None,
            resources: "resources".into(),
            dimension: 0,
            dimension_name: "minecraft:overworld".into(),
            context: (1, 2),
            options_revision: 0,
        };
        PENDING_LOAD.with_borrow_mut(|pending| {
            *pending = Some(PendingLoad {
                inbox: Arc::downgrade(&owner.0),
                report_name: "test.zip".into(),
                key: key.clone(),
                result,
                worker: None,
            })
        });
        complete_parser();
        assert!(owner.lock().is_none());
        send.send(Err("test error".into()))
            .unwrap_or_else(|_| panic!("receiver dropped"));
        complete_parser();
        assert!(other.lock().is_none());
        assert!(!parser_busy());
        let parsed = owner.lock().take().unwrap();
        assert!(parsed.key == key && parsed.result.is_err());
        assert!(owner.lock().is_none());
    }
}

#[cfg(test)]
mod rendered_item_tests {
    use super::rendered_item_id;

    /// Iris 1.11.7's values (probe packs of ours): a block item reads its block's id, and 0 when
    /// `block.properties` lacks it even if `item.properties` names it (oak planks as item 15);
    /// other items read their `item.properties` id, or 65535 when the file lacks them, and 0
    /// when the pack has no such file.
    #[test]
    fn a_held_item_reads_its_block_id_or_item_id() {
        // Stone: block 21 and item 12.
        assert_eq!(rendered_item_id(Some(21), Some(12), true), 21);
        assert_eq!(rendered_item_id(Some(0), Some(15), true), 0);
        assert_eq!(rendered_item_id(Some(0), None, true), 0);
        assert_eq!(rendered_item_id(Some(0), None, false), 0);
        assert_eq!(rendered_item_id(None, Some(11), true), 11);
        assert_eq!(rendered_item_id(None, None, true), 65535);
        assert_eq!(rendered_item_id(None, None, false), 0);
    }
}

#[cfg(test)]
mod native_pack_tests {
    use super::{PickedPack, active_pack, picked_pack_shape};
    use horizon_client::mod_list::{Installed, ModList, NativePage};

    #[test]
    fn unknown_pack_is_optimistic_but_refusal_owner_and_presence_override_it() {
        use crate::game::native_shaders::{fingerprint, remember};
        use horizon_client::settings::Settings;
        use horizon_shader_api::Blend;
        let mut settings = Settings::default();
        settings.horizon.hdr = horizon_client_render::graphics::HdrMode::On;
        settings.mods.installed.push(Installed {
            id: "frame_owner".into(),
            title: "Frame Owner".into(),
            description: String::new(),
            problem: None,
            dev: false,
            native: Some(NativePage::default()),
        });
        let root =
            std::env::temp_dir().join(format!("horizon-frame-policy-{}", std::process::id()));
        let path = root.join("native-frame_owner");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(horizon_shader_api::MANIFEST),
            "schema = 1\nid = \"fixture\"\nname = \"Fixture\"\nversion = \"1\"\napi = \"0.2\"\n[[post]]\nfile = \"p.wgsl\"\n").unwrap();
        settings.horizon.shader_pack = path.to_string_lossy().into_owned();
        // One of Horizon's own packs with HDR on asks for the layered frame, as no pack does,
        // before it was looked at and once it is taken.
        for taken in [false, true] {
            if taken {
                remember(&path, fingerprint(&path), true);
            }
            let shape = picked_pack_shape(&settings);
            assert_eq!(shape.linear, Some(Blend::Gamma));
            assert!(!shape.shaders_pack);
        }
        // A pack for a shader API line this build doesn't serve asks as no pack does: its
        // manifest doesn't read, and the look that follows refuses it.
        let manifest = path.join(horizon_shader_api::MANIFEST);
        let text = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(&manifest, text.replace("api = \"0.2\"", "api = \"0.1\"")).unwrap();
        let shape = picked_pack_shape(&settings);
        assert!(shape.linear.is_none() && !shape.shaders_pack);
        std::fs::write(&manifest, &text).unwrap();
        // A `shaders/` pack keeps the 8-bit frame under HDR output.
        let shaders_pack = root.join("shaders_pack");
        std::fs::create_dir_all(shaders_pack.join("shaders")).unwrap();
        settings.horizon.shader_pack = shaders_pack.to_string_lossy().into_owned();
        let shape = picked_pack_shape(&settings);
        assert!(shape.linear.is_none() && shape.shaders_pack);
        settings.horizon.shader_pack = path.to_string_lossy().into_owned();
        // The frame's blend comes with it: the frame a pack that changes it asks for is
        // another, so the game makes the renderer again (`follow_frame`).
        std::fs::write(&manifest, text.clone() + "[frame]\nblend = \"linear\"\n").unwrap();
        remember(&path, fingerprint(&path), true);
        let shape = picked_pack_shape(&settings);
        assert_eq!(shape.linear, Some(Blend::Linear));
        assert!(shape.frame().linear_blend);
        std::fs::write(&manifest, text).unwrap();
        remember(&path, fingerprint(&path), true);
        assert!(!picked_pack_shape(&settings).frame().linear_blend);
        settings.mods.disabled.insert("frame_owner".into());
        let shape = picked_pack_shape(&settings);
        assert_eq!(shape.linear, None);
        assert!(!shape.shaders_pack);
        settings.mods.disabled.clear();
        assert!(picked_pack_shape(&settings).linear.is_some());
        remember(&path, fingerprint(&path), false);
        let refused = picked_pack_shape(&settings);
        assert!(refused.linear.is_none() && !refused.shaders_pack);
        remember(&path, fingerprint(&path), true);
        std::fs::remove_dir_all(root).unwrap();
        let missing = picked_pack_shape(&settings);
        assert_eq!(missing.linear, None);
        assert!(!missing.shaders_pack);
    }

    /// Spec #16011 item 5: the device asks for ray queries when the pick's manifest declares
    /// `[ray_tracing]`, in a folder or a package; not for another pack, a classic, missing or
    /// refused one, nor while the mod that owns it is off.
    #[test]
    fn a_picked_pack_with_ray_tracing_asks_the_device_for_ray_queries() {
        use crate::game::native_shaders::{fingerprint, remember};
        use horizon_client::settings::Settings;
        let root = std::env::temp_dir().join(format!("horizon-rt-pick-{}", std::process::id()));
        let manifest = |name: &str, extra: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let text = format!(
                "schema = 1\nid = \"fixture\"\nname = \"N\"\nversion = \"1\"\napi = \"0.2\"\n\
                 {extra}[[post]]\nfile = \"p.wgsl\"\n"
            );
            std::fs::write(dir.join(horizon_shader_api::MANIFEST), text).unwrap();
            dir
        };
        let traced = manifest("native-rt_owner", "[ray_tracing]\n");
        let plain = manifest("plain", "");
        let classic = root.join("classic");
        std::fs::create_dir_all(classic.join("shaders")).unwrap();
        let sample = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../horizon-shader-api/packs/traced_shadows");
        let package = root.join("traced.hshader");
        let bytes = horizon_shader_api::Pack::read(&sample)
            .unwrap()
            .package()
            .unwrap();
        std::fs::write(&package, bytes).unwrap();
        let mut settings = Settings::default();
        settings.mods.installed.push(Installed {
            id: "rt_owner".into(),
            title: "RT Owner".into(),
            description: String::new(),
            problem: None,
            dev: false,
            native: Some(NativePage::default()),
        });
        let mut asks = |path: &std::path::Path| {
            settings.horizon.shader_pack = path.to_string_lossy().into_owned();
            picked_pack_shape(&settings).frame().ray_tracing
        };
        assert!(asks(&traced));
        assert!(asks(&package));
        assert!(!asks(&plain));
        assert!(!asks(&classic));
        assert!(!asks(&root.join("missing")));
        assert!(!asks(std::path::Path::new("")));
        remember(&package, fingerprint(&package), false);
        assert!(!asks(&package));
        settings.mods.disabled.insert("rt_owner".into());
        settings.horizon.shader_pack = traced.to_string_lossy().into_owned();
        assert!(!picked_pack_shape(&settings).ray_tracing);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A running pack whose new version fails in a stage keeps drawing its last good one: the
    /// pick asks for the frame it asked for, with HDR off and on alike, so the renderer isn't
    /// made again. A pack that never ran, or one whose manifest is refused, asks as no pack.
    #[test]
    fn a_stage_that_fails_keeps_the_running_packs_frame() {
        use crate::game::native_shaders::{fingerprint, remember_look};
        use horizon_client::settings::Settings;
        use horizon_client_render::graphics::HdrMode;
        let root = std::env::temp_dir().join(format!("horizon-kept-frame-{}", std::process::id()));
        let path = root.join("kept");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(horizon_shader_api::MANIFEST),
            "schema = 1\nid = \"kept\"\nname = \"Kept\"\nversion = \"1\"\napi = \"0.2\"\n[[post]]\nfile = \"p.wgsl\"\n").unwrap();
        std::fs::write(path.join("p.wgsl"), "a stage that fails the check").unwrap();
        let mut settings = Settings::default();
        settings.horizon.shader_pack = path.to_string_lossy().into_owned();
        for hdr in [HdrMode::Off, HdrMode::On] {
            settings.horizon.hdr = hdr;
            remember_look(&path, fingerprint(&path), true, false, false);
            let running = picked_pack_shape(&settings).frame();
            assert!(running.linear_pack);
            // The stage fails while that version runs.
            remember_look(&path, fingerprint(&path), false, false, true);
            assert_eq!(picked_pack_shape(&settings).frame(), running, "{hdr:?}");
            // It fails with nothing running, or the manifest is refused while it runs.
            for (manifest_refused, running) in [(false, false), (true, true)] {
                remember_look(&path, fingerprint(&path), false, manifest_refused, running);
                let none = picked_pack_shape(&settings).frame();
                assert_eq!(none, Default::default(), "{hdr:?}");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shaders_packs_and_packs_with_a_shadow_pass_draw_flat() {
        let root = std::env::temp_dir().join(format!("horizon-flat-packs-{}", std::process::id()));
        // A folder without a manifest is a `shaders/` pack; with one, a pack of Horizon's own.
        let pack = |name: &str, manifest: Option<&str>| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(extra) = manifest {
                std::fs::write(dir.join("material.wgsl"), "").unwrap();
                let text = format!(
                    "schema = 1\nid = \"{name}\"\nname = \"{name}\"\nversion = \"1\"\napi = \"{}\"\n\
                     [material]\nfile = \"material.wgsl\"\n{extra}",
                    horizon_shader_api::API
                );
                std::fs::write(dir.join(horizon_shader_api::MANIFEST), text).unwrap();
            }
            dir
        };
        let classic = PickedPack::read(&pack("shaders_pack", None)).unwrap();
        assert!(classic.draws_flat && classic.starts_wide);
        let unshadowed = PickedPack::read(&pack("unshadowed", Some(""))).unwrap();
        assert!(!unshadowed.draws_flat && !unshadowed.starts_wide);
        let shadowed = PickedPack::read(&pack("shadowed", Some("[shadow]\n"))).unwrap();
        assert!(shadowed.draws_flat && !shadowed.starts_wide);
        let displaced = PickedPack::read(&pack("displaced", Some("[displacement]\n"))).unwrap();
        assert!(!displaced.starts_wide);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn native_shader_follows_its_mod_without_affecting_other_packs() {
        let mut mods = ModList::default();
        mods.installed.push(Installed {
            id: "horizon_better_visuals".into(),
            title: "Better Visuals".into(),
            description: String::new(),
            problem: None,
            dev: false,
            native: Some(NativePage::default()),
        });
        let owned = "native-horizon_better_visuals";
        assert_eq!(active_pack(owned, &mods), owned);
        mods.disabled.insert("horizon_better_visuals".into());
        assert_eq!(active_pack(owned, &mods), "");
        let root =
            std::env::temp_dir().join(format!("horizon-bundled-shader-{}", std::process::id()));
        let pack = root.join(owned);
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join(horizon_shader_api::MANIFEST),
            "schema = 1\nid = \"bundled\"\nname = \"Bundled\"\nversion = \"1\"\napi = \"0.2\"\n[[post]]\nfile = \"p.wgsl\"\n").unwrap();
        let (key, path) = horizon_client::shader_pack_list::resolve(&root, owned);
        assert_eq!(key, "horizon:bundled");
        assert_eq!(path, pack);
        assert_eq!(
            active_pack(&path.file_name().unwrap().to_string_lossy(), &mods),
            ""
        );
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(active_pack("golden_hour", &mods), "golden_hour");
        mods.installed[0].native = None;
        assert_eq!(active_pack(owned, &mods), owned);
    }
}

#[cfg(test)]
mod fluid_pair_tests {
    use super::fluid_pairs;
    #[test]
    fn terrain_reads_y_1_on_fluids_and_0_elsewhere() {
        assert_eq!(fluid_pairs(4, [1, 3]), [-1, 0, -1, 1, -1, 0, -1, 1]);
        assert_eq!(fluid_pairs(2, []), [-1, 0, -1, 0]);
    }
}
