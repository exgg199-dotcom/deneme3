//! Horizon's own shader packs in the game (docs/shaders.md): the pick in `shaderpacks/` (a
//! folder with `horizon-shaders.toml`, or a `.hshader`; `game/interface/shader_packs.rs`) runs once it
//! passes `horizon_shader_api::check`. The pack is looked at twice a second: a saved file (or
//! saved option values) is checked and the pack rebuilt, and a
//! pack that fails is refused with its problems in the chat. Where a stage fails, the last
//! good version keeps drawing, on the frame it has: the pick keeps asking for it
//! (`remember_look`). A pack whose manifest is refused (`manifest_refused`) stops drawing
//! instead: the manifest decides the frame, the one that ran was built for another, and the
//! pick asks for its frame as no pack does (`picked_pack_shape`).

use std::{
    hash::Hasher,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use horizon_client::gfx::native_entity::{EntityKind, EntityRuns};
use horizon_client::text::Text;
use horizon_shader_api::{Manifest, Pack, Primaries, check};

use super::{
    Game,
    shader_packs::{ClassicPack, PickedPack, classic_options_path},
};

const LOOK_EVERY: Duration = Duration::from_millis(500);
const RED: u32 = 0xFF5555;

thread_local! {
    // Loader outcomes survive renderer replacement; rejected files must not cause a rebuild loop.
    // Each is the pack's files (`fingerprint`) and whether a version of the pack draws since:
    // those files, or the last good ones before them (`remember_look`).
    static OUTCOMES: std::cell::RefCell<std::collections::HashMap<PathBuf, (Option<u64>, bool)>> =
        std::cell::RefCell::default();
}

/// Whether a version of the pack at `path` draws since it was last looked at: `None` before
/// that.
pub(super) fn outcome(path: &Path) -> Option<bool> {
    OUTCOMES.with_borrow(|packs| packs.get(path).map(|(_, draws)| *draws))
}

pub(super) fn remember(path: &Path, stamp: Option<u64>, draws: bool) {
    OUTCOMES.with_borrow_mut(|packs| packs.insert(path.to_owned(), (stamp, draws)));
}

/// Clear only this pack's cached refusal after an explicit retry.
pub(super) fn clear_refusal(path: &Path) -> bool {
    OUTCOMES.with_borrow_mut(|packs| {
        if packs.get(path).is_some_and(|&(_, draws)| !draws) {
            packs.remove(path);
            true
        } else {
            false
        }
    })
}

/// Remembers a look at one of Horizon's own packs: its files were `taken`, or they failed
/// while a version of the pack was `running`. A stage that fails leaves that version drawing,
/// so the pick keeps asking for its frame and the renderer isn't made again; a refused
/// manifest stops it (`manifest_refused`), and a pack that never ran has nothing to keep.
pub(super) fn remember_look(
    path: &Path,
    stamp: Option<u64>,
    taken: bool,
    manifest_refused: bool,
    running: bool,
) {
    remember(path, stamp, taken || (running && !manifest_refused));
}

fn refused(path: &Path, stamp: Option<u64>) -> bool {
    OUTCOMES.with_borrow(|packs| packs.get(path) == Some(&(stamp, false)))
}

#[derive(Default)]
pub(super) struct NativeShaders {
    /// The resolved pack path, of either kind (`game/interface/shader_packs.rs`); empty: none.
    pub picked: String,
    /// The classic pack being compiled or running on the current renderer.
    pub classic: Option<ClassicPack>,
    classic_source_checked_at: Option<Instant>,
    /// The pack's files when it was last read (`fingerprint`).
    stamp: Option<u64>,
    looked: Option<Instant>,
    selection: String,
    resolved: PathBuf,
    resolved_at: Option<Instant>,
    frame_pack: Option<Option<PickedPack>>,
    /// The runs of this frame's entity geometry that the pack's `[entity]` stage lights
    /// (`Game::lit_entity`), up to the renderer taking them (`Renderer::take_entity_runs`).
    pub entity_runs: std::cell::RefCell<EntityRuns>,
    /// The same runs' vertices posed on the GPU, counted as `SkinFrame::vertices` counts them.
    pub posed_runs: std::cell::RefCell<EntityRuns>,
    /// While the ray scene has the entity layer: the camera-relative box of its reach this frame
    /// (`Renderer::ray_entity_reach`), and the models of the entities within it that the camera
    /// doesn't draw, with their runs (`Game::ray_only`), up to the renderer taking them.
    pub ray_reach: Option<horizon_client::gfx::Frustum>,
    pub ray_only: std::cell::RefCell<Vec<horizon_client::mesher::Vertex>>,
    pub ray_runs: std::cell::RefCell<EntityRuns>,
    /// Set while an entity of the layer's kinds is culled (`Game::culled_for_rays`): occlusion
    /// then moves its model to `ray_only` instead of dropping it.
    pub ray_keeps: std::cell::Cell<bool>,
}

impl NativeShaders {
    /// Gives `renderer` the frame's entity runs and its ray-only entities.
    pub fn hand_over(&mut self, renderer: &mut horizon_client::gfx::Renderer) {
        renderer.take_entity_runs(self.entity_runs.get_mut(), self.posed_runs.get_mut());
        renderer.take_ray_only(self.ray_only.get_mut(), self.ray_runs.get_mut());
    }

    pub fn resolve_path(&mut self, dir: &Path, picked: &str, filesystem: bool) -> PathBuf {
        if self.selection != picked || self.resolved_at.is_none_or(|t| t.elapsed() >= LOOK_EVERY) {
            let resolved = if filesystem {
                dir.join(picked)
            } else {
                horizon_client::shader_pack_list::resolve(dir, picked).1
            };
            // A manifest being edited may be temporarily invalid. Keep its path so the
            // reload reports the error.
            let unresolved =
                !filesystem && picked.starts_with("horizon:") && resolved == dir.join(picked);
            let keep_path = unresolved
                && self.selection == picked
                && self.resolved_at.is_some()
                && Pack::read_manifest(&self.resolved).is_err();
            if !keep_path {
                self.resolved = resolved;
            }
            self.selection = picked.to_owned();
            self.resolved_at = Some(Instant::now());
            self.frame_pack = None;
        }
        self.resolved.clone()
    }

    /// The selected pack's frame requirements, refreshed with its resolved path.
    pub fn frame_pack(&mut self) -> Option<PickedPack> {
        *self
            .frame_pack
            .get_or_insert_with(|| PickedPack::read(&self.resolved))
    }

    pub fn select(&mut self, name: &str) {
        self.picked = name.to_owned();
        self.classic = None;
        self.classic_source_checked_at = None;
        self.stamp = None;
        self.looked = None;
    }

    /// Check at load boundaries and periodically watch running ZIPs and refused packs.
    pub fn check_classic_source(
        &mut self,
        path: &Path,
        dimension_loaded: bool,
        parser_busy: bool,
    ) -> Option<Option<u64>> {
        if parser_busy {
            return None;
        }
        let loaded_stamp = if dimension_loaded {
            self.classic
                .as_ref()
                .and_then(|pack| pack.watched_stamp(path))
        } else {
            None
        };
        let was_refused = outcome(path) == Some(false);
        if was_refused || loaded_stamp.is_some() {
            let now = Instant::now();
            if self
                .classic_source_checked_at
                .is_some_and(|last| now.duration_since(last) < LOOK_EVERY)
            {
                return None;
            }
            self.classic_source_checked_at = Some(now);
        } else {
            self.classic_source_checked_at = None;
            if dimension_loaded {
                return None;
            }
        }
        let stamp = fingerprint(path);
        if loaded_stamp == Some(stamp) || refused(path, stamp) {
            return None;
        }
        if was_refused {
            OUTCOMES.with_borrow_mut(|packs| packs.remove(path));
        }
        Some(stamp)
    }

    pub fn picking(name: &str) -> Self {
        Self {
            picked: name.to_owned(),
            ..Self::default()
        }
    }
}

impl Game<'_> {
    /// The pack's `[entity]` stage lights vertices `run` of the entity geometry as `kind`. A
    /// run is named once it is drawn, and with its end: what no caller names stays Horizon's
    /// own draw (items, particles, fire, beams, ropes, text, and whatever is added without a
    /// word here), so a run ends before any of those. Within a run, quads of another batch
    /// material (a translucent item in a hand, an energy swirl) and unlit overlays (the beam
    /// that heals the dragon, an End portal) stay Horizon's all the same: `EntityBatch` names
    /// only its ordinary quads, and the stage's shader passes the unlit ones on.
    /// The models the run posed on the GPU (`entity_skin`) are the vertices it drew there
    /// from `posed` on (`SkinFrame::vertices` when it began): the stage lights them alike.
    /// Kept only while the pack in use has the stage, or its rays read the entity layer
    /// (`names_entity_runs`).
    fn lit_run(&self, run: std::ops::Range<usize>, posed: u32, kind: EntityKind) {
        let stage = self.renderer.as_ref();
        if stage.is_some_and(|r| r.names_entity_runs()) {
            self.native_shaders.entity_runs.borrow_mut().push(run, kind);
            let posed = posed as usize..self.skin_frame.borrow().vertices as usize;
            self.native_shaders
                .posed_runs
                .borrow_mut()
                .push(posed, kind);
        }
    }

    /// Where the next run's posed vertices start (`lit_posed_entity`).
    pub(super) fn posed_vertices(&self) -> u32 {
        self.skin_frame.borrow().vertices
    }

    /// Vertices `run` are an entity's model: a mob's or a player's with what it holds and
    /// wears, a vehicle's, a projectile's.
    pub(super) fn lit_entity(&self, run: std::ops::Range<usize>) {
        self.lit_run(run, self.posed_vertices(), EntityKind::Entity);
    }

    /// [`Self::lit_entity`] of a run that may have posed models on the GPU since its posed
    /// vertices numbered `posed` (`posed_vertices`).
    pub(super) fn lit_posed_entity(&self, run: std::ops::Range<usize>, posed: u32) {
        self.lit_run(run, posed, EntityKind::Entity);
    }

    /// Vertices `run` are block entities' models.
    pub(super) fn lit_block_entity(&self, run: std::ops::Range<usize>) {
        self.lit_run(run, self.posed_vertices(), EntityKind::BlockEntity);
    }

    /// What `draw` adds to `out` is entities' models (`lit_entity`), on the GPU too.
    pub(super) fn lit(
        &mut self,
        out: &mut Vec<horizon_client::mesher::Vertex>,
        draw: impl FnOnce(&mut Self, &mut Vec<horizon_client::mesher::Vertex>),
    ) {
        let (start, posed) = (out.len(), self.posed_vertices());
        draw(self, out);
        self.lit_posed_entity(start..out.len(), posed);
    }

    /// Builds the pack at `path` when its files changed since it was last read.
    pub(in crate::game) fn tick_native_shader_pack(&mut self, path: &Path) {
        // A reload would replace the pack a `screenshot-sequence` draws: it waits for the reply.
        if self.screenshot_sequence.is_some() {
            return;
        }
        let now = Instant::now();
        let state = &mut self.native_shaders;
        if state.looked.is_some_and(|t| now - t < LOOK_EVERY) {
            return;
        }
        state.looked = Some(now);
        let stamp = fingerprint(path);
        if stamp == state.stamp {
            // A pack gone (no stamp) is never read: a renderer made again holds the world for
            // nothing (`release_world`, which leaves a running pack's alone).
            if stamp.is_none()
                && let Some(renderer) = &mut self.renderer
            {
                renderer.release_world();
            }
            return;
        }
        if refused(path, stamp) {
            // A renderer made again for this pack holds the world for it (`release_world`).
            if let Some(renderer) = &mut self.renderer {
                renderer.release_world();
            }
            return;
        }
        let reload = state.stamp.is_some();
        state.stamp = stamp;
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let manifest_refused = manifest_refused(path);
        let running = renderer.shader_pack_kind() == Some("native");
        let read = match &manifest_refused {
            Some(why) => Err(vec![why.clone()]),
            None => Pack::read(path).map_err(|e| vec![e]),
        };
        let result = read.and_then(|pack| {
            let report = check(&pack)
                .map_err(|p| p.iter().map(ToString::to_string).collect::<Vec<String>>())?;
            // The player's values (the pack's Shader Options page), else the defaults.
            let options = &pack.manifest.options;
            let values_pack =
                horizon_client::shader_pack_list::native_values_pack(path, &pack.manifest.id);
            let values = horizon_client::shader_pack_list::load_values(&values_pack, options);
            let constants = options.iter().map(|o| o.id.clone()).zip(values).collect();
            let sprites = || {
                let layers = self.pack.render.sprite_layers.iter();
                layers.map(|(name, layer)| (name.as_str(), *layer))
            };
            let tags = pack.manifest.sprite_tags(sprites());
            let mut materials = pack.manifest.sprite_materials(sprites());
            // `blocks` lists: the sprites their models draw with, where no pattern named them.
            let listed = pack.manifest.material_blocks();
            if !listed.is_empty() {
                let macros = horizon_shaders::Macros::standard();
                let properties = horizon_shaders::Properties::parse(&listed, &macros);
                let ids = horizon_shaders::BlockIds::parse(&properties);
                let table = super::shader_packs::block_id_table(&self.pack, &ids);
                for (layer, id) in table.iter().step_by(2).enumerate() {
                    let layer = layer as u32;
                    if *id > 0 && !materials.iter().any(|m| m.0 == layer) {
                        materials.push((layer, *id as u32));
                    }
                }
                materials.sort_unstable();
            }
            let layers = sprites().map(|(_, l)| l as usize + 1).max().unwrap_or(0);
            let translucent = pack.manifest.translucent_layers(&materials, layers);
            // Each light-giving block state's `[[lights]]` colour, with its level.
            let mut lights = horizon_client::gfx::block_light::LightTable::default();
            if !pack.manifest.lights.is_empty() {
                for (id, state) in self.res.facts().states.iter().enumerate() {
                    if state.emission == 0 {
                        continue;
                    }
                    if let Some(color) = pack.manifest.light_of(&state.name) {
                        lights.insert(id as u32, (color, state.emission));
                    }
                }
            }
            let labels = renderer
                .set_native_shader_pack(Some((report, constants, tags, materials, lights)))
                .map_err(|e| vec![e])?;
            Ok((pack.manifest, labels, translucent))
        });
        remember_look(
            path,
            stamp,
            result.is_ok(),
            manifest_refused.is_some(),
            running,
        );
        // The resources were loaded without a native pack, so without the resource packs'
        // LabPBR maps its material stage reads: load them once, now that one runs.
        if result.as_ref().is_ok_and(|(m, _, _)| m.material.is_some())
            && !horizon_client::sprites::labpbr_maps()
        {
            horizon_client::sprites::set_labpbr_maps(true);
            log::info!("Horizon: reloading resources for the LabPBR maps a native pack reads");
            self.reload_packs();
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        match result {
            Ok((m, labels, translucent)) => {
                self.set_translucent_layers(translucent);
                let dimension = match self.dim {
                    super::Dim::Overworld | super::Dim::Extra(_) => 0,
                    super::Dim::Nether => -1,
                    super::Dim::End => 1,
                };
                super::shader_packs::record_report(&name, dimension, &labels, &[]);
                log::info!(
                    "shader pack {} {}: {} in {} ms",
                    m.id,
                    m.version,
                    labels.join(", "),
                    now.elapsed().as_millis()
                );
                if reload {
                    self.system_message(Text::literal(format!("Shader pack {} reloaded", m.name)));
                }
            }
            Err(problems) => {
                let dimension = match self.dim {
                    super::Dim::Overworld | super::Dim::Extra(_) => 0,
                    super::Dim::Nether => -1,
                    super::Dim::End => 1,
                };
                if manifest_refused.is_some() {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.drop_shader_packs();
                    }
                    self.set_translucent_layers(Vec::new());
                } else if let Some(renderer) = &mut self.renderer {
                    renderer.release_world();
                }
                super::shader_packs::record_reload_error(
                    &name,
                    dimension,
                    &problems,
                    running && manifest_refused.is_none(),
                );
                log::warn!("shader pack {name} refused: {}", problems.join("; "));
                self.system_message(Text::colored(format!("Shader pack {name} refused:"), RED));
                for p in problems {
                    self.system_message(Text::colored(p, RED));
                }
            }
        }
    }
}

