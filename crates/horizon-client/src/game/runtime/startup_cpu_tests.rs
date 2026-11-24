use super::*;

fn write_preflight_fixture(root: &std::path::Path) {
    std::fs::write(
        root.join("horizon-resources.json"),
        serde_json::json!({"data_version": horizon_core::version::DATA_VERSION}).to_string(),
    )
    .unwrap();
    for name in [
        "facts.json",
        "block-tags.json",
        "block-behaviours.json",
        "gameplay-facts.json",
        "items.json",
    ] {
        std::fs::write(root.join(name), "{}").unwrap();
    }
}

#[cfg(unix)]
#[test]
fn prepared_preflight_names_a_broken_json_table_link() {
    let root = std::env::temp_dir().join(format!("horizon-preflight-link-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    write_preflight_fixture(&root);
    std::fs::remove_file(root.join("gameplay-facts.json")).unwrap();
    std::os::unix::fs::symlink("missing.json", root.join("gameplay-facts.json")).unwrap();
    let problem = missing_resources(&root, None);
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        problem
            .as_deref()
            .is_some_and(|message| message.contains("gameplay-facts.json")),
        "{problem:?}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires a non-root Unix process to exercise denied filesystem access"]
fn prepared_preflight_reports_denied_manifest_and_directory_access() {
    use std::os::unix::fs::PermissionsExt;
    let root =
        std::env::temp_dir().join(format!("horizon-preflight-access-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let manifest = root.join("horizon-resources.json");
    write_preflight_fixture(&root);
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o000)).unwrap();
    let read_denied = std::fs::read(&manifest).is_err();
    let unreadable = missing_resources(&root, None);
    std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o111)).unwrap();
    let manifest_readable = std::fs::read(&manifest).is_ok();
    let listing_denied = std::fs::read_dir(&root).is_err();
    let unlistable = missing_resources(&root, None);
    // Restore and remove the owned fixture before asserting the production
    // results, including when this regression is run against the broken code.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        read_denied && manifest_readable && listing_denied,
        "run without root privileges"
    );
    assert!(
        unreadable
            .as_deref()
            .is_some_and(|message| message.contains("horizon-resources.json"))
            && unlistable.is_some(),
        "denied access was reported as healthy: manifest={unreadable:?}, directory={unlistable:?}"
    );
}

#[test]
fn a_folder_without_prepared_files_is_named_before_loading() {
    let root = std::env::temp_dir().join(format!("horizon-unprepared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let unprepared = root.join(".local/horizon-client/26.3");
    let missing = missing_resources(&unprepared, None).expect("no manifest is reported");
    assert!(missing.contains("horizon-resources.json"), "{missing}");
    assert!(missing.contains("horizon-unprepared-"), "{missing}");
    assert!(missing.contains("horizon-prepare --prepare"), "{missing}");
    // No hook: no site, no launcher.
    assert!(!missing.contains("playhorizon"), "{missing}");
    assert!(!missing.contains("launcher"), "{missing}");
    let get = horizon_hooks::GetTheGame {
        address: "https://example.org/get",
        title: "",
        paragraphs: &[],
        button: "",
    };
    let missing = missing_resources(&unprepared, Some(get)).expect("no manifest is reported");
    assert!(
        missing.contains("launcher: download it at https://example.org/get"),
        "{missing}"
    );
    std::fs::create_dir_all(&root).unwrap();
    write_preflight_fixture(&root);
    assert_eq!(missing_resources(&root, None), None);

    // A killed export leaves an empty file behind; the error names it
    // instead of serde's "expected value at line 1 column 1".
    std::fs::write(root.join("gameplay-facts.json"), "").unwrap();
    let damaged = missing_resources(&root, None).expect("an empty prepared file is reported");
    assert!(damaged.contains("gameplay-facts.json"), "{damaged}");
    assert!(damaged.contains("damaged"), "{damaged}");
    std::fs::write(root.join("gameplay-facts.json"), "{}").unwrap();
    assert_eq!(missing_resources(&root, None), None);

    std::fs::write(root.join("horizon-resources.json"), " \n").unwrap();
    let damaged = missing_resources(&root, None).expect("an empty manifest is reported");
    assert!(damaged.contains("horizon-resources.json"), "{damaged}");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn worker_prepares_owned_models_and_inventory_before_handoff() {
    let root = std::env::temp_dir().join(format!("horizon-startup-cpu-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("entity-models.json"),
        r#"{
        "minecraft:end_crystal#main": {}, "minecraft:ender_dragon#main": {},
        "minecraft:arrow#main": {}, "minecraft:trident#main": {},
        "minecraft:armor_stand#main": {}, "minecraft:cushion#main": {},
        "minecraft:leash_knot#main": {}, "minecraft:wind_charge#main": {},
        "minecraft:shulker_bullet#main": {}, "minecraft:wither_skull#main": {},
        "minecraft:evoker_fangs#main": {}, "minecraft:standing_banner#main": {},
        "minecraft:standing_banner#flag": {}
    }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("creative-tabs.json"),
        r#"[{
        "id":"minecraft:building_blocks", "items":["minecraft:stone"]
    }]"#,
    )
    .unwrap();
    let worker_root = root.clone();
    let prepared = std::thread::spawn(move || {
        GameCpuResources::load(
            &horizon_assets::resource_source::ResourceRoot::directory(worker_root),
            None,
        )
        .map_err(|error| error.to_string())
    })
    .join()
    .unwrap()
    .unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(prepared.crystal_model.is_some());
    assert!(prepared.cushion_model.is_some());
    let models = &prepared.entity_models;
    assert!(models.leash_knot.is_some() && models.wind_charge.is_some());
    assert!(models.shulker_bullet.is_some() && models.wither_skull.is_some());
    assert!(models.evoker_fangs.is_some() && models.banner.is_some());
    assert!(prepared.dragon_model.is_some());
    assert!(prepared.arrow_model.is_some());
    assert!(prepared.trident_model.is_some());
    assert!(prepared.armor_stand_assets.main.is_some());
    assert_eq!(
        prepared.creative_screen.tabs[0].items[0].item.as_ref(),
        "minecraft:stone"
    );
    // No documents from the old generation remain available to reload.
    assert!(
        GameCpuResources::load(
            &horizon_assets::resource_source::ResourceRoot::directory(root.clone()),
            None
        )
        .is_err()
    );
}
