//! Identity registry tests: persisted key → canonical-user map backing the
//! personal-context federation. Pure logic + filesystem behavior in temp dirs
//! (no server, no network).

use harness_server::identity::IdentityRegistry;

/// Unique temp dir per call (no `tempfile` dep; created + left for the OS).
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "vh-identity-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn missing_file_starts_empty_and_first_key_registers() {
    let dir = temp_dir("missing");
    let path = dir.join("identities.json");

    let mut reg = IdentityRegistry::load(&path).expect("registry loads");
    assert!(
        reg.is_empty(),
        "a missing registry file must load as an empty map"
    );

    // No key in the map yet: the first key registers as the canonical user.
    let canonical = reg.canonical_user_for(&["icloud:rec-1".into(), "platform:ABC".into()]);
    assert_eq!(canonical, "icloud:rec-1", "first key overall registers");
}

#[test]
fn first_key_already_in_map_wins_over_first_key_overall() {
    let dir = temp_dir("precedence");
    let path = dir.join("identities.json");

    let mut reg = IdentityRegistry::load(&path).expect("registry loads");
    // Device A registers under its iCloud record name.
    let _ = reg.canonical_user_for(&["icloud:rec-1".into(), "platform:AAA".into()]);
    reg.save().expect("save");

    // Device B announces the platform key first but shares the iCloud key:
    // the key already present in the map must win (federation works even
    // though clients compute keys in their own precedence order).
    let mut reg2 = IdentityRegistry::load(&path).expect("reload");
    let canonical = reg2.canonical_user_for(&[
        "platform:BBB".into(),
        "icloud:rec-1".into(),
        "email:e@x".into(),
    ]);
    assert_eq!(
        canonical, "icloud:rec-1",
        "a shared key groups devices under the first device's canonical user"
    );
}

#[test]
fn persistence_roundtrip() {
    let dir = temp_dir("roundtrip");
    let path = dir.join("nested").join("identities.json");

    let mut reg = IdentityRegistry::load(&path).expect("registry loads");
    let _ = reg.canonical_user_for(&["icloud:rec-9".into(), "email:me@x".into()]);
    reg.save().expect("save creates the parent dir");

    let mut reg2 = IdentityRegistry::load(&path).expect("reload");
    assert!(!reg2.is_empty(), "registry persisted across reload");
    // A later device sharing only the email key resolves to the same user.
    let canonical = reg2.canonical_user_for(&["email:me@x".into()]);
    assert_eq!(canonical, "icloud:rec-9");
}

#[test]
fn save_sets_owner_only_permissions() {
    let dir = temp_dir("perms");
    let path = dir.join("identities.json");

    let mut reg = IdentityRegistry::load(&path).expect("registry loads");
    let _ = reg.canonical_user_for(&["icloud:rec-1".into()]);
    reg.save().expect("save");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("registry file exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "registry must be owner-read/write only, got {:o}",
            mode & 0o777
        );
    }
}

#[test]
fn empty_keys_get_fresh_unique_ids() {
    let dir = temp_dir("anon");
    let path = dir.join("identities.json");

    let mut reg = IdentityRegistry::load(&path).expect("registry loads");
    let a = reg.canonical_user_for(&[]);
    let b = reg.canonical_user_for(&[]);
    assert!(!a.is_empty(), "anonymous sessions still get a canonical id");
    assert_ne!(a, b, "each keyless session gets a fresh id");
    // Anonymous ids must never register into the persisted map.
    assert!(reg.is_empty(), "anonymous ids are not persisted");
}

#[test]
fn existing_file_loads_its_map() {
    let dir = temp_dir("existing");
    let path = dir.join("identities.json");
    std::fs::write(
        &path,
        r#"{"icloud:rec-2":"icloud:rec-2","platform:ZZZ":"icloud:rec-2"}"#,
    )
    .expect("seed registry file");

    let mut reg = IdentityRegistry::load(&path).expect("registry loads");
    assert_eq!(
        reg.canonical_user_for(&["platform:ZZZ".into()]),
        "icloud:rec-2"
    );
    // Unrelated keys still register fresh.
    assert_eq!(
        reg.canonical_user_for(&["icloud:other".into()]),
        "icloud:other"
    );
}
