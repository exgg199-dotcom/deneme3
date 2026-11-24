//! Opening and closing a local world from the client: the title screen's actions, entering a
//! world on the server thread, and saving and leaving it on exit.
use super::*;
use horizon_client::save::level_mods;

/// File access and pack preparation can fail without invalidating the client.
#[derive(Debug)]
struct WorldPreparationError(String);
impl std::fmt::Display for WorldPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl std::error::Error for WorldPreparationError {}

fn require_saved_world(world: &std::path::Path) -> Result<()> {
    let is_file = |file| {
        let path = world.join(file);
        match std::fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(WorldPreparationError(format!("{}: {error}", path.display()))),
        }
    };
    if !is_file("level.dat")? && !is_file("level.dat_old")? {
        return Err(WorldPreparationError(format!(
            "World save is missing a level.dat or level.dat_old file: {}",
            world.display()
        ))
        .into());
    }
    Ok(())
}

/// Pack preparation must finish before a server is started. The caller owns the
/// world's SessionLock throughout; an error leaves staged inputs available.
pub(super) fn prepare_world_packs(
    game_dir: &std::path::Path,
    world: &std::path::Path,
    resources: &std::path::Path,
    create: Option<&horizon_client::title::WorldOptions>,
) -> Result<()> {
    let io_error =
        |path: &std::path::Path, error: std::io::Error| format!("{}: {error}", path.display());
    if let Some(dir) = create.and_then(|o| o.pack_dir.as_deref()) {
        // Discovery is intentionally tolerant for browsers; creation must not
        // interpret a missing/unreadable temporary folder as an empty selection.
        drop(std::fs::read_dir(dir).map_err(|e| io_error(dir, e))?);
        let packs = create.map_or(&[][..], |o| &o.data_packs[..]);
        horizon_client::datapack::install(dir, world, packs)?;
        let _ = std::fs::remove_dir_all(dir);
    }
    // install writes this itself, including its disabled packs. Only write a
    // selection here when the Create screen did not supply a temporary folder.
    if let Some(o) = create.filter(|o| o.pack_dir.is_none()) {
        std::fs::create_dir_all(world).map_err(|e| io_error(world, e))?;
        let disabled: Vec<String> = horizon_client::datapack::feature_packs(resources)
            .into_iter()
            .map(|p| p.id)
            .filter(|id| !o.data_packs.contains(id))
            .collect();
        let json = serde_json::json!({"enabled": o.data_packs, "disabled": disabled});
        let file = world.join("datapacks.json");
        std::fs::write(&file, json.to_string()).map_err(|e| io_error(&file, e))?;
    }
    if create.is_none() {
        horizon_client::datapack::install_new_world_packs(game_dir, world)?;
    }
    Ok(())
}

