//! The world's save on a timer (`MinecraftServer.computeNextAutosaveInterval`,
//! `onTickRateChanged`), the players' autosaves, and the world's metadata (level.dat).
use super::*;

impl Game<'_> {
    pub(super) fn admit_entity_save() -> Result<()> {
        if !horizon_client::join_seq::visibility::passes_settled() {
            return Err("entity pass is lifted".into());
        }
        Ok(())
    }

    pub(super) fn save_game(&mut self) -> Result<()> {
        self.save_game_with(true)
    }

    /// `MinecraftServer.computeNextAutosaveInterval`: three hundred seconds
    /// of ticks at the rate the server runs at now - its measured rate while
    /// a `/tick sprint` runs - never fewer than a hundred.
    pub(super) fn next_autosave_interval(&self) -> i64 {
        let rate = if self.clock.sprinting() {
            1.0e9 / (self.clock.average_nanos() + 1) as f32
        } else {
            self.clock.rate()
        };
        autosave_interval(rate)
    }

    /// `MinecraftServer.onTickRateChanged`: mods hear of it, then a rate that
    /// asks for a shorter wait than the one already running shortens it.
    pub(super) fn on_tick_rate_changed(&mut self) {
        self.native_server_tick_rate_changed();
        let interval = self.next_autosave_interval();
        if interval < self.ticks_until_autosave {
            self.ticks_until_autosave = interval;
        }
    }

    pub(super) fn finish_player_autosaves(&mut self, wait: bool) -> Result<bool> {
        let saved = self.player_autosaves.completed(wait);
        self.notify_saved_players(saved);
        self.player_autosaves.writes_finished()
    }

    pub(super) fn notify_saved_players(
        &mut self,
        saved: Vec<horizon_client::lan::playerdata::SavedPlayer>,
    ) {
        if !saved.is_empty() {
            let directory = horizon_client::lan::playerdata::directory(&self.world_root);
            for player in saved {
                self.native_player_save_to_file(player.uuid, &player.name, &directory);
            }
        }
    }

    pub(super) fn save_game_metadata(&mut self) -> Result<()> {
        let maps = self.save_maps();
        let random = self.save_random_sequences();
        let fight = self.save_fight();
        let trader = self.save_wandering_trader();
        let portals = self.save_portal_arrivals();
        let local_records = if self.keep_owner_files {
            Ok(())
        } else {
            self.save_stats().and(self.save_advancements())
        };
        let scoreboard = self.save_scoreboard();
        let bossbars = self.save_bossbars();
        let schedule = self.save_schedule();
        self.save_warden_tracker();
        self.sync_level();
        // Level::write can synchronously replace the integrated owner file.
        // Finish the metadata save even if an earlier player's write failed.
        let previous_player_writes = self.finish_player_autosaves(true);
        let stored_owner = self.store_owner();
        if self.keep_owner_files {
            // Crash teardown: `sync_level` filled the player from whatever is in place, not
            // from the owner's last consistent state.
            self.level.write_keeping_player(&self.world_root)?;
        } else {
            write_level_for_role(
                &mut self.level,
                &self.world_root,
                self.options.execution_role,
            )?;
        }
        // PlayerEvent.SaveToFile for the owner, once level.dat is written.
        if let Ok(Some((uuid, name))) = &stored_owner {
            let directory = horizon_client::lan::playerdata::directory(&self.world_root);
            self.native_player_save_to_file(*uuid, name, &directory);
        }
        // PlayerList.saveAll: every placed player saves with the server, to
        // their own file (`PlayerDataStorage.save`); the owner is also in the
        // file level.dat names, which is the one it loads from
        // (`playerdata::load_owner`).
        if self.options.execution_role == execution_role::ExecutionRole::DedicatedServer
            && let Some(host) = self.lan.as_mut()
        {
            // The vehicle tree goes into a player's file when they leave
            // (`take_parent_vehicle`); here it stays in the chunk saves.
            host.hub.guests.update_all(|g| {
                if g.placed {
                    // Read-only fixture input, correlated with the real save receipt.
                    // Keep this identical on the untouched/fixed selector builds.
                    if std::env::var_os("HORIZON_DEV_ADVANCEMENT_SELECTOR_AUDIT").is_some() {
                        eprintln!(
                            "advancement selector model raw: {}",
                            serde_json::json!({"conn":g.conn,"name":g.name,
                                "uuid":horizon_protocol::crypto::uuid_string(g.uuid),
                                "game_time":self.world.game_time,
                                "progress":g.advancements.to_json(&self.adv.tree, horizon_world::DATA_VERSION)})
                        );
                    }
                    self.player_autosaves.enqueue(g, &self.world_root);
                }
            });
        }
        let mut guest_records = Ok(());
        if self.options.execution_role == execution_role::ExecutionRole::DedicatedServer
            && let Some(host) = self.lan.as_ref()
        {
            guest_records = self.save_connected_guest_statistics(host);
            for guest in host.hub.guests.iter() {
                if host
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.uuid == guest.uuid)
                    || !host.play.contains_key(&guest.conn)
                {
                    continue;
                }
                guest_records = guest_records.and(self.save_guest_advancements(host, guest));
            }
        }
        let clocks = self.save_world_state();
        maps.and(random)
            .and(fight)
            .and(trader)
            .and(portals)
            .and(scoreboard)
            .and(bossbars)
            .and(schedule)
            .and(local_records)
            .and(stored_owner.map(|_| ()))
            .and(guest_records)
            .and(clocks)
            .and(previous_player_writes.map(|_| ()))
    }

    /// The autosave doesn't `wait` for the chunks to be on disk: every
    /// generated chunk is saved, hundreds at a time, encoded and written on
    /// the region writer meanwhile.
    pub(super) fn save_game_with(&mut self, wait: bool) -> Result<()> {
        if self.remote.is_some() {
            return Ok(());
        }
        self.admit_level_save()?;
        // `DedicatedServer.saveAllChunks` tells the management protocol's clients when a save starts and when
        // it ends without failing, for every save: the command, the autosave, the pause.
        let dedicated = self.is_dedicated_server();
        if dedicated {
            dedicated::events::record_server("saving", serde_json::json!({}));
        }
        let active = self.save_level(wait);
        let parked = self.save_parked_levels(wait);
        active?;
        parked?;
        let metadata = self.save_game_metadata();
        let player_writes = self.finish_player_autosaves(wait);
        metadata?;
        player_writes?;
        if wait {
            self.message = Some(("Saved the game".into(), Instant::now()));
        }
        self.native_world_saved(wait);
        if dedicated {
            dedicated::events::record_server("saved", serde_json::json!({}));
        }
        Ok(())
    }
}

