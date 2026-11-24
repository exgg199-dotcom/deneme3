//! The effective component view passed to the shared Java durability algorithm.
use horizon_items::items::{ItemInfo, ItemStack};
use horizon_world::item_durability::DurabilityStack;

pub(super) fn project(stack: &ItemStack, item: Option<&ItemInfo>) -> DurabilityStack {
    DurabilityStack {
        count: stack.count.min(i32::MAX as u32) as i32,
        air: &*stack.item == "minecraft:air",
        max_damage: stack
            .carries_component(item, "minecraft:max_damage")
            .then(|| stack.max_damage(item)),
        damage: stack
            .carries_component(item, "minecraft:damage")
            .then(|| stack.effective_damage(item)),
        unbreakable: stack.carries_component(item, "minecraft:unbreakable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_core::nbt::Tag;

    fn prototype() -> ItemInfo {
        ItemInfo {
            max_damage: 20,
            damage: 7,
            components: vec!["minecraft:max_damage".into(), "minecraft:damage".into()],
            ..ItemInfo::default()
        }
    }

    #[test]
    fn prototype_damage_and_explicit_zero_reach_the_mutation_algorithm() {
        let info = prototype();
        let mut stack = ItemStack::new("test:tool", 1);
        assert_eq!(project(&stack, Some(&info)).damage, Some(7));
        stack.set_damage_value(0);
        assert_eq!(project(&stack, Some(&info)).damage, Some(0));
        stack.damage = 80;
        assert_eq!(project(&stack, Some(&info)).damage, Some(20));
    }

    #[test]
    fn removed_components_and_unbreakable_keep_their_presence_semantics() {
        let mut info = prototype();
        let mut stack = ItemStack::new("test:tool", 1);
        stack
            .data_mut()
            .other
            .push(("!minecraft:damage".into(), Tag::Byte(1)));
        assert!(!project(&stack, Some(&info)).is_damageable());
        stack.data_mut().other.clear();
        stack
            .data_mut()
            .other
            .push(("!minecraft:max_damage".into(), Tag::Byte(1)));
        assert_eq!(project(&stack, Some(&info)).max_damage, None);
        stack.data_mut().other.clear();
        info.components.push("minecraft:unbreakable".into());
        assert!(!project(&stack, Some(&info)).is_damageable());
        stack
            .data_mut()
            .other
            .push(("!minecraft:unbreakable".into(), Tag::Byte(1)));
        assert!(project(&stack, Some(&info)).is_damageable());
    }

    #[test]
    fn writing_zero_replaces_a_removal_and_survives_saved_component_encoding() {
        let mut stack = ItemStack::new("test:tool", 1);
        stack
            .data_mut()
            .other
            .push(("!minecraft:damage".into(), Tag::Byte(1)));
        stack.set_damage_value(0);
        assert!(stack.carries_component(None, "minecraft:damage"));
        let saved = horizon_items::item_nbt::item_to_nbt(&stack);
        assert_eq!(
            saved.get("components").unwrap().get("minecraft:damage"),
            Some(&Tag::Int(0))
        );
        assert!(
            saved
                .get("components")
                .unwrap()
                .get("!minecraft:damage")
                .is_none()
        );
    }

    #[test]
    fn stack_maximum_overrides_the_prototype_and_clamps_damage() {
        let info = prototype();
        let mut stack = ItemStack::new("test:tool", 1);
        stack
            .data_mut()
            .other
            .push(("minecraft:max_damage".into(), Tag::Int(5)));
        let state = project(&stack, Some(&info));
        assert_eq!(state.max_damage, Some(5));
        assert_eq!(state.damage, Some(5));
        stack.count = 0;
        assert_eq!(project(&stack, Some(&info)).damage(), 0);
    }
}