/// Why the manifest of the pack at `path` is refused, if it is: it doesn't read (a shader API
/// line this build doesn't serve, a mistake in it), or it asks for what isn't drawn yet
/// (`not_drawn`). A pack that ran before stops then; one whose stages fail keeps its last good
/// version drawing.
fn manifest_refused(path: &Path) -> Option<String> {
    match Pack::read_manifest(path) {
        Ok(manifest) => not_drawn(&manifest).map(Into::into),
        Err(e) => Some(e),
    }
}

/// Why the game doesn't draw a pack the check may still pass: `[frame] primaries = "bt2020"`
/// isn't served yet (#14584), and neither is `[motion]`. Serving `"bt2020"` must also keep HDR's
/// Color Boost (`gfx::hdr::gamut`) off for that pack: it stretches BT.709 colours, and a BT.2020
/// pack's are already where it meant them.
fn not_drawn(m: &Manifest) -> Option<&'static str> {
    if m.primaries == Primaries::Bt2020 {
        return Some(
            "[frame] primaries = \"bt2020\" isn't served yet: leave it out, or set \"bt709\"",
        );
    }
    m.motion.is_some().then_some(
        "[motion] needs a later Horizon: this one checks the motion guide but doesn't draw it",
    )
}

#[path = "native_shaders/source_fingerprint.rs"]
mod source_fingerprint;

