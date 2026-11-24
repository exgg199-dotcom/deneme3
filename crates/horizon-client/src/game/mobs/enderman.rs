//! Enderman leave-block attempts need the authoritative block simulation.
use super::Game;
use horizon_client::{
    Result,
    mobs::{Species, Who},
};
use horizon_core::{
    geometry::{Aabb, Vec3},
    position::BlockPos,
};
use horizon_world::{
    level::{LevelAccess, flags},
    registry::BlockStateId,
};

/// An entity's box (`EntityDimensions.makeBoundingBox`): `width` wide and
/// `height` tall, standing on `feet`.
fn point_box(feet: [f64; 3], width: f64, height: f64) -> Aabb {
    Aabb::new(
        Vec3::new(feet[0] - width / 2.0, feet[1], feet[2] - width / 2.0),
        Vec3::new(
            feet[0] + width / 2.0,
            feet[1] + height,
            feet[2] + width / 2.0,
        ),
    )
}

impl Game<'_> {
    /// `Enderman.hurtServer` teleports inside the hurt (`teleport()`,
    /// `repeatedlyTryToTeleport()`), and so does `Shulker.hurtServer`
    /// (`teleportSomewhere()`, `hitByShulkerBullet()`). `Mob::damage` has no
    /// world, so it leaves them pending (an enderman's tries in `counter2`, a
    /// shulker's flags); a hurt the game layer deals runs them here, right
    /// after it, not on the mob's next AI step.
    pub(super) fn settle_hurt_teleports(&mut self) -> Result<()> {
        let pending: Vec<u32> = self
            .mobs
            .iter()
            .filter(|m| {
                (m.species() == Species::Enderman && m.extra.counter2 > 0)
                    || horizon_client::mobs::shulker::teleport_pending(m)
            })
            .map(|m| m.id)
            .collect();
        for id in pending {
            // What `LivingEntity.hurtServer` raised comes first, where the
            // mob was hurt and from its random's next draws: the hurt
            // sound, `InfestedMobEffect.onMobHurt`'s silverfish.
            let Some(i) = self.mobs.iter().position(|m| m.id == id) else {
                continue;
            };
            let hurt = std::mem::take(&mut self.mobs[i].outbox);
            if !hurt.is_empty() {
                self.handle_mob_events(hurt)?;
            }
            let Some(i) = self.mobs.iter().position(|m| m.id == id) else {
                continue;
            };
            let shulker = self.mobs[i].species() == Species::Shulker;
            // A shulker's teleport reads the mobs around it (`noCollision`,
            // the crowding count of `hitByShulkerBullet`), itself included as
            // in its own tick; an enderman's reads only blocks.
            let uuid = self.level.player_uuid;
            let others: Vec<_> = if shulker {
                self.mobs
                    .iter()
                    .map(|m| super::creatures::seen(m, uuid, self.items()))
                    .collect()
            } else {
                Vec::new()
            };
            // The world is read while the mob moves: take it out, then put it
            // back where it was (the mob order stays).
            let mut mob = self.mobs.swap_remove(i);
            let from = mob.position;
            let mut events = Vec::new();
            // `hitByShulkerBullet` draws its crowding check from the level's random.
            let mut level_random = self.world.random.clone();
            {
                let world = self.mob_world_base(&others);
                if shulker {
                    horizon_client::mobs::shulker::run_pending_teleports(
                        &mut mob,
                        &world,
                        &mut level_random,
                        &mut events,
                    );
                } else {
                    horizon_client::mobs::enderman::run_pending_teleports(
                        &mut mob,
                        &world,
                        &mut events,
                    );
                }
            }
            self.world.random = level_random;
            let to = mob.position;
            self.mobs.push(mob);
            let last = self.mobs.len() - 1;
            self.mobs.swap(i, last);
            // The hurt's teleport (`teleportTo`) is one position update, told at once.
            self.entity_section_moved(id, Some([from.x, from.y, from.z]), [to.x, to.y, to.z]);
            self.handle_mob_events(events)?;
        }
        Ok(())
    }
    /// `EndermanLeaveBlockGoal.tick`: normalize, check, place, post
    /// BLOCK_PLACE with the enderman and the placed state, then clear.
    pub(super) fn enderman_place_block(
        &mut self,
        mob: u32,
        pos: [i32; 3],
        carried: BlockStateId,
        occupants: &[Aabb],
    ) -> Result<()> {
        // Runtime ids identify the actual mob in this level. An obsolete attempt
        // must not clear another mob's hand or a subsequently changed stack.
        if !self.mobs.iter().any(|m| {
            m.id == mob
                && m.alive()
                && !m.removed
                && m.species() == Species::Enderman
                && m.extra.carried == Some(carried)
        }) {
            return Ok(());
        }
        let at = BlockPos::new(pos[0], pos[1], pos[2]);
        let area = Aabb::new(
            Vec3::new(f64::from(pos[0]), f64::from(pos[1]), f64::from(pos[2])),
            Vec3::new(
                f64::from(pos[0] + 1),
                f64::from(pos[1] + 1),
                f64::from(pos[2] + 1),
            ),
        );
        struct Candidate {
            state: BlockStateId,
            target: BlockStateId,
            below: BlockStateId,
        }
        let candidate = self.with_level(|b, level, ticks| {
            let target = level.block_state(at);
            let below = level.block_state(at.offset(0, -1, 0));
            // Failed candidates still run normalization and its scheduled ticks.
            let state = b.update_from_neighbour_shapes(level, ticks, at, carried);
            let support = level.facts.get(below);
            if !level.facts.is_air(target)
                || level.facts.is_air(below)
                || &*support.name == "minecraft:bedrock"
                || !support.full_collision
                || !b.can_survive(level, ticks, at, state)
            {
                return None;
            }
            Some(Candidate {
                state,
                target,
                below,
            })
        });
        // Neighbor normalization can enqueue entity changes. Non-mobs are
        // queried after those side effects; mob boxes remain the goal's snapshot.
        self.process_level_events()?;
        let Some(candidate) = candidate else {
            return Ok(());
        };
        if occupants.iter().any(|bounds| bounds.intersects(area)) || self.enderman_non_mob_in(area)
        {
            return Ok(());
        }
        // Keep the fork's entity_place veto, with the normalized state. A cancel
        // leaves the original carried block intact, even when it normalized to AIR.
        if self.place_canceled(mob, at, candidate.state, candidate.below, None) {
            return self.process_level_events();
        }
        // A listener can change the candidate's blocks or the mob. Refuse that
        // stale attempt instead of overwriting its writes or normalizing twice.
        let kind = self
            .mobs
            .iter()
            .find(|m| {
                m.id == mob
                    && m.alive()
                    && !m.removed
                    && m.species() == Species::Enderman
                    && m.extra.carried == Some(carried)
            })
            .map(|m| m.kind.id.clone());
        let Some(kind) = kind else {
            return self.process_level_events();
        };
        if self.world.block(pos[0], pos[1], pos[2]) != candidate.target
            || self.world.block(pos[0], pos[1] - 1, pos[2]) != candidate.below
        {
            return self.process_level_events();
        }
        let facts = self.res.facts();
        let from = facts.get(candidate.target).name.clone();
        let to = facts.get(candidate.state).name.clone();
        // AIR is a legitimate normalized state. setBlock may report no
        // change for it; the eligible operation still empties the hand.
        let changed = self.with_level(|b, level, ticks| {
            b.set_block(level, ticks, at, candidate.state, flags::UPDATE_ALL)
        });
        self.post_block_game_event(
            "minecraft:block_place",
            pos,
            Some(horizon_client::mobs::Who::Mob(mob)),
            Some(candidate.state),
        );
        if let Some(m) = self.mobs.iter_mut().find(|m| {
            m.id == mob
                && !m.removed
                && m.species() == Species::Enderman
                && m.extra.carried == Some(carried)
        }) {
            m.extra.carried = None;
        }
        self.process_level_events()?;
        if changed {
            self.native_entity_change_block(mob, &kind, pos, &from, &to);
        }
        Ok(())
    }

    /// Raw EntityGetter candidates, not building blockers. Mobs use the goal's
    /// per-mob snapshot; these other lists are queried after prior queued events.
    fn enderman_non_mob_in(&self, area: Aabb) -> bool {
        use super::commands::Target;
        use super::entity::{Entity as _, entity_lists_of, for_each_entity};
        let mut occupied = false;
        for_each_entity!(entity_lists_of!(self), entity => {
            if !matches!(entity.selector_target(),Some(Target::Mob(_)))
                && !entity.is_removed() && entity.bounds().intersects(area) {
                occupied=true;
            }
        });
        if occupied {
            return true;
        }
        if self
            .player
            .as_ref()
            .filter(|_| !self.spectator())
            .is_some_and(|p| p.bounds().intersects(area))
        {
            return true;
        }
        if self.lan_acting.is_some() {
            if self
                .lan_building_boxes
                .boxes(self.dim)
                .into_iter()
                .any(|(lo, hi)| {
                    Aabb::new(
                        Vec3::new(lo[0], lo[1], lo[2]),
                        Vec3::new(hi[0], hi[1], hi[2]),
                    )
                    .intersects(area)
                })
            {
                return true;
            }
        } else if self.lan.as_ref().is_some_and(|host| {
            host.hub.guests.iter().any(|g| {
                host.hub.here(g)
                    && super::building_players::guest_body_present(g)
                    && !g.spectator()
                    && g.bounds().intersects(area)
            })
        }) {
            return true;
        }
        let box_hits = |(lo, hi): ([f64; 3], [f64; 3])| {
            Aabb::new(
                Vec3::new(lo[0], lo[1], lo[2]),
                Vec3::new(hi[0], hi[1], hi[2]),
            )
            .intersects(area)
        };
        if self.item_frames.iter().any(|e| box_hits(e.bounds()))
            || self.paintings.iter().any(|e| box_hits(e.bounds()))
            || self.crystals.iter().any(|e| e.bounds().intersects(area))
        {
            return true;
        }
        // OtherEntity has no authoritative dimensions except the simulated
        // marker's zero-size box; retain its existing selector point fallback.
        if self
            .others
            .iter()
            .any(|e| Aabb::new(e.position, e.position).intersects(area))
        {
            return true;
        }
        let sized =
            |at: [f64; 3], width: f64, height: f64| point_box(at, width, height).intersects(area);
        if self
            .fangs
            .list
            .iter()
            .any(|e| sized(e.pos, 0.5, f64::from(0.8f32)))
            || self
                .ominous_spawners
                .iter()
                .any(|e| sized(e.position, 0.25, 0.25))
            || self
                .potions
                .thrown_entities()
                .iter()
                .any(|(_, p, _)| sized([p.x, p.y, p.z], 0.25, 0.25))
            || self
                .breath_clouds
                .iter()
                .any(|e| sized(e.at, f64::from(e.radius * 2.0), 0.5))
            || self
                .potions
                .clouds
                .iter()
                .any(|e| sized(e.at, f64::from(e.radius * 2.0), 0.5))
            || self
                .hook
                .as_ref()
                .is_some_and(|e| e.bounds().intersects(area))
        {
            return true;
        }
        if self.lan.as_ref().is_some_and(|host| {
            host.hub
                .guests
                .iter()
                .filter(|g| host.hub.here(g))
                .any(|g| {
                    host.play
                        .get(&g.conn)
                        .and_then(|p| p.hook.as_ref())
                        .is_some_and(|e| e.bounds().intersects(area))
                })
        }) {
            return true;
        }
        self.dragon.as_ref().is_some_and(|d| {
            sized([d.position.x, d.position.y, d.position.z], 16.0, 8.0)
                || (0..8).any(|part| d.part_box(part).intersects(area))
        })
    }
}
#[cfg(test)]
#[path = "enderman/enderman_occupancy_tests.rs"]
mod occupancy_tests;
