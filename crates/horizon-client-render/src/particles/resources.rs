//! Resource/JSON half of particle resource reloads. Preparing this value on
//! the pack worker lets the frame owner install it without reading pack files.

use super::{ModBehaviour, ParticleType};
use horizon_assets::resource_source::ResourceSource;

#[derive(Clone, Default)]
pub struct PreparedParticles {
    pub(super) vanilla: Vec<(ParticleType, Vec<String>)>,
    pub(super) mods: Vec<PreparedModParticle>,
}
#[derive(Clone)]
pub(super) struct PreparedModParticle {
    pub(super) key: String,
    pub(super) textures: Vec<String>,
    pub(super) behaviour: Option<ModBehaviour>,
}

impl PreparedParticles {
    /// Parse the resource generation without touching a live particle engine.
    pub fn load(root: &ResourceSource) -> Self {
        let mut prepared = Self::default();
        for namespace in root.namespaces("assets") {
            if namespace != "minecraft" {
                prepared.load_mod_sprites(&namespace, root);
            }
        }
        for &ty in ParticleType::ALL {
            let Ok(bytes) = root.read(&format!("assets/minecraft/particles/{}.json", ty.name()))
            else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let textures = texture_names(&json);
            if !textures.is_empty() {
                prepared.vanilla.push((ty, textures));
            }
        }
        prepared
    }

    /// A resource path includes its folders: `particles/trails/spark.json`
    /// belongs to `<ns>:trails/spark`, not `<ns>:spark`.
    fn load_mod_sprites(&mut self, ns: &str, root: &ResourceSource) {
        let prefix = format!("assets/{ns}/particles/");
        for path in root.list(&prefix) {
            let Some(stem) = path
                .strip_prefix(&prefix)
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let Some(key) =
                horizon_client_core::identifier::parse_identifier(&format!("{ns}:{stem}"))
            else {
                continue;
            };
            if ParticleType::from_name(&key).is_some() {
                continue;
            }
            let Ok(bytes) = root.read(&path) else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let behaviour = json.get("horizon:behaviour").and_then(|value| {
                let behaviour = ModBehaviour::parse(value);
                if behaviour.is_none() {
                    log::warn!("Invalid particle behaviour in {path}");
                }
                behaviour
            });
            let textures = texture_names(&json);
            if !textures.is_empty() {
                self.mods.push(PreparedModParticle {
                    key,
                    textures,
                    behaviour,
                });
            }
        }
    }
}

fn texture_names(json: &serde_json::Value) -> Vec<String> {
    json.get("textures")
        .and_then(serde_json::Value::as_array)
        .map(|textures| {
            textures
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(texture_name)
                .collect()
        })
        .unwrap_or_default()
}

/// ParticleDescription's identifiers remain namespaced when `particle/` is
/// inserted into their path. The atlas omits only the `minecraft` namespace.
fn texture_name(raw: &str) -> Option<String> {
    let id = horizon_client_core::identifier::parse_identifier(raw)?;
    let (namespace, path) = id.split_once(':')?;
    Some(if namespace == "minecraft" {
        format!("particle/{path}")
    } else {
        format!("{namespace}:particle/{path}")
    })
}