/// A hash of the names, sizes and modification times of the pack's files and of its saved
/// option values (`None` when the pack is unreadable).
pub(super) fn fingerprint(path: &Path) -> Option<u64> {
    let mut hasher = std::hash::DefaultHasher::new();
    source_fingerprint::walk(path, &mut hasher)?;
    // The player's option values, when there are any.
    let values = if horizon_shader_api::package::is_pack(path) {
        Pack::read_manifest(path).ok().map(|manifest| {
            let values_pack =
                horizon_client::shader_pack_list::native_values_pack(path, &manifest.id);
            horizon_client::shader_pack_list::values_path(&values_pack)
        })
    } else {
        classic_options_path(path)
    };
    if let Some(values) = values.filter(|values| values.is_file()) {
        let _ = source_fingerprint::walk(&values, &mut hasher);
    }
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::not_drawn;
    use horizon_shader_api::Manifest;

    #[test]
    fn outcomes_and_refusals_survive_renderer_resets() {
        use super::{NativeShaders, fingerprint, outcome, refused, remember};
        let root = std::env::temp_dir().join(format!("horizon-acceptance-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("post.wgsl");
        std::fs::write(&file, "first files").unwrap();
        let stamp = fingerprint(&root);
        assert_eq!(outcome(&root), None);
        remember(&root, stamp, true);
        assert_eq!(outcome(&root), Some(true));
        // Policy, SDK and GPU refusals all reach this same outcome before any resource reload.
        remember(&root, stamp, false);
        for _ in 0..3 {
            let mut reset = NativeShaders::picking(&root.to_string_lossy());
            reset.select(&root.to_string_lossy());
            assert_eq!(outcome(&root), Some(false));
            assert!(refused(&root, fingerprint(&root)));
        }
        std::fs::write(&file, "repaired files with a different size").unwrap();
        assert!(!refused(&root, fingerprint(&root)));
        remember(&root, fingerprint(&root), true);
        assert_eq!(outcome(&root), Some(true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classic_options_changes_invalidate_the_shader_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Example.zip");
        let options = super::classic_options_path(&path).unwrap();
        std::fs::write(&path, b"synthetic shader source").unwrap();
        let initial = super::fingerprint(&path).unwrap();

        std::fs::write(&options, b"SHADOWS=true").unwrap();
        let selected = super::fingerprint(&path).unwrap();
        assert_ne!(initial, selected);
        std::fs::write(&options, b"SHADOWS=false").unwrap();
        assert_ne!(selected, super::fingerprint(&path).unwrap());

        std::fs::remove_file(options).unwrap();
        assert_eq!(initial, super::fingerprint(&path).unwrap());
    }

    #[test]
    fn explicit_retry_rechecks_the_same_shader_files_once() {
        use super::{NativeShaders, clear_refusal, fingerprint, outcome, remember};

        let root = tempfile::tempdir().unwrap();
        let failed = root.path().join("failed.zip");
        let healthy = root.path().join("healthy.zip");
        std::fs::write(&failed, "stable source").unwrap();
        std::fs::write(&healthy, "different source").unwrap();
        let stamp = fingerprint(&failed);
        remember(&failed, stamp, false);
        remember(&healthy, fingerprint(&healthy), true);

        let mut state = NativeShaders::default();
        assert_eq!(state.check_classic_source(&failed, false, false), None);
        assert!(clear_refusal(&failed));
        assert!(!clear_refusal(&failed));
        assert!(!clear_refusal(&healthy));
        assert_eq!(outcome(&healthy), Some(true));
        assert_eq!(
            state.check_classic_source(&failed, false, false),
            Some(stamp)
        );

        remember(&failed, stamp, false);
        assert_eq!(state.check_classic_source(&failed, false, false), None);
    }

    #[test]
    fn a_dimension_refusal_expires_with_source_or_renderer_changes() {
        use super::{ClassicPack, LOOK_EVERY, NativeShaders, fingerprint, outcome};
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dimension-shaders.zip");
        std::fs::write(&path, "initial source").unwrap();
        let stamp = fingerprint(&path);
        let mut state = NativeShaders {
            classic: Some(ClassicPack::refusal(&path, stamp, "server:no_effects")),
            ..NativeShaders::default()
        };
        assert_eq!(state.check_classic_source(&path, true, false), None);
        assert_eq!(outcome(&path), None);
        assert_eq!(state.check_classic_source(&path, false, false), Some(stamp));

        state.classic_source_checked_at = Some(Instant::now() - LOOK_EVERY);
        std::fs::write(&path, "different source with supported inputs").unwrap();
        assert_ne!(fingerprint(&path), stamp);
        assert_eq!(
            state.check_classic_source(&path, true, false),
            Some(fingerprint(&path))
        );
    }

    #[test]
    fn classic_source_checks_only_at_load_boundaries_and_retries_changed_refusals() {
        use super::{LOOK_EVERY, NativeShaders, fingerprint, outcome, remember};
        use std::time::Instant;

        let pack =
            std::env::temp_dir().join(format!("horizon-classic-retry-{}", std::process::id()));
        std::fs::create_dir_all(&pack).unwrap();
        let file = pack.join("shader.fsh");
        std::fs::write(&file, "original").unwrap();
        let original = fingerprint(&pack);
        assert!(original.is_some());
        let mut state = NativeShaders::default();

        assert_eq!(state.check_classic_source(&pack, false, true), None);
        assert_eq!(
            state.check_classic_source(&pack, false, false),
            Some(original)
        );
        remember(&pack, original, true);
        assert_eq!(state.check_classic_source(&pack, true, false), None);

        remember(&pack, original, false);
        assert_eq!(state.check_classic_source(&pack, true, false), None);
        let checked_at = state.classic_source_checked_at;
        assert!(checked_at.is_some());
        std::fs::write(&file, "a repaired source with a different size").unwrap();
        assert_eq!(state.check_classic_source(&pack, true, true), None);
        assert_eq!(state.classic_source_checked_at, checked_at);

        state.classic_source_checked_at = Some(Instant::now() - LOOK_EVERY);
        let replacement = fingerprint(&pack);
        assert_ne!(replacement, original);
        assert_eq!(
            state.check_classic_source(&pack, true, false),
            Some(replacement)
        );
        assert_eq!(outcome(&pack), None);
        assert_eq!(state.check_classic_source(&pack, true, false), None);
        std::fs::remove_dir_all(pack).unwrap();
    }

    #[test]
    fn selected_frame_pack_refreshes_with_resolution_and_caches_missing_packs() {
        use super::{LOOK_EVERY, NativeShaders};
        use std::time::Instant;

        let root = std::env::temp_dir().join(format!("horizon-frame-cache-{}", std::process::id()));
        let path = root.join("classic");
        let mut state = NativeShaders::default();
        assert_eq!(state.resolve_path(&root, "classic", true), path);
        assert!(state.frame_pack().is_none());

        std::fs::create_dir_all(&path).unwrap();
        assert!(state.frame_pack().is_none());
        state.resolved_at = Some(Instant::now() - LOOK_EVERY);
        assert_eq!(state.resolve_path(&root, "classic", true), path);
        assert!(state.frame_pack().unwrap().shaders_pack);

        let manifest = path.join(horizon_shader_api::MANIFEST);
        std::fs::write(
            manifest,
            format!(
                "schema = 1\nid = \"fixture\"\nname = \"Fixture\"\nversion = \"1\"\napi = \"{}\"\n\
                 [frame]\nblend = \"linear\"\n[[post]]\nfile = \"p.wgsl\"\n",
                horizon_shader_api::API
            ),
        )
        .unwrap();
        assert!(state.frame_pack().unwrap().shaders_pack);
        state.resolved_at = Some(Instant::now() - LOOK_EVERY);
        state.resolve_path(&root, "classic", true);
        assert_eq!(
            state.frame_pack().unwrap().linear,
            Some(horizon_shader_api::Blend::Linear)
        );

        std::fs::remove_dir_all(&root).unwrap();
        assert!(state.frame_pack().unwrap().linear.is_some());
        assert_eq!(
            state.resolve_path(&root, "another", true),
            root.join("another")
        );
        assert!(state.frame_pack().is_none());
    }

    fn manifest(extra: &str) -> Manifest {
        Manifest::parse(&format!(
            "schema = 1\nid = \"p\"\nname = \"P\"\nversion = \"1\"\napi = \"0.2\"\n\
             [[post]]\nfile = \"post.wgsl\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn stable_options_are_in_the_reload_signature_and_override_keeps_its_path() {
        use super::{NativeShaders, fingerprint};
        let dir =
            std::env::temp_dir().join(format!("horizon-native-identity-{}", std::process::id()));
        let pack = dir.join("native-test_mod");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join(horizon_shader_api::MANIFEST),
            "schema = 1\nid = \"p\"\nname = \"P\"\nversion = \"1\"\napi = \"0.2\"\n[[post]]\nfile = \"post.wgsl\"\n").unwrap();
        std::fs::write(pack.join("post.wgsl"), "fixture").unwrap();
        let mut state = NativeShaders::default();
        assert_eq!(state.resolve_path(&dir, "horizon:p", false), pack);
        let first = fingerprint(&pack).unwrap();
        let manifest_path = pack.join(horizon_shader_api::MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).unwrap();
        std::fs::write(&manifest_path, "temporarily invalid").unwrap();
        state.resolved_at = Some(std::time::Instant::now() - super::LOOK_EVERY);
        assert_eq!(state.resolve_path(&dir, "horizon:p", false), pack);
        assert_ne!(fingerprint(&pack).unwrap(), first);
        std::fs::write(&manifest_path, manifest).unwrap();
        let first = fingerprint(&pack).unwrap();
        let values = horizon_client::shader_pack_list::native_values_pack(&pack, "p");
        let path = horizon_client::shader_pack_list::values_path(&values);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "strength=2\n").unwrap();
        assert_ne!(fingerprint(&pack).unwrap(), first);
        std::fs::rename(&pack, dir.join("renamed")).unwrap();
        state.resolved_at = None;
        assert_eq!(
            state.resolve_path(&dir, "horizon:p", false),
            dir.join("renamed")
        );
        let manifest_path = dir.join("renamed").join(horizon_shader_api::MANIFEST);
        let text = std::fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("id = \"p\"", "id = \"other\"");
        std::fs::write(manifest_path, text).unwrap();
        state.resolved_at = Some(std::time::Instant::now() - super::LOOK_EVERY);
        assert_eq!(
            state.resolve_path(&dir, "horizon:p", false),
            dir.join("horizon:p"),
            "a valid different manifest ID must not replace the selected identity"
        );
        let mut override_state = NativeShaders::default();
        assert_eq!(
            override_state.resolve_path(&dir, "native-test_mod", true),
            pack,
            "an explicit missing filesystem override must not switch to another installed pack"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Every way a manifest is refused stops the pack that ran: another API line, what isn't
    /// drawn yet, a mistake in it. A stage that fails is not one of them: its last good
    /// version keeps drawing.
    #[test]
    fn a_refused_manifest_stops_the_pack_and_a_failing_stage_does_not() {
        use super::manifest_refused;
        let pack = std::env::temp_dir().join(format!("horizon-refused-{}", std::process::id()));
        std::fs::create_dir_all(&pack).unwrap();
        let write = |api: &str, extra: &str| {
            let text = format!(
                "schema = 1\nid = \"p\"\nname = \"P\"\nversion = \"1\"\napi = \"{api}\"\n\
                 [[post]]\nfile = \"post.wgsl\"\n{extra}"
            );
            std::fs::write(pack.join(horizon_shader_api::MANIFEST), text).unwrap();
        };
        // A stage the check refuses: the manifest itself is taken.
        std::fs::write(pack.join("post.wgsl"), "not wgsl").unwrap();
        write("0.2", "");
        assert_eq!(manifest_refused(&pack), None);
        assert!(
            horizon_shader_api::check(&horizon_shader_api::Pack::read(&pack).unwrap()).is_err()
        );
        for (api, extra, why) in [
            ("0.1", "", "needs shader API 0.1; this Horizon serves 0.2"),
            (
                "0.2",
                "[frame]\nblend = \"linear\"\nprimaries = \"bt2020\"\n",
                "primaries = \"bt2020\" isn't served yet",
            ),
            ("0.2", "[frame\n", "horizon-shaders.toml"),
        ] {
            write(api, extra);
            let refused = manifest_refused(&pack).unwrap_or_default();
            assert!(refused.contains(why), "{api} {extra}: {refused}");
        }
        std::fs::remove_dir_all(pack).unwrap();
    }

    #[test]
    fn a_pack_is_drawn_but_for_what_isnt_served_yet() {
        // Either blend: `"linear"` is drawn as `"gamma"` is. The entity stage is drawn too.
        for frame in [
            "",
            "[frame]\nblend = \"linear\"\n",
            "[frame]\nprimaries = \"bt709\"\n",
            "[entity]\nfile = \"e.wgsl\"\n",
        ] {
            assert_eq!(not_drawn(&manifest(frame)), None, "{frame}");
        }
        let wide = "[frame]\nblend = \"linear\"\nprimaries = \"bt2020\"\n";
        let why = not_drawn(&manifest(wide)).unwrap();
        assert!(
            why.contains("primaries = \"bt2020\" isn't served yet"),
            "{why}"
        );
        let why = not_drawn(&manifest("[motion]\n")).unwrap();
        assert!(why.contains("[motion] needs a later Horizon"), "{why}");
        let kept =
            "[[resource]]\nname = \"cache\"\nbuffer = 64\nelement = \"u32\"\nkeep = \"view\"\n";
        assert_eq!(not_drawn(&manifest(kept)), None);
        let traced = format!("{kept}[ray_tracing]\n");
        assert_eq!(not_drawn(&manifest(&traced)), None);
    }
}
