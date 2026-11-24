//! Exercise the actual registered leave-block goal and Game event handler.
use super::{Audio, Game, Options, Progress, Settings, load_resources};
use horizon_client::{
    chunk::Chunk,
    mobs::{Mob, MobEvent, goals::Cx},
};
use horizon_core::random::LegacyRandom;
use horizon_core::{geometry::Vec3, position::ChunkPos};
use mod_api::RandomSource;
use std::sync::Arc;

/// The production tick lends the level's RNG and attachments to goals while
/// they read its blocks. Keep that ownership boundary in the registered-goal test.
fn tick_registered_goals(game: &mut Game<'_>, mob: &mut Mob) -> Vec<MobEvent> {
    let mut goals = std::mem::take(&mut mob.ai.goals);
    let mut snapshot = vec![super::creatures::seen(mob, None, game.items())];
    snapshot.extend(
        game.mobs
            .iter()
            .map(|other| super::creatures::seen(other, None, game.items())),
    );
    let mut events = Vec::new();
    let mut lent = horizon_client::health::LentLevel::take(&mut game.world);
    {
        let world = game.mob_world_base(&snapshot);
        let mut level = horizon_client::health::DamageLevel::lent(
            &game.world,
            game.pack.assets.registry,
            &mut lent,
            Some(game.items()),
            Some(&game.pack.block_registries),
            &game.damage_hooks,
        );
        goals.tick(
            mob,
            &mut Cx {
                w: &world,
                level: &mut level,
                events: &mut events,
            },
        );
    }
    lent.give_back(&mut game.world);
    mob.ai.goals = goals;
    events
}

#[test]
#[ignore = "daily"]
fn leave_block_goal_uses_real_neighbor_shapes_and_keeps_air_placement() {
    let root = std::path::PathBuf::from(std::env::var_os("HORIZON_TEST_RESOURCES").unwrap());
    let settings = Settings::default();
    let loaded = load_resources(
        horizon_assets::resource_source::ResourceRoot::directory(root.clone()),
        Vec::new(),
        None,
        &root,
        &settings,
        std::time::Instant::now(),
        false,
        super::execution_role::ExecutionRole::IntegratedServer,
        &Progress::new(),
        &mut |_, _| unreachable!(),
    )
    .unwrap();
    let mut options = Options::parse(std::iter::empty()).unwrap().unwrap();
    options.resources = root.clone();
    options.execution_role = super::execution_role::ExecutionRole::IntegratedServer;
    let mut game = Game::new(
        &loaded.resources,
        loaded.pack,
        loaded.gui.unwrap(),
        options,
        settings,
        Audio::silent(&horizon_assets::resource_source::ResourceSource::directory(
            root.to_path_buf(),
        )),
        None,
    )
    .unwrap();
    game.world
        .chunks
        .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
    game.block_ticks
        .add_empty_chunk(ChunkPos::new(0, 0))
        .unwrap();
    let registry = game.pack.assets.registry;
    let air = registry.default_state("minecraft:air").unwrap();
    let stone = registry.default_state("minecraft:stone").unwrap();
    let kind = game
        .res
        .mob_types()
        .get("minecraft:enderman")
        .unwrap()
        .clone();
    for (name, expected_name) in [("poppy", "air"), ("note_block", "note_block")] {
        for x in 0..8 {
            for z in 0..8 {
                for y in 3..7 {
                    game.world
                        .set(x, y, z, if y == 3 { stone } else { air })
                        .unwrap();
                }
            }
        }
        game.mobs.clear();
        let carried = registry
            .default_state(&format!("minecraft:{name}"))
            .unwrap();
        let mut mob = Mob::new(325, kind.clone(), Vec3::new(3.5, 4.0, 3.5), 0.0);
        mob.random.set_seed(325);
        mob.extra.carried = Some(carried);
        game.mobs.push(mob);
        for _ in 0..48_000 {
            let mut mob = game.mobs.pop().unwrap();
            let events = tick_registered_goals(&mut game, &mut mob);
            game.mobs.push(mob);
            game.handle_mob_events(events).unwrap();
            if game.mobs[0].extra.carried.is_none() {
                break;
            }
        }
        assert_eq!(
            game.mobs[0].extra.carried, None,
            "eligible {name} must empty its hand"
        );
        let mut states = Vec::new();
        for x in 0..8 {
            for z in 0..8 {
                for y in 4..7 {
                    let state = game.world.block(x, y, z);
                    if state != air {
                        states.push(registry.state(state).unwrap());
                    }
                }
            }
        }
        if expected_name == "air" {
            assert!(
                states.is_empty(),
                "poppy normalizes to AIR before placement, not a dropped unsupported flower"
            );
        } else {
            assert_eq!(states.len(), 1);
            assert_eq!(states[0].name, "minecraft:note_block");
            assert_eq!(
                states[0].properties.get("instrument").map(String::as_str),
                Some("basedrum")
            );
        }
    }
    // A waterlogged candidate schedules the real level's fluid tick even
    // when a preceding world write makes the placement ineligible.
    let fence = registry.default_state("minecraft:oak_fence").unwrap();
    let mut descriptor = registry.state(fence).unwrap().clone();
    descriptor
        .properties
        .insert("waterlogged".into(), "true".into());
    let fence = registry.resolve(&descriptor).unwrap();
    let mut mob = Mob::new(326, kind, Vec3::new(3.5, 4.0, 3.5), 0.0);
    mob.random.set_seed(325);
    mob.extra.carried = Some(fence);
    game.mobs.clear();
    game.mobs.push(mob);
    let mut attempt = None;
    for _ in 0..48_000 {
        let mut mob = game.mobs.pop().unwrap();
        let events = tick_registered_goals(&mut game, &mut mob);
        game.mobs.push(mob);
        if let Some(event) = events
            .into_iter()
            .find(|e| matches!(e, MobEvent::EndermanPlaceBlock { .. }))
        {
            attempt = Some(event);
            break;
        }
    }
    let attempt = attempt.expect("the registered leave goal emits an attempt");
    let MobEvent::EndermanPlaceBlock { pos, mob, .. } = &attempt else {
        unreachable!()
    };
    assert_eq!(*mob, 326);
    game.world.set(pos[0], pos[1], pos[2], stone).unwrap();
    let before = game.block_ticks.fluids.count();
    // A stale carried-state identity cannot mutate the world or clear a new hand.
    game.mobs[0].extra.carried = Some(stone);
    game.handle_mob_events(vec![attempt.clone()]).unwrap();
    assert_eq!(game.block_ticks.fluids.count(), before);
    assert_eq!(game.mobs[0].extra.carried, Some(stone));
    game.mobs[0].extra.carried = Some(fence);
    game.handle_mob_events(vec![attempt.clone()]).unwrap();
    assert!(game.block_ticks.fluids.count() > before);
    assert_eq!(game.world.block(pos[0], pos[1], pos[2]), stone);
    assert_eq!(game.mobs[0].extra.carried, Some(fence));
}

