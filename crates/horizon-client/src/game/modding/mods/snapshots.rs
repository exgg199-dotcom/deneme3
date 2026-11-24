//! Worker-owned catalogue snapshots and live-switch metadata.
use super::*;
use catalog::{Catalog, Selection};

fn read_catalog(
    home: &Path,
    dev: &[PathBuf],
    native: &horizon_mod_package::runtime::Runtime,
    cancel: &horizon_client::mod_registry::Cancellation,
) -> Result<Arc<Catalog>, String> {
    let mut catalog = catalog::read(home, dev, cancel)?;
    let catalog_mut = Arc::make_mut(&mut catalog);
    catalog_mut.native_ids = native.built().iter().map(|m| m.id.to_owned()).collect();
    for validated in native.packages() {
        let package = horizon_mod_package::Package::read(&validated.path)?;
        if package.manifest != validated.manifest {
            return Err(format!(
                "native resource package {} changed since engine validation; restart required",
                validated.manifest.id
            ));
        }
        let declared = super::super::native_content_items::owned_item_ids(
            &validated.manifest,
            &super::native_declared().items,
        );
        let content = horizon_mod_package::Content::read_declaring(
            &validated.path,
            &validated.manifest,
            &declared,
        )?;
        let (source, revision, lease) =
            catalog::pin_source(home, &validated.path, &content, cancel)?;
        let pinned = horizon_mod_package::Package::read(&source)?;
        if pinned.manifest != validated.manifest {
            return Err("native package changed during snapshot preparation".into());
        }
        let id = validated.manifest.id.clone();
        if !catalog_mut.entries.contains_key(&id) {
            catalog_mut.order.push(id.clone());
        }
        catalog_mut.rows.retain(|row| row.id != id);
        catalog_mut.rows.push(Installed {
            id: id.clone(),
            title: format!("{} {}", pinned.manifest.name, pinned.manifest.version),
            description: pinned.manifest.description.clone(),
            problem: None,
            dev: false,
            native: None,
        });
        catalog_mut.info.insert(
            id.clone(),
            mod_list::ModInfo::of(&pinned.manifest, pinned.icon.as_ref()),
        );
        catalog_mut.entries.insert(
            id,
            Arc::new(catalog::Entry {
                revision,
                live: horizon_client::mod_content::live_switch(&content).is_ok(),
                source,
                manifest: pinned.manifest,
                dev: false,
                lease,
            }),
        );
    }
    let unbuilt = super::not_built(
        &home.join(mod_list::NATIVE_DIR),
        native.built(),
        &catalog_mut.rows,
    );
    catalog_mut.rows.extend(unbuilt);
    Ok(catalog)
}

pub(in crate::game) fn prepare_content(
    root: &Path,
    enabled: &std::collections::BTreeSet<String>,
    disabled: &std::collections::BTreeSet<String>,
    dev: &[PathBuf],
    sources: &ModsState,
) -> horizon_client::Result<Arc<PreparedContent>> {
    let home = Game::game_dir();
    horizon_client::mod_registry::with_content_snapshot(&home, || {
        let catalog = read_catalog(&home, dev, &sources.native, &Default::default())?;
        let selection = Selection::wanted(&catalog.wanted_ids(enabled, disabled), &catalog);
        content::build(root, &selection, catalog, sources, false).map_err(|error| error.to_string())
    })?
    .map_err(Into::into)
}

pub(in crate::game) fn rebuild_content(
    root: &Path,
    selection: &Selection,
    catalog: Arc<Catalog>,
    sources: &ModsState,
) -> horizon_client::Result<Arc<PreparedContent>> {
    content::build(root, selection, catalog, sources, true)
}

/// A loader fallback records what actually survived without rereading mutable packages.
pub(in crate::game) fn content_for_loaded_root(
    root: &Path,
    requested: Arc<PreparedContent>,
) -> horizon_client::Result<Arc<PreparedContent>> {
    if root == requested.root.prepared() {
        return Ok(requested);
    }
    let mut enabled = requested.selection.enabled.clone();
    enabled.retain(|id| {
        !requested.catalog.entries.contains_key(id) && !requested.catalog.invalid.contains(id)
    });
    let root = horizon_assets::resource_source::ResourceRoot::directory(root.to_owned())
        .with_root_lease(horizon_assets::root_lease::RootLease::acquire(root)?);
    let mut sources = requested.sources.clone();
    sources.content = None;
    sources.active_condition_root = None;
    Ok(Arc::new(PreparedContent {
        root,
        catalog: requested.catalog.clone(),
        selection: Selection {
            enabled,
            ..Default::default()
        },
        sources,
        data_changed: false,
    }))
}