impl Game<'_> {
    /// `onAbort`: the confirm screen's Back or Escape reopens the Data
    /// Packs screen with the selection it had.
    pub(super) fn reopen_data_packs(&mut self, event_loop: &dyn EventLoopCtl) -> Result<()> {
        let action = self
            .title
            .as_mut()
            .and_then(|t| t.reopen_data_packs_action());
        if let Some(action) = action {
            self.title_action(event_loop, action)?;
        }
        Ok(())
    }

    /// What the title screen asked for, from a click or Enter.
    pub(super) fn title_action(
        &mut self,
        event_loop: &dyn EventLoopCtl,
        action: Action,
    ) -> Result<()> {
        match action {
            Action::Quit if self.options.split_guest => {}
            Action::Quit => {
                if self.splitscreen.guest {
                    self.leave_server(None)?;
                } else if horizon_ios_launcher::show() {
                    self.quit_to_launcher = true;
                } else {
                    event_loop.exit();
                }
            }
            Action::Options => self.open_options(true),
            Action::Accessibility => {
                self.open_options_at(true, horizon_client::options_ui::Page::Accessibility)
            }
            Action::Language => {
                self.open_options_at(true, horizon_client::options_ui::Page::Language)
            }
            Action::Sounds => self.open_options_at(true, horizon_client::options_ui::Page::Sound),
            Action::Credits => {
                self.open_options_at(true, horizon_client::options_ui::Page::Credits)
            }
            Action::Mods => self.open_options_at(true, horizon_client::options_ui::Page::Mods),
            Action::OpenUri(uri) => packs::open_uri(&uri),
            Action::CopyText(text) => {
                use horizon_client::clipboard::Clipboard;
                self.clipboard.set(&text);
            }
            Action::Play { folder } => {
                self.open_world_from_title(&folder, None, None, false, None)?
            }
            Action::Create {
                folder,
                name,
                seed,
                creative,
                options,
            } => {
                self.open_world_from_title(&folder, Some(&name), seed, creative, Some(&options))?
            }
            Action::Connect {
                address,
                lan,
                realm,
            } => self.connect_server_with_options(
                &address,
                &self.options.player_name(),
                None,
                lan,
                realm,
            )?,
            Action::DataPacks { dir, selected } => {
                self.keys.clear();
                self.set_screen(Screen::Options);
                self.options_from_title = true;
                let resources = self.vanilla_root();
                let additional = self
                    .discover_mod_packs(mod_api::PackType::ServerData)
                    .into_iter()
                    .filter_map(|pack| pack.selection)
                    .collect();
                self.ui
                    .options
                    .open_data_packs(dir, selected, resources, additional);
            }
            Action::None => {}
        }
        Ok(())
    }
    /// UI actions keep recoverable preparation errors in the menu. The raw
    /// open_world API and non-UI pending requests retain their error result.
    pub(super) fn open_world_from_title(
        &mut self,
        folder: &str,
        name: Option<&str>,
        seed: Option<i64>,
        creative: bool,
        create: Option<&horizon_client::title::WorldOptions>,
    ) -> Result<()> {
        match self.open_world(folder, name, seed, creative, create) {
            Ok(()) => {
                if let Some(PendingWorldEntry::Local {
                    folder: pending,
                    from_title,
                    ..
                }) = self.pending_entry.as_mut()
                    && pending == folder
                {
                    *from_title = true;
                }
                Ok(())
            }
            Err(error) if self.show_world_preparation_error(error.as_ref()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// A request acknowledged before loading can still hit a recoverable file
    /// error on resume. Keep the title available for the player to retry.
    pub(super) fn show_world_preparation_error(
        &mut self,
        error: &(dyn std::error::Error + 'static),
    ) -> bool {
        if !error.is::<WorldPreparationError>() {
            return false;
        }
        let Some(title) = self.title.as_mut() else {
            return false;
        };
        log::warn!("Horizon: could not prepare the world: {error}");
        title.failed(error);
        self.pending_entry = None;
        self.perf.cancel_world_opening();
        true
    }

    /// Enter a local world from the client: it runs on the client's server thread.
    pub(super) fn open_world(
        &mut self,
        folder: &str,
        name: Option<&str>,
        seed: Option<i64>,
        creative: bool,
        create: Option<&horizon_client::title::WorldOptions>,
    ) -> Result<()> {
        let world_folder = if create.is_some() {
            WorldFolder::New
        } else {
            WorldFolder::Existing
        };
        self.open_world_request(folder, name, seed, creative, create, world_folder)
    }

    pub(super) fn open_world_request(
        &mut self,
        folder: &str,
        name: Option<&str>,
        seed: Option<i64>,
        creative: bool,
        create: Option<&horizon_client::title::WorldOptions>,
        world_folder: WorldFolder,
    ) -> Result<()> {
        let game_dir = std::env::current_dir()?;
        let world = game_dir.join("saves").join(folder);
        if world_folder == WorldFolder::Existing {
            require_saved_world(&world)?;
        }
        // openWorldLoadLevelData: an existing world whose level data does not read opens
        // RecoverWorldDataScreen instead.
        if world_folder != WorldFolder::New
            && let Some(title) = self.title.as_mut()
            && title.open_recover(folder)
        {
            return Ok(());
        }
        world_loading::world_data_used();
        if self.defer_world_entry(PendingWorldEntry::Local {
            folder: folder.to_owned(),
            name: name.map(str::to_owned),
            seed,
            creative,
            create: create.cloned().map(Box::new),
            world_folder,
            from_title: false,
        })? {
            return Ok(());
        }
        // A world saved with mods that are not loaded now asks first (arismoko/horizon#3856), unless the player
        // already chose to open it anyway; the question reads the world and writes nothing.
        if world_folder != WorldFolder::New {
            if !level_mods::is_confirmed(&world) {
                let missing = self.ask_missing_mods(&world);
                if !missing.is_empty() {
                    if let Some(title) = self.title.as_mut() {
                        title.ask_missing_mods(folder, missing);
                        return Ok(());
                    }
                    for m in &missing {
                        log::warn!("Horizon: {}", level_mods::describe(m));
                    }
                }
            }
        }
        self.join_world_rest()?;
        // Own the save before Create World's first write, and retain that same
        // lock through the handoff to the server's WorldRequest.
        let allowlist = game_dir.join("saves/allowed_symlinks.txt");
        let session_lock = match world_folder {
            WorldFolder::Existing => {
                horizon_client::save::SessionLock::acquire_existing(&world, Some(&allowlist))
            }
            WorldFolder::New => horizon_client::save::SessionLock::acquire_new(&world, &allowlist),
            WorldFolder::OpenOrCreate => {
                horizon_client::save::SessionLock::acquire_validated(&world, &allowlist)
            }
        }
        .map_err(|error| WorldPreparationError(error.to_string()))?;
        if world_folder == WorldFolder::Existing {
            require_saved_world(&world)?;
        }
        prepare_world_packs(&game_dir, &world, &self.vanilla_root(), create)
            .map_err(|error| WorldPreparationError(error.to_string()))?;
        // IntegratedServer.initServer's line, which other launchers read in `latest.log` to
        // offer the world again.
        horizon_client::logging::write_as(
            "Server thread",
            log::Level::Info,
            &format!(
                "Starting integrated minecraft server version {}",
                horizon_core::VANILLA_VERSION
            ),
        );
        self.open_integrated(folder, name, seed, creative, create, session_lock)
    }

    /// Load authoritative world state synchronously, without client entry or
    /// integrated-server selection: a test-only convenience over the same two
    /// steps production startup calls separately (`prepare_world_and_rules`,
    /// then `finish_open_world`).
    #[cfg(test)]
    pub(super) fn load_world(&mut self, request: world_loading::WorldRequest) -> Result<()> {
        let prepared = self.prepare_world_and_rules(request)?;
        self.finish_open_world(prepared)
    }

    /// Leaving the game with a world open (window closed, Quit, `quit`):
    /// menu items go back (Player.closeContainer), then the world is saved,
    /// once.
    pub(super) fn finish_perf(&mut self) {
        // One session per computer: P1's (a splitscreen player's would count it again).
        if self.options.split_guest {
            return;
        }
        let s = &self.settings;
        let settings = horizon_telemetry::payload::Settings {
            render_distance: s.render_distance.clamp(0, 255) as u8,
            simulation_distance: s.simulation_distance.clamp(0, 255) as u8,
            // 260 is the slider's "unlimited".
            max_fps: if s.max_fps >= 260 {
                0
            } else {
                s.max_fps.min(u32::from(u16::MAX)) as u16
            },
            vsync: s.vsync,
            fullscreen: s.fullscreen,
            graphics: s.graphics_preset.clone(),
        };
        // Again now: on Wayland the window is on a screen only once it has been shown.
        if let Some(window) = &self.window {
            horizon_client::telemetry::note_display(window);
        }
        self.perf
            .finish(&horizon_client::logging::game_home(), settings);
    }

    pub(super) fn save_for_exit(&mut self) -> Result<()> {
        // Native application termination can bypass CloseRequested. Finish any
        // save screen synchronously here, before the process can go away.
        while self.pending_save.is_some() {
            self.poll_closing_save()?;
            if self.pending_save.is_some() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // A repro recording is finished and bundled before the process may go away.
        self.repro_on_exit();
        self.finish_perf();
        if self.title.is_some() || self.saved_for_exit {
            return Ok(());
        }
        // exitWorldAndClose disconnects the client from its world.
        self.client_logging_out();
        // Native application termination can bypass the close request, so make the same
        // client-side worker abort explicit before the game-owned streaming state is dropped.
        self.streaming.abort_generation();
        if let Some(server) = self.integrated.take() {
            // The server thread saves the world.
            server.stop()?;
            self.saved_for_exit = true;
            return Ok(());
        }
        if let Some(mut remote) = self.remote.take() {
            self.retire_recording(&remote.session);
            // Minecraft.disconnect closes the connection, so the host sees a clean leave.
            remote.session.disconnect();
            self.saved_for_exit = true;
            return Ok(());
        }
        self.close_container();
        if self.remote.is_none() {
            self.award_custom_stat("leave_game", 1);
        }
        self.lan_close(false);
        self.free_player_beds()?;
        self.save_game()?;
        self.saved_for_exit = true;
        Ok(())
    }

    pub(super) fn close_world(&mut self) -> Result<()> {
        self.destroying.clear();
        // Pause-menu and death-screen exits use the normal world-close action.
        // A standalone replay owns a synthetic remote session, so it must go
        // through replay cleanup instead of being treated like a server.
        if self.replay.playing() {
            self.close_studio_replay();
            return Ok(());
        }
        // A recording cannot cross into another world, and a replay feeding the
        // title screen is nonsense.
        self.repro_world_closed();
        self.clear_waypoints();
        self.post_effects.clear();
        self.spectated_post_effect = None;
        // Mod screens' living scenes go with the world their entities were copied from.
        self.live_scenes.get_mut().clear();
        if self.external_server() {
            return self.leave_server(None);
        }
        self.begin_closing_save(false)
    }
}

#[cfg(test)]
mod tests {
    use super::WorldFolder;

    #[test]
    fn reopening_requires_an_existing_save() {
        let fixture = Scratch::new();
        let world = fixture.0.join("world");
        assert!(super::require_saved_world(&world).is_err());
        assert!(!world.exists());
        std::fs::create_dir(&world).unwrap();
        assert!(super::require_saved_world(&world).is_err());
        std::fs::create_dir(world.join("level.dat")).unwrap();
        std::fs::create_dir(world.join("level.dat_old")).unwrap();
        assert!(super::require_saved_world(&world).is_err());
        std::fs::remove_dir(world.join("level.dat_old")).unwrap();
        std::fs::write(world.join("level.dat_old"), b"recovery fixture").unwrap();
        assert!(super::require_saved_world(&world).is_ok());
    }

    struct MenuLoop;
    impl super::super::event_loop::EventLoopCtl for MenuLoop {
        fn exit(&self) {
            panic!("a recoverable menu error must not exit the loop")
        }
        fn set_control_flow(&self, _: winit::event_loop::ControlFlow) {}
    }
    #[test]
    #[ignore = "daily"]
    fn title_world_preparation_failures_are_recoverable_and_keep_creation_inputs() {
        use horizon_client::title::{Mode, Title, WorldOptions};
        for deferred in [false, true] {
            let fixture = Scratch::new();
            let world = fixture.0.join("world");
            let staged = fixture.0.join("staged");
            if deferred {
                std::fs::create_dir(&world).unwrap();
                std::fs::write(world.join("datapacks"), b"preserve destination").unwrap();
            }
            std::fs::write(&staged, b"unreadable as a pack directory").unwrap();
            let options = WorldOptions {
                pack_dir: Some(staged.clone()),
                ..Default::default()
            };
            let mut game = super::super::native_machines::tests::server_game();
            let mut title = Title::new(
                fixture.0.join("saves"),
                &game.options.resources,
                &game.gui.splashes,
            );
            title.mode = Mode::Create {
                name: "Kept name".into(),
                seed: "4242".into(),
                creative: false,
                focus: 0,
                options: options.clone(),
            };
            game.title = Some(title);
            let folder = world.to_str().unwrap();
            let result = if deferred {
                game.pending_entry = Some(super::super::PendingWorldEntry::Local {
                    folder: folder.into(),
                    name: Some("Kept name".into()),
                    seed: Some(4242),
                    creative: false,
                    create: Some(Box::new(options.clone())),
                    world_folder: WorldFolder::New,
                    from_title: true,
                });
                game.resume_world_entry()
            } else {
                game.title_action(
                    &MenuLoop,
                    horizon_client::title::Action::Create {
                        folder: folder.into(),
                        name: "Kept name".into(),
                        seed: Some(4242),
                        creative: false,
                        options: options.clone(),
                    },
                )
            };
            assert!(
                result.is_ok(),
                "a recoverable title-screen access error must not terminate the game"
            );
            let title = game.title.as_mut().unwrap();
            assert!(
                matches!(&title.mode, Mode::Error {message, ..} if message.contains(if deferred {"already exists"} else {"staged"}))
            );
            title.escape();
            assert!(
                matches!(&title.mode, Mode::Create {name, seed, options: restored, ..}
                if name == "Kept name" && seed == "4242" && restored == &options)
            );
            assert!(game.pending_entry.is_none());
            assert!(game.integrated.is_none() && game.integrated_starting.is_none());
            assert!(!world.join("level.dat").exists());
            assert_eq!(
                std::fs::read(&staged).unwrap(),
                b"unreadable as a pack directory"
            );
            if deferred {
                assert_eq!(
                    std::fs::read(world.join("datapacks")).unwrap(),
                    b"preserve destination"
                );
            }
            drop(game);
            drop(
                horizon_client::save::SessionLock::acquire(&world)
                    .expect("failed entry releases its lock"),
            );
        }
    }

    #[test]
    #[ignore = "daily"]
    fn raw_world_requests_keep_error_results_even_when_a_title_exists() {
        for deferred in [false, true] {
            let fixture = Scratch::new();
            let world = fixture.0.join("world");
            let held = horizon_client::save::SessionLock::acquire(&world).unwrap();
            let options = horizon_client::title::WorldOptions::default();
            let mut game = super::super::native_machines::tests::server_game();
            game.title = Some(horizon_client::title::Title::new(
                fixture.0.join("saves"),
                &game.options.resources,
                &game.gui.splashes,
            ));
            let folder = world.to_str().unwrap();
            let result = if deferred {
                game.pending_entry = Some(super::super::PendingWorldEntry::Local {
                    folder: folder.into(),
                    name: Some("raw fixture".into()),
                    seed: Some(1),
                    creative: false,
                    create: Some(Box::new(options.clone())),
                    world_folder: WorldFolder::New,
                    from_title: false,
                });
                game.resume_world_entry()
            } else {
                game.open_world(folder, Some("raw fixture"), Some(1), false, Some(&options))
            };
            assert!(result.err().unwrap().to_string().contains("already exists"));
            assert!(matches!(
                game.title.as_ref().unwrap().mode,
                horizon_client::title::Mode::Main
            ));
            assert!(game.integrated.is_none() && game.integrated_starting.is_none());
            drop(game);
            drop(held);
        }
    }

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "horizon-pack-preparation-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn pack_preparation_propagates_missing_source_selection_and_default_copy_errors() {
        let fixture = Scratch::new();
        let game = &fixture.0;
        let world = game.join("world");
        let resources = game.join("resources");
        let options = horizon_client::title::WorldOptions {
            pack_dir: Some(game.join("missing-staged-packs")),
            ..Default::default()
        };
        assert!(super::prepare_world_packs(game, &world, &resources, Some(&options)).is_err());
        assert!(!world.join("datapacks.json").exists());

        std::fs::create_dir_all(world.join("datapacks.json")).unwrap();
        assert!(
            super::prepare_world_packs(
                game,
                &world,
                &resources,
                Some(&horizon_client::title::WorldOptions::default())
            )
            .is_err()
        );
        assert!(world.join("datapacks.json").is_dir());

        std::fs::create_dir(game.join(horizon_client::datapack::NEW_WORLD_PACKS)).unwrap();
        std::fs::write(
            game.join(horizon_client::datapack::NEW_WORLD_PACKS)
                .join("fixture.txt"),
            b"source",
        )
        .unwrap();
        std::fs::write(world.join("datapacks"), b"preserve destination").unwrap();
        assert!(super::prepare_world_packs(game, &world, &resources, None).is_err());
        assert_eq!(
            std::fs::read(world.join("datapacks")).unwrap(),
            b"preserve destination"
        );
    }

    #[test]
    fn successful_pack_preparation_keeps_selection_and_copies_both_enabled_and_disabled_packs() {
        let fixture = Scratch::new();
        let root = &fixture.0;
        let staged = root.join("staged");
        let world = root.join("world");
        let format = horizon_core::version::DATA_PACK_FORMAT;
        let metadata = serde_json::json!({"pack":{"min_format":[format.0,format.1],"max_format":[format.0,format.1],"description":"copy fixture"}}).to_string();
        for name in ["on", "off"] {
            std::fs::create_dir_all(staged.join(name)).unwrap();
            std::fs::write(staged.join(name).join("pack.mcmeta"), &metadata).unwrap();
        }
        let selected = vec!["vanilla".to_owned(), "file/on".to_owned()];
        let options = horizon_client::title::WorldOptions {
            pack_dir: Some(staged.clone()),
            data_packs: selected.clone(),
            ..Default::default()
        };
        super::prepare_world_packs(root, &world, &root.join("resources"), Some(&options)).unwrap();
        assert!(!staged.exists());
        for name in ["on", "off"] {
            assert!(
                world
                    .join("datapacks")
                    .join(name)
                    .join("pack.mcmeta")
                    .is_file()
            );
        }
        let selection: serde_json::Value =
            serde_json::from_slice(&std::fs::read(world.join("datapacks.json")).unwrap()).unwrap();
        assert_eq!(selection["enabled"], serde_json::json!(selected));
        assert_eq!(selection["disabled"], serde_json::json!(["file/off"]));
    }

    #[test]
    #[ignore = "daily"]
    fn a_pack_copy_failure_aborts_creation_and_keeps_the_staged_packs() {
        let root = std::env::temp_dir().join(format!(
            "horizon-pack-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let staged = root.with_extension("packs");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir_all(staged.join("fixture")).unwrap();
        let format = horizon_core::version::DATA_PACK_FORMAT;
        let metadata = serde_json::json!({"pack":{"min_format":[format.0,format.1],"max_format":[format.0,format.1],"description":"copy fixture"}}).to_string();
        std::fs::write(staged.join("fixture/pack.mcmeta"), &metadata).unwrap();
        assert_eq!(horizon_client::packs::folder(&staged, format).len(), 1);
        // A file at the destination makes the pack-copy operation fail on every OS.
        std::fs::write(root.join("datapacks"), b"preserve destination").unwrap();
        let options = horizon_client::title::WorldOptions {
            pack_dir: Some(staged.clone()),
            world_type: horizon_client::save::WorldPreset::Flat,
            structures: false,
            ..Default::default()
        };
        let lock = horizon_client::save::SessionLock::acquire(&root).unwrap();
        let result = super::prepare_world_packs(&root, &root, &root, Some(&options));
        assert!(
            result.is_err(),
            "failed pack installation must not start a world"
        );
        assert_eq!(
            std::fs::read_to_string(staged.join("fixture/pack.mcmeta")).unwrap(),
            metadata,
            "failed installation must preserve the staged input"
        );
        assert_eq!(
            std::fs::read(root.join("datapacks")).unwrap(),
            b"preserve destination"
        );
        assert!(!root.join("level.dat").exists());
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(staged).unwrap();
    }

    #[test]
    #[ignore = "daily"]
    fn create_world_cannot_change_pack_selection_before_taking_the_world_lock() {
        let root = std::env::temp_dir().join(format!(
            "horizon-create-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let original = b"{\"enabled\":[\"file/keep\"],\"disabled\":[]}";
        std::fs::write(root.join("datapacks.json"), original).unwrap();
        let held = horizon_client::save::SessionLock::acquire(&root).unwrap();
        let mut game = super::super::native_machines::tests::server_game();
        let result = game.open_world(
            root.to_str().unwrap(),
            Some("lock fixture"),
            Some(1),
            false,
            Some(&horizon_client::title::WorldOptions::default()),
        );
        assert_eq!(
            std::fs::read(root.join("datapacks.json")).unwrap(),
            original,
            "a world owned by another instance must be unchanged"
        );
        assert!(result.is_err(), "the existing world lock must refuse entry");
        drop(game);
        drop(held);
        std::fs::remove_dir_all(root).unwrap();
    }
}