/// Normalization must not bypass the existing entity_place cancel contract:
/// even a carried flower that becomes AIR stays in the enderman's hand.
#[test]
#[ignore = "daily"]
fn canceled_normalized_placements_keep_the_original_carried_block() {
    use horizon_core::position::BlockPos;
    use horizon_world::registry::BlockStateId;
    use mod_api::{EntityPlaceEvent, Flow, Hooks, ModEntry, ServerHost};
    use mod_platform::testing::with_mods;
    use std::sync::Mutex;

    struct Cancel {
        heard: Arc<Mutex<Vec<BlockPos>>>,
        normalized: Arc<Mutex<Vec<BlockStateId>>>,
    }
    #[mod_api::__subscriptions]
    impl Hooks for Cancel {
        fn entity_place(&self, _: &mut dyn ServerHost, event: &mut EntityPlaceEvent) -> Flow {
            self.heard.lock().unwrap().push(event.pos);
            self.normalized.lock().unwrap().push(
                crate::game::api_values::TryIntoEngine::try_into_engine(&event.state, &()).unwrap(),
            );
            Flow::Cancel
        }
    }

    let heard = Arc::new(Mutex::new(Vec::new()));
    let normalized = Arc::new(Mutex::new(Vec::new()));
    let mods = vec![ModEntry::new(
        "enderman_place_probe",
        Box::new(Cancel {
            heard: heard.clone(),
            normalized: normalized.clone(),
        }),
    )];
    with_mods(mods.into(), || {
        super::native_entity_refs::tests::with_game(|game| {
            game.world
                .chunks
                .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
            game.block_ticks
                .add_empty_chunk(ChunkPos::new(0, 0))
                .unwrap();
            let registry = game.pack.assets.registry;
            let air = registry.default_state("minecraft:air").unwrap();
            let stone = registry.default_state("minecraft:stone").unwrap();
            game.world.set(3, 3, 3, stone).unwrap();
            let kind = game
                .res
                .mob_types()
                .get("minecraft:enderman")
                .unwrap()
                .clone();
            for (i, name) in ["minecraft:poppy", "minecraft:note_block"]
                .into_iter()
                .enumerate()
            {
                let carried = registry.default_state(name).unwrap();
                let mut mob = Mob::new(325, kind.clone(), Vec3::new(3.5, 4.0, 3.5), 0.0);
                mob.extra.carried = Some(carried);
                game.mobs.clear();
                game.mobs.push(mob);
                game.handle_mob_events(vec![MobEvent::EndermanPlaceBlock {
                    mob: 325,
                    pos: [3, 4, 3],
                    carried,
                    occupants: Vec::new(),
                }])
                .unwrap();
                assert_eq!(game.world.block(3, 4, 3), air);
                assert_eq!(game.mobs[0].extra.carried, Some(carried));
                assert_eq!(heard.lock().unwrap().len(), i + 1, "one veto per attempt");
                let state = normalized.lock().unwrap()[i];
                if name == "minecraft:poppy" {
                    assert_eq!(state, air, "the veto sees the AIR-normalized flower");
                } else {
                    let state = registry.state(state).unwrap();
                    assert_eq!(state.name, "minecraft:note_block");
                    assert_eq!(
                        state.properties.get("instrument").map(String::as_str),
                        Some("basedrum"),
                        "the veto sees the normalized note block"
                    );
                }
            }
        });
    });
    assert_eq!(*heard.lock().unwrap(), [BlockPos::new(3, 4, 3); 2]);
}

