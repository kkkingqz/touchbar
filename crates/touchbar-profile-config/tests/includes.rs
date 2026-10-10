use std::{fs, os::unix::fs::PermissionsExt};
use touchbar_profile_config::{ProfileDocument, load_with_sources};
const ROOT: &str = "version=1\nfallback=\"default\"\n[[profile]]\nid=\"default\"\n[[profile.element]]\nkind=\"flexible-space\"\nminimum=0\nweight=1\n";
fn write(root: &std::path::Path, name: &str, source: &str) {
    fs::write(root.join(name), source).unwrap();
    fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o600)).unwrap();
}
#[test]
fn legacy_document_unchanged_and_includes_have_explicit_order() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "root.toml", ROOT);
    assert_eq!(
        load_with_sources(&dir.path().join("root.toml"))
            .unwrap()
            .document,
        ProfileDocument::from_toml(ROOT).unwrap()
    );
    write(
        dir.path(),
        "root.toml",
        &ROOT.replace(
            "version=1",
            "version=1\ninclude=[\"second.toml\",\"first.toml\"]",
        ),
    );
    for name in ["first", "second"] {
        write(
            dir.path(),
            &format!("{name}.toml"),
            &format!(
                "version=1\n[[profile]]\nid=\"{name}\"\n[[profile.element]]\nkind=\"flexible-space\"\nminimum=0\nweight=1\n"
            ),
        );
    }
    let loaded = load_with_sources(&dir.path().join("root.toml")).unwrap();
    assert_eq!(
        loaded
            .document
            .profiles
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["default", "second", "first"]
    );
    assert_eq!(loaded.sources.len(), 3);
}
#[test]
fn nested_include_duplicate_id_and_unsafe_files_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = ROOT.replace("version=1", "version=1\ninclude=[\"part.toml\"]");
    write(dir.path(), "root.toml", &root);
    for part in [
        "version=1\ninclude=[\"nested.toml\"]",
        ROOT,
        "version=1\nfallback=\"default\"",
    ] {
        write(dir.path(), "part.toml", part);
        assert!(load_with_sources(&dir.path().join("root.toml")).is_err());
    }
    write(dir.path(), "part.toml", "version=1");
    fs::set_permissions(
        dir.path().join("part.toml"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    assert!(load_with_sources(&dir.path().join("root.toml")).is_err());
    fs::remove_file(dir.path().join("part.toml")).unwrap();
    std::os::unix::fs::symlink("root.toml", dir.path().join("part.toml")).unwrap();
    assert!(load_with_sources(&dir.path().join("root.toml")).is_err());
}
#[test]
fn include_file_count_and_total_byte_budgets_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let names = (0..63)
        .map(|i| format!("\"part{i}.toml\""))
        .collect::<Vec<_>>()
        .join(",");
    let root = ROOT.replace("version=1", &format!("version=1\ninclude=[{names}]"));
    write(dir.path(), "root.toml", &root);
    for i in 0..63 {
        write(dir.path(), &format!("part{i}.toml"), "version=1");
    }
    assert_eq!(
        load_with_sources(&dir.path().join("root.toml"))
            .unwrap()
            .sources
            .len(),
        64
    );
    write(
        dir.path(),
        "root.toml",
        &root.replace("include=[", "include=[\"extra.toml\","),
    );
    assert!(load_with_sources(&dir.path().join("root.toml")).is_err());
    let padded =
        |source: &str, size: usize| format!("{source}\n#{}", " ".repeat(size - source.len() - 2));
    write(
        dir.path(),
        "root.toml",
        &padded(
            &ROOT.replace(
                "version=1",
                "version=1\ninclude=[\"a.toml\",\"b.toml\",\"c.toml\"]",
            ),
            256 * 1024,
        ),
    );
    for name in ["a", "b", "c"] {
        write(
            dir.path(),
            &format!("{name}.toml"),
            &padded("version=1", 256 * 1024),
        );
    }
    assert!(load_with_sources(&dir.path().join("root.toml")).is_ok());
    write(
        dir.path(),
        "root.toml",
        &padded(
            &ROOT.replace(
                "version=1",
                "version=1\ninclude=[\"a.toml\",\"b.toml\",\"c.toml\",\"extra.toml\"]",
            ),
            256 * 1024,
        ),
    );
    write(dir.path(), "extra.toml", "version=1");
    assert!(load_with_sources(&dir.path().join("root.toml")).is_err());
    write(dir.path(), "a.toml", &padded("version=1", 256 * 1024 + 1));
    assert!(load_with_sources(&dir.path().join("root.toml")).is_err());
}
#[test]
fn symlinked_parent_and_escaping_includes_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    write(external.path(), "part.toml", "version=1");
    std::os::unix::fs::symlink(external.path(), dir.path().join("parts")).unwrap();
    for name in ["parts/part.toml", "../part.toml", "/etc/passwd"] {
        write(
            dir.path(),
            "root.toml",
            &ROOT.replace("version=1", &format!("version=1\ninclude=[\"{name}\"]")),
        );
        assert!(load_with_sources(&dir.path().join("root.toml")).is_err());
    }
}

#[test]
fn gnome_fragments_preserve_all_profiles_and_rules() {
    let root = tempfile::tempdir().unwrap();
    let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    std::fs::copy(
        config.join("profiles-gnome.toml.example"),
        root.path().join("profiles.toml"),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("profiles-gnome")).unwrap();
    for name in ["default", "browser", "office", "media"] {
        std::fs::copy(
            config.join(format!("profiles-gnome/{name}.toml.example")),
            root.path().join(format!("profiles-gnome/{name}.toml")),
        )
        .unwrap();
    }
    let loaded =
        touchbar_profile_config::load_with_sources(&root.path().join("profiles.toml")).unwrap();
    assert_eq!(loaded.sources.len(), 5);
    assert_eq!(loaded.document.profile_ids().len(), 4);
    assert_eq!(loaded.document.rules.len(), 4);
    assert_eq!(loaded.document.contributions.len(), 6);
}
