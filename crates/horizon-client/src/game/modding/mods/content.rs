//! Canonical content loading from immutable package snapshots.
use super::{
    ModsState,
    catalog::{Catalog, PreparedContent, Selection},
};
use horizon_assets::root_lease::{RootLease, lease_for};
use horizon_mod_package::{Package, selection::SelectedPackage};
use std::{path::Path, sync::Arc};

pub(super) fn build(
    base: &Path,
    selected: &Selection,
    catalog: Arc<Catalog>,
    original: &ModsState,
    strict: bool,
) -> horizon_client::Result<Arc<PreparedContent>> {
    let mut sources = original.clone();
    // A generation must not retain a chain of all previous generations.
    sources.content = None;
    sources.active_condition_root = None;
    sources.load_condition_candidate = None;
    sources.refused = Arc::new(std::sync::Mutex::new(Vec::new()));
    sources.load_notices = Arc::new(std::sync::Mutex::new(Vec::new()));
    sources.pending = None;
    sources.live = None;
    sources.catalog = Some(catalog.clone());
    sources.applied = Some(selected.enabled.clone());
    let mut packages = Vec::new();
    for id in &selected.order {
        let Some(entry) = selected.entries.get(id) else {
            continue;
        };
        // Code retains its validated startup manifest below; only pure content
        // can adopt a newly read package's metadata during a live switch.
        if entry.manifest.wasm.is_some() || entry.manifest.native.is_some() {
            continue;
        }
        packages.push(SelectedPackage {
            path: entry.source.clone(),
            package: Package::read(&entry.source)?,
        });
    }
    for native in original.native.packages() {
        let entry = selected.entries.get(&native.manifest.id).ok_or_else(|| {
            format!(
                "validated native package {} has no retained source snapshot",
                native.manifest.id
            )
        })?;
        if entry.manifest != native.manifest {
            return Err(format!("native package {} requires restart", native.manifest.id).into());
        }
        packages.push(SelectedPackage {
            path: entry.source.clone(),
            package: Package::read(&entry.source)?,
        });
    }
    for code in original
        .selected
        .iter()
        .filter(|p| p.package.manifest.wasm.is_some())
    {
        let path = selected
            .entries
            .get(&code.package.manifest.id)
            .map_or_else(|| code.path.clone(), |entry| entry.source.clone());
        packages.push(SelectedPackage {
            path,
            package: Package {
                manifest: code.package.manifest.clone(),
                icon: code.package.icon.clone(),
                layouts: code.package.layouts.clone(),
            },
        });
    }
    sources.selected = packages.into();
    let (root, accepted) = super::content_root_report(base, &sources)?;
    if strict {
        let refused: Vec<_> = selected
            .entries
            .keys()
            .filter(|id| !accepted.contains(*id))
            .cloned()
            .collect();
        if !refused.is_empty() {
            return Err(format!(
                "content generation refused selected packages: {}",
                refused.join(", ")
            )
            .into());
        }
    }
    let mut selection = selected.clone();
    selection.entries.retain(|id, _| accepted.contains(id));
    selection
        .order
        .retain(|id| selection.entries.contains_key(id));
    selection
        .enabled
        .retain(|id| !catalog.entries.contains_key(id) || accepted.contains(id));
    let parent = RootLease::combine(
        lease_for(root.prepared())
            .into_iter()
            .chain(selected.entries.values().map(|entry| entry.lease.clone())),
    );
    let root = root.with_root_lease(parent);
    let data_changed = original.data_switched
        || original
            .content
            .as_ref()
            .is_some_and(|old| old.data_changed || old.selection != selection);
    sources.data_switched = data_changed;
    Ok(Arc::new(PreparedContent {
        root,
        catalog,
        selection,
        sources,
        data_changed,
    }))
}