#[test]
#[ignore = "daily"]
fn a_placement_listener_discarding_the_enderman_refuses_the_attempt() {
    use mod_api::{EntityPlaceEvent, Flow, Hooks, ModEntry, ServerHost};
    use mod_platform::testing::with_mods;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Discard(Arc<AtomicUsize>);
    #[mod_api::__subscriptions]
    impl Hooks for Discard {
        fn entity_place(&self, host: &mut dyn ServerHost, event: &mut EntityPlaceEvent) -> Flow {
            assert!(
                event
                    .placer
                    .expect("the live enderman resolves")
                    .discard(host)
            );
            self.0.fetch_add(1, Ordering::SeqCst);
            Flow::Continue
        }
    }

    let heard = Arc::new(AtomicUsize::new(0));
    let mods = vec![ModEntry::new(
        "enderman_discard_probe",
        Box::new(Discard(heard.clone())),
    )];
    with_mods(mods.into(), || {
        super::native_entity_refs::tests::with_game(|game| {
            game.world
                .chunks
                .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
            game.block_ticks
                .add_empty_chunk(ChunkPos::new(0, 0))
                .unwrap();
            let registry = game.pack.assets.registry;
            let air = registry.default_state("minecraft:air").unwrap();
            let stone = registry.default_state("minecraft:stone").unwrap();
            game.world.set(3, 3, 3, stone).unwrap();
            let kind = game
                .res
                .mob_types()
                .get("minecraft:enderman")
                .unwrap()
                .clone();
            let mut mob = Mob::new(325, kind, Vec3::new(3.5, 4.0, 3.5), 0.0);
            mob.extra.carried = Some(stone);
            game.mobs.push(mob);
            let attempt = MobEvent::EndermanPlaceBlock {
                mob: 325,
                pos: [3, 4, 3],
                carried: stone,
                occupants: Vec::new(),
            };
            game.handle_mob_events(vec![attempt.clone()]).unwrap();
            assert_eq!(heard.load(Ordering::SeqCst), 1);
            let mob = game.mobs.iter().find(|m| m.id == 325).unwrap();
            // Discard does not set health to zero: alive() alone is not enough.
            assert!(mob.alive());
            assert!(mob.removed);
            assert_eq!(mob.extra.carried, Some(stone));
            assert_eq!(game.world.block(3, 4, 3), air);
            // An already removed enderman cannot start another placement either.
            game.handle_mob_events(vec![attempt]).unwrap();
            assert_eq!(heard.load(Ordering::SeqCst), 1);
            assert_eq!(game.world.block(3, 4, 3), air);
        });
    });
}

