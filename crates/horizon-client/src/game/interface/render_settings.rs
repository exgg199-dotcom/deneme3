//! The renderer and its settings: building it for the window, applying the video settings and a
//! shader pack's translucent layers, and the far terrain (LOD) distances.
use super::*;

#[derive(Default)]
pub(super) struct FrameAttempt<K = FrameKey>(Option<K>);

/// The requested frame and drawable size, the shader path, and its outcome.
type FrameKey = (
    Option<(horizon_client::gfx::FrameConfiguration, [u32; 2])>,
    PathBuf,
    Option<bool>,
);

impl<K: PartialEq> FrameAttempt<K> {
    // Remember attempts, including failures, until the requested configuration changes.
    fn changed(&mut self, key: K) -> bool {
        let changed = self.0.as_ref() != Some(&key);
        self.0 = Some(key);
        changed
    }
}

impl Game<'_> {
    fn renderer_for(
        &self,
        window: Arc<Window>,
        pack: shader_packs::PickedPack,
    ) -> Result<Renderer> {
        build_renderer(
            window,
            self.options.offscreen,
            &self.pack,
            &self.gui,
            self.renderer.as_ref(),
            None,
            self.settings.horizon.api,
            self.settings.horizon.translucent_batching,
            self.settings.vsync,
            &self.settings.horizon,
            pack,
            self.renderer
                .as_ref()
                .map(|r| r.display_reading())
                .unwrap_or_default(),
            (
                self.settings.file.int("textureFiltering"),
                self.settings.file.int("maxAnisotropyBit"),
            ),
        )
    }
    /// The frame follows what draws the picture (docs/design/hdr-linear-light.md, "When it
    /// changes"): when HDR or the pick (unless refused) call for another frame than the
    /// renderer's, it is made again before this frame draws.
    pub(super) fn follow_frame(&mut self) {
        if self.options.paced_by_split || self.options.split_guest {
            return;
        }
        let picked = shader_packs::picked_pack(&self.settings);
        let path = self.native_shaders.resolve_path(
            &Self::shader_packs_dir(),
            picked,
            shader_packs::dev_pack().is_some(),
        );
        let pack = if shader_packs::pack_owned(picked, &path, &self.settings.mods) {
            self.native_shaders.frame_pack()
        } else {
            None
        };
        let outcome = if pack.is_some() {
            native_shaders::outcome(&path)
        } else {
            Some(false)
        };
        let pack = pack.filter(|_| outcome != Some(false)).unwrap_or_default();
        // Display capabilities can still change between shader checks.
        let request = pack.frame();
        let wanted = self.renderer.as_ref().map(|r| {
            (
                r.requested_frame(&self.settings.horizon, request),
                [r.config.width, r.config.height],
            )
        });
        if !self.frame_attempted.changed((wanted, path, outcome)) {
            return;
        }
        if self
            .renderer
            .as_ref()
            .is_none_or(|r| r.frame_follows(&self.settings.horizon, request))
        {
            return;
        }
        if let Err(e) = self.rebuild_renderer(pack) {
            log::warn!("Horizon: the renderer for the new frame could not be made: {e}");
        }
    }

    /// The renderer made again on the same device from the resources loaded, keeping the
    /// pipelines that come out the same and the world the old one holds (`Renderer::take_world`):
    /// only what draws in the frame is made again, and no section is meshed again.
    fn rebuild_renderer(&mut self, pack: shader_packs::PickedPack) -> Result<()> {
        let Some(window) = self.window.clone() else {
            return Ok(());
        };
        let started = std::time::Instant::now();
        if let Some(renderer) = &mut self.renderer {
            renderer.release_surface();
        }
        let mut renderer = match self.renderer_for(window, pack) {
            Ok(renderer) => renderer,
            Err(error) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.recreate_surface()?;
                }
                return Err(error);
            }
        };
        let mut world_kept = false;
        if let Some(old) = &mut self.renderer {
            renderer.resize(old.config.width, old.config.height);
            world_kept = renderer.take_world(old);
            renderer.capture_result = old.capture_result.take();
            if world_kept {
                renderer.capture = old.capture.take();
            }
        }
        self.adopt(renderer, world_kept);
        log::info!(
            "Horizon: the renderer was made again for the frame the settings call for in {} ms",
            started.elapsed().as_millis()
        );
        Ok(())
    }

    pub(super) fn apply_settings(&mut self) {
        self.bind_mod_mappings();
        // LocalPlayer.refreshChatAbilities: the chat shows what the Chat option allows.
        let abilities = self.chat_abilities();
        self.ui.chat.set_abilities(&self.gui.atlas.font, abilities);
        let render_distance_changed = self.streaming.radius != self.settings.render_distance;
        self.streaming.radius = self.settings.render_distance;
        // Options.renderDistance, biomeBlendRadius, ambientOcclusion,
        // cutoutLeaves, Opaque Ice and the leaf-culling mode:
        // LevelExtractor.allChanged when they change.
        let leaf_culling = self.effective_leaf_culling();
        let (s, st) = (&self.settings, &self.streaming);
        let reload = render_distance_changed
            || (
                s.biome_blend,
                s.smooth_lighting,
                s.cutout_leaves,
                s.horizon.opaque_ice,
                leaf_culling,
            ) != (
                st.biome_blend,
                st.smooth_lighting,
                st.cutout_leaves,
                st.opaque_ice,
                st.leaf_culling,
            );
        self.streaming.biome_blend = self.settings.biome_blend;
        self.streaming.smooth_lighting = self.settings.smooth_lighting;
        self.streaming.cutout_leaves = self.settings.cutout_leaves;
        self.streaming.opaque_ice = self.settings.horizon.opaque_ice;
        self.streaming.leaf_culling = leaf_culling;
        if reload {
            self.all_sections_changed();
        }
        self.audio.master = self.settings.master_volume;
        if self.settings.sound_device != self.audio.device()
            && let Err(error) = self.audio.set_device(&self.settings.sound_device)
        {
            log::warn!("Horizon: could not apply the requested audio output ({error}); will retry");
        }
        let gui = self.settings.gui_scale_inputs();
        if self.applied_gui_scale != Some(gui) {
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.mark_gui_geometry_changed();
            }
            self.applied_gui_scale = Some(gui);
        }
        self.apply_ui_settings();
    }

    /// The sprite layers a shader pack draws in the translucent pass: every section is meshed
    /// again when they change.
    pub(super) fn set_translucent_layers(&mut self, layers: Vec<bool>) {
        if *layers != *self.streaming.translucent_layers {
            self.streaming.translucent_layers = layers.into();
            self.all_sections_changed();
        }
    }

    /// LOD Distance and Far Detail Reduced as the player chose them, the
    /// distance no farther than a server sampling far terrain for us allows.
    pub(super) fn far_terrain_choice(&self) -> (u32, bool) {
        let g = &self.settings.horizon;
        let reduced = g.far_detail == horizon_client::graphics::FarDetail::Reduced;
        let distance = match self.server_far_terrain() {
            Some(offer) => g.lod_distance.min(offer.distance),
            None => g.lod_distance,
        };
        (distance, reduced)
    }

    /// The far terrain another machine's server samples for us
    /// (`lod_net`): a client of a dedicated server or of a LAN world, not of
    /// its own server thread, which hands its generator over directly.
    pub(super) fn server_far_terrain(&self) -> Option<horizon_client::lod_net::Offer> {
        if self.integrated.is_some() {
            return None;
        }
        self.remote.as_ref()?.far_terrain
    }

    /// LOD Distance as far terrain uses it (`graphics::lod_distance_capped`).
    pub(super) fn lod_distance(&self) -> u32 {
        let (distance, _) = self.far_terrain_choice();
        match &self.renderer {
            Some(renderer) => {
                horizon_client::graphics::lod_distance_capped(distance, &renderer.info)
            }
            None => distance.min(horizon_client::lod::MAX_CHUNKS),
        }
    }

    /// Only enabled LOD worlds extend the renderer's range, never simulation.
    pub(super) fn terrain_distance(&self) -> i32 {
        if self.lod.is_some() {
            self.streaming.radius.max(self.lod_distance() as i32)
        } else {
            self.streaming.radius
        }
    }

    /// The renderer for the window, with the current block sprites, GUI
    /// atlas and world border texture.
    /// A new renderer's atlas holds only the pack's sprites: what was drawn into it at run time
    /// (skins, maps, the themed UI's glyphs and corners) has to be drawn again.
    pub(super) fn forget_atlas_slots(&mut self) {
        self.skin_slots.forget();
        self.maps.slots.forget();
        self.trim_slots.forget();
        self.map_patches.clear();
        self.text_patches.get_mut().clear();
        self.kept_block_entities.get_mut().clear();
        horizon_client::ui_theme::reset_atlas();
    }

    pub(super) fn new_renderer(&self, window: Arc<Window>) -> Result<Renderer> {
        self.renderer_for(window, shader_packs::picked_pack_shape(&self.settings))
    }

    /// A renderer made again from the resources loaded takes over: what named the old one's
    /// atlas slots is forgotten, the world is meshed again, and the shader pack and the
    /// settings are applied again.
    pub(super) fn adopt_renderer(&mut self, renderer: Renderer) {
        self.adopt(renderer, false);
    }

    /// `adopt_renderer`; with `world_kept`, a renderer that took the old one's sections and far
    /// tiles (`Renderer::take_world`): they and the meshes on their way stay.
    fn adopt(&mut self, mut renderer: Renderer, world_kept: bool) {
        if !world_kept {
            // A new renderer starts with the vanilla column.
            renderer.clear_sections(self.world.layout());
        }
        // And has no wall for the world border.
        self.border_renderer.invalidate();
        if let Some(old) = self.renderer.as_mut() {
            renderer.restore_screenshot_state(old.take_screenshot_state());
        }
        self.renderer = Some(renderer);
        self.dynamic_resolution.restart_measurements();
        self.airplay = Default::default();
        // Reattach before this renderer can submit its first frame.
        if !self.options.split_guest {
            self.sync_airplay();
        }
        self.forget_atlas_slots();
        let shader_mesh_changed = self.clear_shader_mesh_state();
        if !world_kept {
            self.clear_lod();
            self.pending_uploads.clear();
            let context = self.worker_context();
            self.streaming.reload_resources(&mut self.world, context);
        } else if shader_mesh_changed {
            self.all_sections_changed();
        }
        self.native_shaders = native_shaders::NativeShaders::picking(&std::mem::take(
            &mut self.native_shaders.picked,
        ));
        self.apply_ui_settings();
    }

    /// `LevelExtractor.allChanged`: displayed sections and mesh work are
    /// discarded, then every loaded section is rebuilt.
    pub(super) fn all_sections_changed(&mut self) {
        self.clear_lod();
        self.pending_uploads.clear();
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.clear_sections(self.world.layout());
        }
        self.streaming.reload_sections(&mut self.world);
    }
}

