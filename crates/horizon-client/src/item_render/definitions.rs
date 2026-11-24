//! Nested client item identifiers resolve through the retained virtual resource source.
use horizon_assets::{item_model::ClientItem, resource_source::ResourceSource};
use std::collections::BTreeMap;

/// Every `items/**/*.json` client item definition, by identifier. Through
/// Resolved resource names preserve nested item identifiers.
pub(super) fn load(root: &ResourceSource) -> BTreeMap<String, ClientItem> {
    let mut out = BTreeMap::new();
    for namespace in root.namespaces("assets") {
        let prefix = format!("assets/{namespace}/items/");
        for name in root.list(&prefix) {
            let Some(id) = name
                .strip_prefix(&prefix)
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let Ok(text) = root.read_to_string(&name) else {
                continue;
            };
            match ClientItem::from_json(&text) {
                Ok(item) => {
                    out.insert(format!("{namespace}:{id}"), item);
                }
                Err(error) => {
                    log::warn!("Horizon: item definition {namespace}:{id} not loaded: {error}")
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_assets::item_model::{
        Context, DisplayContext, ItemModels, LayerContent, NoEnv, StackView,
    };
    use std::fs;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(std::env::temp_dir().join(format!("horizon-item-discovery-{unique}")))
        }
        fn write(&self, file: &str, text: &str) {
            let path = self.0.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prepared_item_definitions_keep_nested_ids_and_use_packed_bytes() {
        let fixture = Fixture::new();
        let packed = r#"{"model":{"type":"minecraft:model","model":"server:item/packed"}}"#;
        fixture.write("horizon-resources.json", "{}");
        fixture.write("assets/server/items/weapons/orb.json", packed);
        horizon_core::prepared_files::write(&fixture.0).unwrap();
        // A packed prepared root must read the prepared bytes, like the other
        // tables, rather than silently reverting to the loose-file loader.
        fixture.write("assets/server/items/weapons/orb.json", "{");
        let definitions = load(&ResourceSource::directory(fixture.0.clone()));
        let horizon_assets::item_model::ItemModelNode::Model(model) =
            &definitions["server:weapons/orb"].model
        else {
            panic!("expected packed model");
        };
        assert_eq!(model.model, "server:item/packed");
        assert!(!definitions.contains_key("server:orb"));
    }

    #[test]
    fn nested_server_definitions_resolve_by_full_path_and_reload() {
        let fixture = Fixture::new();
        let first = r#"{"model":{"type":"minecraft:model","model":"server:item/stones/orb"}}"#;
        let second = r#"{"model":{"type":"minecraft:model","model":"server:item/weapons/orb"}}"#;
        fixture.write("assets/server/items/item/stones/orb.json", first);
        fixture.write("assets/server/items/item/weapons/orb.json", second);
        fixture.write("assets/minecraft/items/paper.json", first);
        fixture.write("assets/other/items/item/stones/orb.json", second);
        fixture.write("assets/server/items/item/invalid.json", "{");
        fixture.write("assets/server/items/item/ignored.txt", first);
        let definitions = load(&ResourceSource::directory(fixture.0.clone()));
        assert_eq!(definitions.len(), 4);
        assert!(definitions.contains_key("minecraft:paper"));
        assert!(definitions.contains_key("other:item/stones/orb"));
        assert!(!definitions.contains_key("server:orb"));
        let models = ItemModels {
            definitions,
            ..Default::default()
        };
        let context = Context {
            display: DisplayContext::Gui,
            owner: None,
            level: None,
            seed: 0,
        };
        for (id, model) in [
            ("item/stones/orb", "server:item/stones/orb"),
            ("item/weapons/orb", "server:item/weapons/orb"),
        ] {
            let mut stack = StackView::new("minecraft:paper", 1);
            stack.patch.insert(
                "minecraft:item_model".into(),
                Some(serde_json::json!(format!("server:{id}"))),
            );
            let resolved = models.resolve(&stack, &context, &NoEnv);
            assert_eq!(resolved.layers.len(), 1);
            assert_eq!(
                resolved.layers[0].content,
                LayerContent::Model {
                    model: model.into()
                }
            );
        }
        fixture.write("assets/server/items/item/stones/orb.json", second);
        let reloaded = load(&ResourceSource::directory(fixture.0.clone()));
        let horizon_assets::item_model::ItemModelNode::Model(leaf) =
            &reloaded["server:item/stones/orb"].model
        else {
            panic!("expected model leaf")
        };
        assert_eq!(leaf.model, "server:item/weapons/orb");
    }
}