impl Game<'_> {
    pub(super) fn admit_level_save(&self) -> Result<()> {
        // A carrier transfer that broke after `prepared` stopped the
        // session (`carrier_stop_session`): its recovery replays the
        // affected regions from the disk state the transfer itself
        // wrote, so no later save — the autosave a later tick would
        // run, or any exit path's — may write over them.
        if self.world.carrier_fences.is_broken() {
            return Err(
                "a carrier transfer did not finish; the world is left for recovery at reopen"
                    .into(),
            );
        }
        Self::admit_entity_save()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "daily"]
    fn metadata_save_reports_a_failed_player_write_without_losing_level_data() {
        crate::game::native_entity_refs::tests::with_client_game(|game| {
            let root =
                std::env::temp_dir().join(format!("horizon-player-receipt-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            game.world_root = root.clone();
            let _guest_connection = crate::game::lan::world_spawn_sync_tests::host_for(game);
            let blocked_world = root.join("blocked-player-output");
            std::fs::write(&blocked_world, b"fixture").unwrap();
            let guest = game.lan.as_ref().unwrap().hub.guests.iter().next().unwrap();
            game.player_autosaves.enqueue(guest, &blocked_world);

            let error = game.save_game_metadata().unwrap_err();
            assert!(error.to_string().contains("blocked-player-output"));
            assert!(root.join("level.dat").is_file());

            // Fail the same player write after the control save has queued
            // dirty terrain. A later save must still see the chunk as dirty.
            let guest = game.lan.as_ref().unwrap().hub.guests.iter().next().unwrap();
            game.player_autosaves.enqueue(guest, &blocked_world);
            let key = (7, 8);
            let chunk = horizon_client::chunk::Chunk::empty(key.0, key.1);
            game.world.chunks.insert(key, std::sync::Arc::new(chunk));
            game.world.modified.insert(key);
            let (reply, result) = std::sync::mpsc::channel();
            game.begin_control_save(false, reply).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while game.pending_save.is_some() {
                assert!(std::time::Instant::now() < deadline);
                game.poll_closing_save().unwrap();
                std::thread::yield_now();
            }
            assert!(result.try_recv().unwrap().contains("blocked-player-output"));
            assert!(game.world.modified.contains(&key));

            // A synchronous save also has to retain dirty terrain when the
            // writer reports a failure after accepting its snapshots.
            let writer_root = root.join("failed-writer");
            std::fs::create_dir(&writer_root).unwrap();
            std::fs::write(writer_root.join("entities"), b"fixture").unwrap();
            game.save = std::sync::Arc::new(horizon_client::save::SaveDir::new(
                writer_root,
                horizon_client::save::LevelShape::OVERWORLD,
            ));
            let entity =
                horizon_core::nbt::Tag::compound([("version", horizon_core::nbt::Tag::Int(3))]);
            game.save
                .save_entities_later(vec![(key, vec![entity])])
                .unwrap();

            let error = game.save_level(true).unwrap_err();
            assert!(error.to_string().contains("entities"), "{error}");
            assert!(game.world.modified.contains(&key));
            std::fs::remove_dir_all(root).unwrap();
        });
    }
}
