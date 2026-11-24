//! The save screen freezes simulation, but keeps pumping window events while
//! snapshots are prepared in small batches and the region workers drain.
use super::{Dim, Game, loading_frame};
use horizon_client::{Result, hud::HudBatch};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

pub(super) struct ClosingSave {
    pub quit: bool,
    close: bool,
    reply: Option<std::sync::mpsc::Sender<String>>,
    started: Instant,
    queued_chunks: Vec<(i32, i32)>,
    retried_writes: bool,
    parked: Option<(Dim, VecDeque<Dim>)>,
    stage: Stage,
}
enum Stage {
    Generation,
    Chunks(VecDeque<(i32, i32)>),
    Entities(VecDeque<(i32, i32)>),
    Writes,
    Server,
    Quit,
}

// Bound the work in both sparse and block-entity-heavy worlds. A single chunk
// is indivisible; check the time budget after each snapshot, not each batch.
const SNAPSHOTS_PER_FRAME: usize = 16;
const SNAPSHOT_BUDGET: Duration = Duration::from_millis(2);

impl Game<'_> {
    /// Save the level's chunk tickets: `/forceload`'s in vanilla's `chunk_tickets.dat`, the
    /// mods' in Horizon's own `mod_chunk_tickets.dat`.
    pub(super) fn save_chunk_tickets(&self) -> Result<()> {
        self.save
            .save_chunk_tickets(self.streaming.commanded(), self.tickets.portal_tickets())?;
        self.save
            .save_mod_tickets(self.streaming.mod_tickets(), |slot| {
                mod_api::TicketController::from_slot(slot).map(|c| c.id())
            })
    }
    pub(super) fn begin_closing_save(&mut self, quit: bool) -> Result<()> {
        if let Some(save) = &mut self.pending_save {
            save.quit |= quit;
            return Ok(());
        }
        if self.title.is_some() || self.saved_for_exit {
            // A quit from the title still needs the event loop to finish.
            self.pending_save = Some(ClosingSave {
                quit,
                close: true,
                reply: None,
                started: Instant::now(),
                queued_chunks: Vec::new(),
                retried_writes: false,
                parked: None,
                stage: Stage::Quit,
            });
            return Ok(());
        }
        if self.integrated.is_none() && self.remote.is_none() {
            self.admit_level_save()?;
        }
        if self.integrated.is_some() {
            // Minecraft.disconnect: the client logs out before its server halts.
            self.client_logging_out();
        }
        // Stop client-side terrain work before the integrated server is shut down. The
        // server's own stop path aborts its workers, but the window's streaming registration
        // is dropped only after the event loop exits and would otherwise wait for queued work.
        self.streaming.abort_generation();
        if let Some(integrated) = &self.integrated {
            integrated.request_stop();
            self.pending_save = Some(ClosingSave {
                quit,
                close: true,
                reply: None,
                started: Instant::now(),
                queued_chunks: Vec::new(),
                retried_writes: false,
                parked: None,
                stage: Stage::Server,
            });
        } else if self.remote.is_some() {
            if !quit {
                return self.leave_server(None);
            }
            self.client_logging_out();
            if let Some(mut remote) = self.remote.take() {
                remote.session.disconnect();
            }
            self.saved_for_exit = true;
            self.pending_save = Some(ClosingSave {
                quit,
                close: true,
                reply: None,
                started: Instant::now(),
                queued_chunks: Vec::new(),
                retried_writes: false,
                parked: None,
                stage: Stage::Quit,
            });
        } else {
            self.close_container();
            self.award_custom_stat("leave_game", 1);
            self.lan_close(false);
            self.free_player_beds()?;
            self.pending_save = Some(ClosingSave {
                quit,
                close: true,
                reply: None,
                started: Instant::now(),
                queued_chunks: Vec::new(),
                retried_writes: false,
                parked: Some((self.dim, self.levels.parked_dims().into())),
                stage: Stage::Generation,
            });
        }
        self.capture(false);
        Ok(())
    }

    /// Control saves acknowledge only after durable completion, while their
    /// snapshot work is spread across event-loop turns too.
    pub(super) fn begin_control_save(
        &mut self,
        all: bool,
        reply: std::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        if self.pending_save.is_some() {
            return Err("a world save is already in progress".into());
        }
        self.admit_level_save()?;
        if all {
            self.mark_all_resident_chunks_for_save();
        }
        self.keys.clear();
        self.physical_keys.clear();
        self.left = false;
        self.right = false;
        self.pending_save = Some(ClosingSave {
            quit: false,
            close: false,
            reply: Some(reply),
            started: Instant::now(),
            queued_chunks: Vec::new(),
            retried_writes: false,
            parked: Some((self.dim, self.levels.parked_dims().into())),
            stage: Stage::Generation,
        });
        Ok(())
    }

    /// `true` means the window can exit. The session lock stays in Game until
    /// every snapshot has been queued and every writer has synced its files.
    pub(super) fn poll_closing_save(&mut self) -> Result<bool> {
        let Some(mut save) = self.pending_save.take() else {
            return Ok(false);
        };
        let result = (|| -> Result<bool> {
            if !matches!(&save.stage, Stage::Quit | Stage::Server) {
                self.admit_level_save()?;
            }
            match &mut save.stage {
                Stage::Quit => Ok(true),
                Stage::Server => {
                    let server = self
                        .integrated
                        .as_ref()
                        .ok_or("the integrated server stopped with an error")?;
                    if !server.stopped() {
                        return Ok(false);
                    }
                    // Drop/join only after completion; leave_server's teardown
                    // can then run without blocking on the server's writes.
                    if save.quit {
                        self.integrated.take().unwrap().stop()?;
                    } else {
                        self.leave_server(None)?;
                    }
                    Ok(true)
                }
                Stage::Generation => {
                    if save.started.elapsed() > Duration::from_secs(30) {
                        return Err("chunk generation did not finish before saving".into());
                    }
                    let deadline = Instant::now() + Duration::from_millis(2);
                    loop {
                        match self.streaming.prepare_poll_save(&mut self.world, deadline) {
                            horizon_client::streaming::SaveStep::Pending => break,
                            horizon_client::streaming::SaveStep::Full(chunk) => {
                                self.native_late_full_chunk_snapshot(&chunk);
                                self.streaming.save_finished(chunk);
                            }
                            horizon_client::streaming::SaveStep::Protos(protos) => {
                                self.native_proto_snapshots(&protos);
                                if self.streaming.submit_protos(protos) {
                                    save.stage = Stage::Chunks(
                                        self.world.modified.iter().copied().collect(),
                                    );
                                }
                                break;
                            }
                        }
                    }
                    Ok(false)
                }
                Stage::Chunks(keys) => {
                    let started = Instant::now();
                    for _ in 0..SNAPSHOTS_PER_FRAME {
                        let Some(&key) = keys.front() else { break };
                        let chunks = self.with_entities(self.world.chunks.get(&key));
                        self.native_full_chunk_snapshots(&chunks);
                        self.save.save_later(
                            &chunks,
                            self.pack.assets.registry,
                            world_biomes!(self),
                        )?;
                        save.queued_chunks.push(key);
                        keys.pop_front();
                        if started.elapsed() >= SNAPSHOT_BUDGET {
                            break;
                        }
                    }
                    if keys.is_empty() {
                        self.spawn_loaded_entities();
                        save.stage = Stage::Entities(self.world.chunks.keys().copied().collect());
                    }
                    Ok(false)
                }
                Stage::Entities(keys) => {
                    let started = Instant::now();
                    for _ in 0..SNAPSHOTS_PER_FRAME {
                        let Some(&key) = keys.front() else { break };
                        let chunks = self.entity_chunks(&HashSet::from([key]), false);
                        self.save.save_entities_later(chunks)?;
                        keys.pop_front();
                        if started.elapsed() >= SNAPSHOT_BUDGET {
                            break;
                        }
                    }
                    if keys.is_empty() {
                        self.prune_entity_memory();
                        self.save_border()?;
                        self.save_raids()?;
                        self.save.save_attachments(&self.world.attachments)?;
                        self.save_chunk_tickets()?;
                        if save
                            .parked
                            .as_ref()
                            .is_none_or(|(origin, _)| self.dim == *origin)
                        {
                            self.save_game_metadata()?;
                        }
                        save.stage = Stage::Writes;
                    }
                    Ok(false)
                }
                Stage::Writes => {
                    let players = self.finish_player_autosaves(false)?;
                    match self.save.writes_finished() {
                        Err(_) if !save.retried_writes && self.save.retry_failed_writes()? => {
                            save.retried_writes = true;
                            return Ok(false);
                        }
                        result => {
                            if !result? || !players {
                                return Ok(false);
                            }
                        }
                    }
                    for key in save.queued_chunks.drain(..) {
                        self.world.modified.remove(&key);
                    }
                    if let Some((origin, remaining)) = &mut save.parked {
                        if let Some(next) = remaining.pop_front() {
                            self.enter_level(next)?;
                            if save.close {
                                self.streaming.abort_generation();
                            }
                            save.stage = Stage::Generation;
                            save.started = Instant::now();
                            save.retried_writes = false;
                            return Ok(false);
                        }
                        if self.dim != *origin {
                            self.enter_level(*origin)?;
                        }
                    }
                    Ok(true)
                }
            }
        })();
        match result {
            Ok(true) => {
                if let Some(reply) = save.reply.take() {
                    let _ = reply.send(serde_json::json!({ "ok": true }).to_string());
                }
                if save.quit && !save.close {
                    // A window close during a control save still has to close
                    // containers and save the resulting inventory changes.
                    self.begin_closing_save(true)?;
                    return Ok(false);
                }
                if save.quit {
                    self.saved_for_exit = true;
                    Ok(true)
                } else {
                    if save.close && self.title.is_none() {
                        self.finish_perf();
                        self.finish_close_world()?;
                    }
                    self.clock.reset();
                    self.previous = Instant::now();
                    Ok(false)
                }
            }
            Err(mut error) => {
                if let Some((origin, _)) = &save.parked
                    && self.dim != *origin
                    && let Err(restore) = self.enter_level(*origin)
                {
                    error = format!("{error}; could not restore the original dimension: {restore}")
                        .into();
                    save.close = true;
                }
                if let Some(reply) = save.reply.take() {
                    let _ =
                        reply.send(serde_json::json!({ "error": error.to_string() }).to_string());
                }
                if !save.close && !save.quit {
                    log::warn!("Horizon: control save failed: {error}");
                    self.clock.reset();
                    self.previous = Instant::now();
                    return Ok(false);
                }
                // Keep ownership (and the lock) while shutdown reports the
                // durable-write failure.
                self.pending_save = Some(save);
                Err(error)
            }
            Ok(false) => {
                self.pending_save = Some(save);
                Ok(false)
            }
        }
    }

    pub(super) fn render_closing_save(&mut self) -> Result<()> {
        if matches!(
            self.pending_save.as_ref().map(|save| &save.stage),
            Some(Stage::Quit)
        ) {
            return Ok(());
        }
        // disconnectWithSavingScreen: a GenericMessageScreen, its title and
        // no navigation.
        horizon_client::widgets::begin_screen(|| {
            horizon_client::widgets::ScreenInfo::new(
                "saving",
                super::screen_info::tr("menu.savingLevel", &[]),
            )
            .without_navigation()
        });
        let Some(renderer) = &mut self.renderer else {
            return Ok(());
        };
        let mut hud = HudBatch::new(
            [renderer.config.width, renderer.config.height],
            self.settings.gui_scale_inputs(),
        );
        let progress = match self.pending_save.as_ref().map(|save| &save.stage) {
            Some(Stage::Chunks(_)) => 0.25,
            Some(Stage::Entities(_)) => 0.5,
            Some(Stage::Writes) => 0.9,
            _ => 0.0,
        };
        horizon_client::level_loading::draw_progress(
            &mut hud,
            &self.gui.atlas,
            "Saving world...",
            progress,
        );
        renderer.render(&loading_frame(&hud))?;
        Ok(())
    }

    /// `GameRenderer.tryTakeScreenshotIfNeeded`: while a local world without a
    /// saved icon is on the screen, grab the rendered frame once a second until
    /// the save has one. The Select World list shows it as the world's preview.
    pub(super) fn tick_world_icon(&mut self) {
        // Its grab would take a sequence's frame.
        if self.screenshot_sequence.is_some() {
            return;
        }
        let Some(icon) = self.world_icon.as_ref() else {
            return;
        };
        if icon
            .last_attempt
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        if icon.path.is_file() {
            // tryTakeScreenshotIfNeeded's hasWorldScreenshot: it has one, stop asking.
            self.world_icon = None;
            return;
        }
        if self
            .ui
            .screenshot_writes
            .iter()
            .any(|write| write.targets(&icon.path))
        {
            return;
        }
        // takeAutoScreenshot only saves with every loaded section rendered, or
        // the icon would picture the holes the streamer has not filled yet.
        if self.streaming.pending() != 0
            || !self.pending_uploads.is_empty()
            || self.remote_backlog() != 0
        {
            return;
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        // countRenderedSections() > 10: a client that joined its server
        // thread has nothing pending before the first chunks arrive, and the
        // icon would be only sky.
        if renderer.drawn_sections <= 10 {
            return;
        }
        if self.pending_screenshot.is_some() || renderer.screenshot_busy() {
            // Wait for the queued frame or readback to finish before trying again.
            return;
        }
        let path = icon.path.clone();
        if let Some(icon) = self.world_icon.as_mut() {
            icon.last_attempt = Some(Instant::now());
        }
        renderer.request_screenshot(horizon_client::gfx::ScreenshotRequest {
            path,
            size: Some([64, 64]),
            scaled: false,
            background: true,
        });
    }
}

