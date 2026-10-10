use std::path::PathBuf;
use touchbar_package::PluginManifest;
use touchbar_set_config::SetDocument;
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn baseline() -> serde_json::Value {
    serde_json::from_str(include_str!("remaining_sets_baseline.json")).unwrap()
}
#[test]
fn first_party_layouts_have_no_commands() {
    for name in ["capture", "hyprland", "command-deck", "omarchy"] {
        let dir = root().join("plugins").join(name);
        let text = std::fs::read_to_string(dir.join("touchbar-plugin.toml")).unwrap();
        let manifest = PluginManifest::from_toml(&text).unwrap();
        assert!(text.contains("kind = \"button-set-logic\""), "{name}");
        let expected = baseline();
        assert_eq!(
            manifest.sets.len(),
            expected[name]["items"].as_array().unwrap().len()
        );
        for item in expected[name]["items"].as_array().unwrap() {
            let id = item["id"].as_str().unwrap();
            let reference = manifest.sets.iter().find(|s| s.id == id).unwrap();
            let source = std::fs::read_to_string(dir.join(&reference.file)).unwrap();
            let set = SetDocument::from_toml(&source).unwrap();
            assert_eq!(set.id, id);
            fn widgets(node: &touchbar_set_config::LayoutNode, out: &mut Vec<u64>) {
                if let Some(id) = node.widget_id {
                    out.push(id);
                }
                for child in &node.children {
                    widgets(child, out);
                }
            }
            let mut ids = vec![];
            widgets(&set.layout, &mut ids);
            let wanted: &[u64] = match id {
                "screenshots" => &[1, 2, 3, 4],
                "recording" => &[10],
                "capture-tools" => &[20, 21, 22],
                "workspaces" => &[11, 10],
                "window-actions" => &[20, 21],
                "move-window" => &[31, 30],
                "quick-actions" => &[1, 2, 3],
                "launchers" => &[10, 11, 12],
                "session" => &[20, 21],
                "custom-actions" => &[100, 101, 102, 103],
                "screensaver" => &[1],
                "palette" => &[],
                _ => panic!("unexpected set"),
            };
            assert_eq!(ids, wanted, "{name}/{id} widgets");

            assert_eq!(
                set.default_width,
                item["default_width"].as_u64().unwrap_or(160) as u32
            );
            assert_eq!(
                set.show_in_default_profile,
                item["show_in_default_profile"].as_bool().unwrap_or(true)
            );
            assert!(
                !source.contains("executable")
                    && !source.contains("command.run")
                    && !source.contains("/usr/bin/")
            );
        }
        let raw: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            serde_json::to_value(raw.get("permission")).unwrap(),
            expected[name]["permission"],
            "{name} scopes"
        );
    }
}
#[test]
fn omarchy_provider_is_unchanged() {
    let text =
        std::fs::read_to_string(root().join("plugins/omarchy/touchbar-plugin.toml")).unwrap();
    let raw: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        serde_json::to_value(raw.get("appearance-provider")).unwrap(),
        baseline()["omarchy"]["appearance-provider"]
    );
}
#[test]
fn omarchy_geometry_runs_with_default_guest_budget() {
    let dir = root().join("plugins/omarchy");
    let bytes = std::fs::read(dir.join("component/plugin.wasm")).unwrap();
    let set = SetDocument::from_toml(
        &std::fs::read_to_string(dir.join("sets/screensaver.toml")).unwrap(),
    )
    .unwrap();
    let mut host = touchbar_plugin_host::sets::ButtonSetHost::from_bytes_with_broker(
        &bytes,
        set,
        touchbar_plugin_host::HostLimits::default(),
        None,
    )
    .unwrap();
    for width in [80.0, 320.0, 2008.0] {
        host.render_at(
            "screensaver",
            touchbar_ui::Rect::new(0.0, 0.0, width, 60.0),
            touchbar_plugin_host::Appearance::default(),
            std::time::Duration::ZERO,
        )
        .unwrap();
        let touchbar_set_config::StateValue::List(mark) = &host.snapshot().0["mark"] else {
            panic!()
        };
        assert_eq!(mark.len(), 290);
    }
}
#[test]
fn original_panel_composition_and_profile_selection_are_preserved() {
    let expected = baseline();
    for name in ["capture", "hyprland", "command-deck", "omarchy"] {
        let dir = root().join("plugins").join(name);
        let manifest = PluginManifest::from_toml(
            &std::fs::read_to_string(dir.join("touchbar-plugin.toml")).unwrap(),
        )
        .unwrap();
        let files = touchbar_package::read_set_files(&dir, &manifest).unwrap();
        let catalog = touchbar_package::resolve_set_catalog(&manifest, &files).unwrap();
        let bars: Vec<touchbar_package::PresentationBar> =
            serde_json::from_value(expected[name]["bar"].clone()).unwrap();
        assert_eq!(catalog.bars, bars, "{name} panels");
        let profiles: Vec<touchbar_package::AutomaticProfile> =
            serde_json::from_value(expected[name]["profile"].clone()).unwrap();
        assert_eq!(catalog.profiles, profiles, "{name} profiles");
    }
}
