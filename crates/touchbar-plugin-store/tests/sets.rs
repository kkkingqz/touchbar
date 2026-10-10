use std::fs;
use touchbar_plugin_store::{PluginStore, StorePaths, inspect_package, pack_directory};

const MANIFEST: &str = r#"
manifest_version = 1
[plugin]
name="Sets"
version="0.1.0"
description="Declarative sets"
license="MIT"
source="github:example/sets"
api="^1.0"
[runtime]
kind="button-set-logic"
entrypoint="component/plugin.wasm"
world="touchbar:plugin/button-set-logic@1.0.0"
[[sets]]
id="browser"
file="sets/browser.toml"
"#;
const SET: &str = r#"
version=1
id="browser"
label="Browser"
default_width=240
[selection]
applications=["firefox"]
priority=10
[layout]
kind="button"
widget_id=10
label="Back"
on_activate="back"
"#;
fn package() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("component")).unwrap();
    fs::create_dir(root.path().join("sets")).unwrap();
    fs::write(root.path().join("touchbar-plugin.toml"), MANIFEST).unwrap();
    fs::write(
        root.path().join("component/plugin.wasm"),
        b"test-wasm-unchanged",
    )
    .unwrap();
    fs::write(root.path().join("sets/browser.toml"), SET).unwrap();
    root
}
#[test]
fn set_files_are_locked_artifacts_and_items_install_without_legacy_items() {
    let root = package();
    let inspected = inspect_package(root.path()).unwrap();
    assert!(inspected.artifacts.contains_key("sets/browser.toml"));
    assert_eq!(inspected.manifest.catalog_items()[0].id, "browser");
    assert_eq!(inspected.manifest.catalog_items()[0].default_width, 240);
    assert_eq!(
        inspected.manifest.catalog_profiles()[0].applications,
        vec!["firefox"]
    );
    let home = tempfile::tempdir().unwrap();
    let mut store = PluginStore::open(StorePaths::under(home.path().join("store"))).unwrap();
    let installed = store.install_directory(root.path()).unwrap();
    assert_eq!(installed.items[0].id, "browser");
    assert!(!installed.enabled);
}
#[test]
fn mixed_items_and_sets_rejected() {
    let root = package();
    fs::write(
        root.path().join("touchbar-plugin.toml"),
        format!("{MANIFEST}\n[[items]]\nid=\"legacy\"\nlabel=\"Legacy\"\n"),
    )
    .unwrap();
    assert!(inspect_package(root.path()).is_err());
}
#[test]
fn set_id_must_match_reference_and_path_escape_rejected() {
    let root = package();
    fs::write(
        root.path().join("sets/browser.toml"),
        SET.replace("id=\"browser\"", "id=\"other\""),
    )
    .unwrap();
    assert!(inspect_package(root.path()).is_err());
    fs::write(
        root.path().join("touchbar-plugin.toml"),
        MANIFEST.replace("sets/browser.toml", "../browser.toml"),
    )
    .unwrap();
    assert!(inspect_package(root.path()).is_err());
}
#[test]
fn archive_roundtrip_keeps_sets_and_layout_only_update_preserves_wasm() {
    let root = package();
    let first = inspect_package(root.path()).unwrap();
    fs::write(
        root.path().join("sets/browser.toml"),
        SET.replace("label=\"Back\"", "label=\"Previous\""),
    )
    .unwrap();
    let second = inspect_package(root.path()).unwrap();
    assert_ne!(first.package_digest, second.package_digest);
    assert_eq!(
        first.artifacts["component/plugin.wasm"],
        second.artifacts["component/plugin.wasm"]
    );
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("sets.touchbar");
    pack_directory(root.path(), &archive).unwrap();
    let mut store = PluginStore::open(StorePaths::under(out.path().join("store"))).unwrap();
    let installed = store.install_archive(&archive).unwrap();
    assert_eq!(
        installed.artifacts["sets/browser.toml"],
        second.artifacts["sets/browser.toml"]
    );
    assert!(
        store
            .package_path(&installed)
            .unwrap()
            .join("sets/browser.toml")
            .exists()
    );
}

#[test]
fn set_parent_directory_cannot_be_a_symlink() {
    let root = package();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("browser.toml"), SET).unwrap();
    fs::remove_dir_all(root.path().join("sets")).unwrap();
    std::os::unix::fs::symlink(external.path(), root.path().join("sets")).unwrap();
    assert!(inspect_package(root.path()).is_err());
}

#[test]
fn panel_reuses_owner_without_creating_another_item() {
    let root = package();
    let source = SET.replace(
        "default_width=240",
        "default_width=240\nexpanded_panel=\"browser-details\"",
    );
    fs::write(root.path().join("sets/browser.toml"),format!("{source}\n[[panels]]\nid=\"browser-details\"\nminimum_width=120\npreferred_width=240\nmaximum_width=480\n")).unwrap();
    let inspected = inspect_package(root.path()).unwrap();
    assert_eq!(inspected.manifest.catalog_items().len(), 1);
    assert_eq!(
        inspected.manifest.catalog_items()[0]
            .expanded_bar
            .as_deref(),
        Some("browser-details")
    );
    assert_eq!(
        inspected.manifest.catalog_bars()[0]
            .principal_item
            .as_deref(),
        Some("browser")
    );
}

#[test]
fn direct_set_reader_rejects_fifo_without_blocking() {
    let root = package();
    fs::remove_file(root.path().join("sets/browser.toml")).unwrap();
    let path = std::ffi::CString::new(
        root.path()
            .join("sets/browser.toml")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    // SAFETY: path is a live NUL-terminated name for a private test FIFO.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let manifest = touchbar_package::PluginManifest::from_toml(MANIFEST).unwrap();
    assert!(touchbar_package::read_set_files(root.path(), &manifest).is_err());
}