#[cfg(test)]
mod tests {
    use super::FrameAttempt;

    #[test]
    fn failed_frame_attempt_waits_for_a_policy_setting_or_size_change() {
        let mut attempts = FrameAttempt::default();
        let mut rebuilds = 0;
        // The requested frame stands for itself here: the 8-bit one, then the float one.
        for key in [
            (("8-bit", [1440, 1080]), "p", None),
            (("8-bit", [1440, 1080]), "p", None),
            // HDR output offered later, with HDR On: the frame is looked at again.
            (("float", [1440, 1080]), "p", None),
            (("float", [1440, 1080]), "p", Some(true)),
            (("float", [1440, 1080]), "p", Some(true)),
            (("float", [1440, 1080]), "p", Some(false)),
            (("float", [1440, 1080]), "p", Some(false)),
            (("float", [1440, 1080]), "q", None),
            (("8-bit", [1440, 1080]), "q", None),
            // The smaller drawable can fit after allocation at the first size failed.
            (("8-bit", [640, 480]), "q", None),
            (("8-bit", [640, 480]), "q", None),
        ] {
            if attempts.changed(key) {
                // A replacement fails: no success is recorded, just the attempted key.
                rebuilds += 1;
            }
        }
        assert_eq!(rebuilds, 7);
    }
}