#[test]
#[ignore = "daily"]
fn an_air_normalized_placement_empties_the_hand_without_a_block_change_notice() {
    use mod_api::{EntityChangeBlockEvent, Hooks, ModEntry, ServerHost};
    use mod_platform::testing::with_mods;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Changed(Arc<AtomicUsize>);
    #[mod_api::__subscriptions]
    impl Hooks for Changed {
        fn entity_change_block(
            &self,
            _: &mut dyn ServerHost,
            event: &mut EntityChangeBlockEvent<'_>,
        ) {
            assert_eq!(event.from, "minecraft:air");
            assert_eq!(event.to, "minecraft:note_block");
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let heard = Arc::new(AtomicUsize::new(0));
    let mods = vec![ModEntry::new(
        "enderman_change_probe",
        Box::new(Changed(heard.clone())),
    )];
    with_mods(mods.into(), || {
        super::native_entity_refs::tests::with_game(|game| {
            game.world
                .chunks
                .insert((0, 0), Arc::new(Chunk::empty(0, 0)));
            game.block_ticks
                .add_empty_chunk(ChunkPos::new(0, 0))
                .unwrap();
            let registry = game.pack.assets.registry;
            let air = registry.default_state("minecraft:air").unwrap();
            let stone = registry.default_state("minecraft:stone").unwrap();
            game.world.set(3, 3, 3, stone).unwrap();
            let kind = game
                .res
                .mob_types()
                .get("minecraft:enderman")
                .unwrap()
                .clone();
            for name in ["minecraft:poppy", "minecraft:note_block"] {
                let carried = registry.default_state(name).unwrap();
                let mut mob = Mob::new(325, kind.clone(), Vec3::new(3.5, 4.0, 3.5), 0.0);
                mob.extra.carried = Some(carried);
                game.mobs.clear();
                game.mobs.push(mob);
                game.handle_mob_events(vec![MobEvent::EndermanPlaceBlock {
                    mob: 325,
                    pos: [3, 4, 3],
                    carried,
                    occupants: Vec::new(),
                }])
                .unwrap();
                assert_eq!(game.mobs[0].extra.carried, None);
                let state = game.world.block(3, 4, 3);
                if name == "minecraft:poppy" {
                    assert_eq!(state, air);
                    assert_eq!(heard.load(Ordering::SeqCst), 0);
                } else {
                    assert_eq!(registry.state(state).unwrap().name, "minecraft:note_block");
                    assert_eq!(heard.load(Ordering::SeqCst), 1);
                }
            }
        });
    });
}

fn sampled_attempt(game: &mut Game<'_>, overlap_self: bool) -> MobEvent {
    for _ in 0..240_000 {
        let mut mob = game.mobs.remove(0);
        let own = mob.bounds();
        let events = tick_registered_goals(game, &mut mob);
        game.mobs.insert(0, mob);
        for event in events {
            if let MobEvent::EndermanPlaceBlock { pos, .. } = &event {
                let cube = horizon_core::geometry::Aabb::new(
                    Vec3::new(pos[0] as f64, pos[1] as f64, pos[2] as f64),
                    Vec3::new(
                        pos[0] as f64 + 1.0,
                        pos[1] as f64 + 1.0,
                        pos[2] as f64 + 1.0,
                    ),
                );
                if pos[1] == 4 && cube.intersects(own) == overlap_self {
                    return event;
                }
            }
        }
    }
    panic!("registered goal did not emit the bounded candidate");
}

#[test]
#[ignore = "daily"]
fn leave_block_raw_query_uses_non_mob_presence_and_snapshot_identity() {
    use horizon_client::{
        items::ItemStack,
        projectiles::{Projectile, ProjectileKind},
    };
    super::lan_actions::vehicle_loot_tests::with_fixture(|game| {
        game.boats.clear();
        game.ui.difficulty = 2;
        let registry = game.pack.assets.registry;
        let air = registry.default_state("minecraft:air").unwrap();
        let stone = registry.default_state("minecraft:stone").unwrap();
        game.block_ticks
            .add_empty_chunk(ChunkPos::new(0, 0))
            .unwrap();
        let kind = game.res.mob_types()["minecraft:enderman"].clone();
        let pig_type = game.res.mob_types()["minecraft:pig"].clone();
        let mut failures = Vec::new();
        for (label, blocked, self_hit, pig_kind) in [
            ("empty", false, false, 0),
            ("self", false, true, 0),
            ("already_removed_pig", false, true, 1),
            ("dying_pig", true, true, 2),
            ("later_moved_pig", true, true, 3),
            ("later_removed_pig", true, true, 3),
            ("item", true, false, 0),
            ("arrow", true, false, 0),
            ("marker", true, false, 0),
            ("player", true, false, 0),
            ("waiting_player", true, false, 0),
            ("dead_player", true, false, 0),
            ("expired_player", false, false, 0),
            ("spectator", false, false, 0),
            ("cactus_bad_support", true, false, 0),
        ] {
            for x in 0..8 {
                for z in 0..8 {
                    for y in 3..8 {
                        game.world
                            .set(x, y, z, if y == 3 { stone } else { air })
                            .unwrap();
                    }
                }
            }
            game.mobs.clear();
            game.dropped.clear();
            game.creatures.projectiles.clear();
            game.others.clear();
            for index in 0..game.lan.as_ref().unwrap().hub.guests.len() {
                game.lan.as_mut().unwrap().hub.guests.update(index, |g| {
                    g.position = [20.5, 65.0, 20.5];
                    g.motion = None;
                    g.dead = false;
                    g.death_time = 0;
                    g.health = 20.0;
                    g.placed = true;
                    g.loaded = true;
                    g.load_timeout = 0;
                    g.game_mode = 0;
                });
            }
            let carried = if label == "cactus_bad_support" {
                registry.default_state("minecraft:cactus").unwrap()
            } else {
                stone
            };
            let mut actor = Mob::new(325, kind.clone(), Vec3::new(3.5, 4.0, 3.5), 0.0);
            *actor.random = LegacyRandom::new(325);
            actor.extra.carried = Some(carried);
            game.mobs.push(actor);
            if pig_kind > 0 {
                let mut pig = Mob::new(326, pig_type.clone(), Vec3::new(3.5, 4.0, 3.5), 0.0);
                pig.removed = pig_kind == 1;
                if pig_kind == 2 {
                    pig.health = 0.0;
                }
                game.mobs.push(pig);
            }
            let event = sampled_attempt(game, self_hit);
            let MobEvent::EndermanPlaceBlock { pos, .. } = &event else {
                unreachable!()
            };
            let pos = *pos;
            let at = Vec3::new(
                pos[0] as f64 + 0.5,
                pos[1] as f64 + 0.2,
                pos[2] as f64 + 0.5,
            );
            match label {
                "item" => game.spawn_item(
                    ItemStack::new("minecraft:stone", 1),
                    [at.x, at.y, at.z],
                    [0.0; 3],
                    0,
                ),
                "arrow" => {
                    let mut arrow = Projectile::new(
                        ProjectileKind::Arrow {
                            base_damage: 2.0,
                            critical: false,
                            effect: None,
                        },
                        at,
                        None,
                    );
                    arrow.id = 900;
                    game.creatures.projectiles.push(arrow);
                }
                "marker" => game.others.push(super::commands::OtherEntity {
                    attachment_lifetime: Default::default(),
                    join: Default::default(),
                    id: 900,
                    kind: "minecraft:marker".into(),
                    position: at,
                    yaw: 0.0,
                    pitch: 0.0,
                    item_transformation: None,
                    interaction: None,
                    knot_check: None,
                    eye: None,
                }),
                "player" | "waiting_player" | "dead_player" | "expired_player" | "spectator" => {
                    game.lan.as_mut().unwrap().hub.guests.update(0, |g| {
                        g.position = [at.x, pos[1] as f64, at.z];
                        g.loaded = label != "waiting_player";
                        g.load_timeout = if g.loaded { 0 } else { 60 };
                        g.dead = label == "dead_player" || label == "expired_player";
                        g.health = if g.dead { 0.0 } else { 20.0 };
                        g.death_time = if label == "expired_player" { 20 } else { 0 };
                        if label == "spectator" {
                            g.game_mode = 3;
                        }
                    });
                }
                "later_moved_pig" => game.mobs[1].position = Vec3::new(50.0, 4.0, 50.0),
                "later_removed_pig" => game.mobs[1].removed = true,
                _ => {}
            }
            super::dispense_fixture::start_recording();
            game.handle_mob_events(vec![event]).unwrap();
            let trace = super::dispense_fixture::stop_recording();
            let kept = game.mobs[0].extra.carried.is_some();
            let changed = game.world.block(pos[0], pos[1], pos[2]) != air;
            if kept != blocked || changed == blocked {
                failures.push(format!(
                    "{label}: kept={kept}, changed={changed}, expected_blocked={blocked}"
                ));
            }
            let emits = trace
                .iter()
                .filter(|l| l.ends_with(" minecraft:block_place"))
                .count();
            if emits != usize::from(!blocked) {
                failures.push(format!("{label}: block-place events={emits}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    });
}
