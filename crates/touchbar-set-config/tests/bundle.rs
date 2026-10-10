use std::collections::BTreeMap;
use touchbar_set_config::SetBundle;
#[test]
fn bundle_roundtrip_and_unexpected_data_are_bounded() {
    let files = BTreeMap::from([("sets/browser.toml".into(), b"version=1".to_vec())]);
    let encoded = SetBundle::encode(&files).unwrap();
    assert_eq!(SetBundle::decode(&encoded).unwrap().files, files);
    for end in 0..encoded.len() {
        assert!(SetBundle::decode(&encoded[..end]).is_err());
    }
    let mut appended = encoded;
    appended.push(0);
    assert!(SetBundle::decode(&appended).is_err());
    assert!(SetBundle::encode(&BTreeMap::from([("../escape".into(), vec![])])).is_err());
    assert!(
        SetBundle::encode(&BTreeMap::from([(
            "sets/huge.toml".into(),
            vec![0; 256 * 1024 + 1]
        )]))
        .is_err()
    );
}

#[test]
fn duplicate_members_are_rejected() {
    let encoded = SetBundle::encode(&BTreeMap::from([("sets/a.toml".into(), vec![1])])).unwrap();
    let mut duplicated = encoded.clone();
    duplicated[4..8].copy_from_slice(&2u32.to_le_bytes());
    duplicated.extend_from_slice(&encoded[8..]);
    assert!(SetBundle::decode(&duplicated).is_err());
}

#[test]
fn bundle_paths_must_be_literal_normalized_paths() {
    for path in [
        "sets/./a.toml",
        "./sets/a.toml",
        "sets//a.toml",
        "sets/../a.toml",
    ] {
        assert!(
            SetBundle::encode(&BTreeMap::from([(path.into(), vec![1])])).is_err(),
            "{path}"
        );
    }
}