impl Game<'_> {
    /// Save the level in place: its changed chunks, its entities and its raids.
    pub(super) fn save_level(&mut self, wait: bool) -> Result<()> {
        self.admit_level_save()?;
        self.save_border()?;
        self.save_chunk_tickets()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.streaming.prepare_save(&mut self.world, wait, deadline) {
                horizon_client::streaming::SaveStep::Full(chunk) => {
                    self.native_late_full_chunk_snapshot(&chunk);
                    self.streaming.save_finished(chunk);
                }
                horizon_client::streaming::SaveStep::Protos(protos) => {
                    self.native_proto_snapshots(&protos);
                    self.streaming.finish_save(protos, wait);
                    break;
                }
                horizon_client::streaming::SaveStep::Pending => unreachable!(),
            }
        }
        let keys: Vec<_> = self.world.modified.iter().copied().collect();
        self.save.save_attachments(&self.world.attachments)?;
        let chunks = self.with_entities(keys.iter().filter_map(|k| self.world.chunks.get(k)));
        self.native_full_chunk_snapshots(&chunks);
        self.save
            .save_later(&chunks, self.pack.assets.registry, world_biomes!(self))?;
        // Keep the dirty marks if the writer refuses these snapshots.
        for key in &keys {
            self.world.modified.remove(key);
        }
        // Prepare entities while the chunk writer runs, then use one barrier.
        let finished = self.save_entities(false).and_then(|()| {
            let raids = self.save_raids();
            // Retry retained snapshots once, including edits from unloaded chunks.
            if wait && let Err(error) = self.save.flush() {
                if !self.save.retry_failed_writes()? {
                    return Err(error);
                }
                self.save.flush()?;
            }
            raids
        });
        if let Err(error) = finished {
            // Restore the submitted keys without clearing any edits made
            // after the snapshots were taken.
            self.world.modified.extend(keys);
            return Err(error);
        }
        if !wait {
            self.world.modified.autosaved(&keys, Instant::now());
        }
        if mod_api::__private::listened(mod_api::HookId::LevelSave) {
            self.native_level_save(self.dim.id());
        }
        Ok(())
    }
}