impl Game<'_> {
    pub(in crate::game) fn publish_mod_catalog(&mut self) {
        let Some(catalog) = self.mods.catalog.clone() else {
            return;
        };
        self.settings.mods.installed = catalog.rows.clone();
        self.settings.mods.info = catalog.info.clone();
        for (id, info) in &mut self.settings.mods.info {
            info.warnings = mod_api::__private::registration_warnings(id)
                .into_iter()
                .map(|w| (w.key().to_owned(), w.args().to_vec()))
                .collect();
        }
        self.mods.content_ids = catalog.entries.keys().cloned().collect();
        self.mods.live_ids = catalog
            .entries
            .iter()
            .filter(|(_, entry)| entry.live)
            .map(|(id, _)| id.clone())
            .collect();
        self.list_native_mods();
        self.sync_native_switches();
        self.mods.report_refusals(&mut self.settings.mods.installed);
        self.settings
            .mods
            .apply_dependency_notices(&self.mods.load_notices.lock().unwrap());
    }
    pub(in crate::game) fn refresh_mod_catalog(&mut self) {
        if self.mod_catalog_job.is_some() {
            self.mods.refresh_again = true;
            return;
        }
        let home = Self::game_dir();
        let dev = self.mods.dev.clone();
        let native = self.mods.native.clone();
        match crate::game::mod_browser::OwnedJob::spawn("mod-catalogue", move |cancel| {
            let _access =
                horizon_client::import::acquire_home_access(&home).map_err(|e| e.to_string())?;
            horizon_client::mod_registry::with_content_snapshot_managed(&home, &cancel, || {
                read_catalog(&home, &dev, &native, &cancel)
            })
            .map_err(|e| e.to_string())?
        }) {
            Ok(job) => self.mod_catalog_job = Some(job),
            Err(error) => log::warn!("Horizon: mod catalogue refresh failed: {error}"),
        }
    }

    pub(in crate::game) fn poll_mod_catalog(&mut self) {
        let Some(job) = &self.mod_catalog_job else {
            return;
        };
        let result = match job.poll() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("mod catalogue worker stopped".into())
            }
        };
        self.mod_catalog_job = None;
        if self.mods.refresh_again {
            self.mods.refresh_again = false;
            self.refresh_mod_catalog();
            return;
        }
        match result {
            Ok(catalog) => {
                self.mods.catalog = Some(catalog);
                self.publish_mod_catalog();
                self.switch_mods();
            }
            Err(error) => log::warn!("Horizon: mod catalogue not refreshed: {error}"),
        }
    }

    pub(in crate::game) fn mods_live_now(&self) -> std::collections::BTreeSet<String> {
        if self.integrated.is_none() && self.remote.is_some() {
            return Default::default();
        }
        self.mods
            .catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .entries
                    .iter()
                    .filter(|(_, entry)| entry.live)
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(in crate::game) fn requested_mod_selection(&self) -> (Selection, Arc<Catalog>) {
        if self.remote.is_some()
            && self.integrated.is_none()
            && let Some(content) = &self.mods.content
        {
            return (content.selection.clone(), content.catalog.clone());
        }
        let catalog = self.mods.catalog.clone().unwrap_or_default();
        let selection = self
            .mods
            .pending
            .clone()
            .or_else(|| {
                self.mods
                    .content
                    .as_ref()
                    .map(|content| content.selection.clone())
            })
            .unwrap_or_default();
        (selection, catalog)
    }

    pub(in crate::game) fn switch_mods(&mut self) {
        let (Some(applied), Some(catalog)) = (&self.mods.content, &self.mods.catalog) else {
            return;
        };
        let plan = catalog::switch_plan(
            &applied.selection,
            &catalog.wanted_ids(&self.settings.mods.enabled, &self.settings.mods.disabled),
            catalog,
            self.integrated.is_some() || self.remote.is_none(),
        );
        self.ui.options.mods_note = (!plan.waiting.is_empty()).then(|| {
            lang::tr(
                "horizon.mods.switch.restart",
                &[&plan.waiting.into_iter().collect::<Vec<_>>().join(", ")],
            )
        });
        if self.mods.pending.as_ref() == Some(&plan.next) {
            return;
        }
        if plan.next == applied.selection && self.mods.pending.is_none() {
            return;
        }
        self.mods.pending = Some(plan.next);
        self.mods.live = Some(std::time::Instant::now());
        self.reload_packs();
    }

    pub(in crate::game) fn mods_reloaded(&mut self, success: bool) {
        if !success {
            return;
        }
        let Some(content) = self.pack.content.clone() else {
            return;
        };
        self.mods.install_content_snapshot(content.clone());
        self.mods.data_switched |= self.mods.live.is_some();
        if self.mods.pending.as_ref() == Some(&content.selection) {
            self.mods.pending = None;
            self.mods.live = None;
        }
    }

    pub(in crate::game) fn shutdown_mod_catalog(&mut self) -> bool {
        self.mod_transaction = None;
        self.mod_catalog_job
            .as_ref()
            .is_none_or(|job| job.shutdown())
    }
}
