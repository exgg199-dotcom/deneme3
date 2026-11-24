//! Exercise the Mob damage pipeline with a scoped native knockback hook.
use horizon_client_lib::{
    health::{Attack, DamageHooks, Hurting, Knock, TestLevel},
    items::ItemStack,
    join_seq::JoinList,
    mobs::{Damage, Mob, MobEvent, MobTypes, Species, apply_brain_events},
};
use std::{cell::RefCell, path::PathBuf, sync::Arc};

#[derive(Clone, Debug, PartialEq)]
struct Observation {
    attacker: i32,
    held: [Option<ItemStack>; 2],
    species: Option<Species>,
}
thread_local! {
    static OBSERVED: RefCell<Vec<Observation>> = const { RefCell::new(Vec::new()) };
}

fn native_knockback(
    level: horizon_client_lib::health::DamageLevel<'_>,
    pushed: Hurting<'_>,
    _: &mut Knock,
) -> bool {
    if let Hurting::Mob(_, source) = pushed {
        OBSERVED.with_borrow_mut(|seen| {
            seen.push(Observation {
                attacker: level.attack().attacker,
                held: level.attack().held.clone(),
                species: source.attacker_species,
            });
        });
    }
    false
}

fn fixture(held: Option<ItemStack>) -> JoinList<Mob> {
    OBSERVED.with_borrow_mut(Vec::clear);
    let root = PathBuf::from(
        std::env::var_os("HORIZON_TEST_RESOURCES")
            .expect("set HORIZON_TEST_RESOURCES to prepared resources"),
    );
    let attributes = horizon_world::json_file::read(&root.join("attributes.json")).unwrap();
    let attributes = horizon_world::attributes::entity::Catalog::from_json(&attributes).unwrap();
    let types = MobTypes::load_for_server(&root, &attributes).unwrap();
    let mut fish = Mob::new(
        1,
        Arc::new(types.types["minecraft:pufferfish"].clone()),
        Default::default(),
        0.0,
    );
    fish.extra.set_main_hand_stack(held);
    let mut pig = Mob::new(
        2,
        Arc::new(types.types["minecraft:pig"].clone()),
        Default::default(),
        0.0,
    );
    pig.position.x = 0.5;
    [fish, pig].into_iter().collect()
}

fn sting(mobs: &mut JoinList<Mob>, outer_stack: ItemStack) -> Vec<MobEvent> {
    let hooks = DamageHooks {
        knockback: Some(native_knockback),
        ..DamageHooks::NONE
    };
    let outer = Attack {
        attacker: 99,
        held: [Some(outer_stack), None],
        ..Attack::default()
    };
    let mut world = TestLevel::with_hooks(0, hooks);
    let mut level = world.level().attacked_by(&outer);
    let mut events = vec![MobEvent::PufferfishSting {
        mob: 1,
        target: 2,
        amount: 3.0,
        source: Damage::mob(1, [0.0; 3]),
        poison_ticks: 120,
    }];
    apply_brain_events(mobs, &mut level, &mut events, 0);
    assert_eq!(level.attack().attacker, outer.attacker);
    assert_eq!(level.attack().held, outer.held);
    events
}

#[test]
#[ignore = "daily"]
fn sting_native_hook_sees_fish_full_stack_and_restores_outer_attacker() {
    let mut stick = ItemStack::new("minecraft:stick", 7);
    stick.damage = 9;
    stick.data_mut().repair_cost = 17;
    stick.data_mut().enchantments = vec![(Arc::from("minecraft:unbreaking"), 3)];
    let mut mobs = fixture(Some(stick.clone()));
    let outer_stack = ItemStack::new("minecraft:apple", 2);
    let health_before = mobs[1].health;
    let events = sting(&mut mobs, outer_stack);
    assert_eq!(mobs[1].health, health_before - 3.0);
    assert!(mobs[1].has_effect("minecraft:poison"));
    assert!(events.iter().any(|event| matches!(
        event,
        MobEvent::Sound("entity.puffer_fish.sting", _, 1.0, 1.0)
    )));
    assert_eq!(
        OBSERVED.with_borrow(Clone::clone),
        vec![Observation {
            attacker: 1,
            held: [Some(stick), None],
            species: Some(Species::Pufferfish),
        }]
    );
}

#[test]
#[ignore = "daily"]
fn an_empty_fish_hand_does_not_inherit_an_enclosing_players_item() {
    let mut mobs = fixture(None);
    sting(&mut mobs, ItemStack::new("minecraft:diamond_sword", 1));
    assert_eq!(
        OBSERVED.with_borrow(Clone::clone),
        vec![Observation {
            attacker: 1,
            held: [None, None],
            species: Some(Species::Pufferfish),
        }]
    );
}
