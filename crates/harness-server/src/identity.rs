//! Persisted identity registry for personal-context federation: maps client
//! identity keys (iCloud record name, platform UUID, me-contact email — in
//! client-chosen precedence order) to a canonical user id.
//!
//! On `session.start` the server resolves the announced keys here: the first
//! key already present in the map wins (that's what groups two devices of the
//! same person), otherwise the first key overall registers as a new canonical
//! user. Sessions without keys (unknown device, offline iCloud, old client)
//! get a fresh unique id — they federate with nothing.
//!
//! The map persists as JSON at `[personal_context.federation].registry_path`,
//! written atomically (tmp file + rename) with owner-only permissions, so a
//! crash mid-write can never corrupt it and other users on the machine cannot
//! read it.

use std::collections::HashMap;
use std::path::Path;

/// A key → canonical-user map. `mutating` methods register new keys;
/// [`IdentityRegistry::save`] must be called to persist them.
#[derive(Debug, Clone, Default)]
pub struct IdentityRegistry {
    map: HashMap<String, String>,
    path: std::path::PathBuf,
}

impl IdentityRegistry {
    /// Load the registry from `path`. A missing file is an empty map (first
    /// boot); an unparseable file is an error — silently starting over would
    /// re-register every device as a new user.
    pub fn load(path: &Path) -> Result<Self, String> {
        let map = if path.exists() {
            let bytes = std::fs::read(path).map_err(|e| format!("read registry: {e}"))?;
            serde_json::from_slice(&bytes).map_err(|e| format!("parse registry: {e}"))?
        } else {
            HashMap::new()
        };
        Ok(Self {
            map,
            path: path.to_path_buf(),
        })
    }

    /// Resolve `keys` (client precedence order) to a canonical user id.
    /// Empty keys → fresh unique id (never registered, never persisted).
    pub fn canonical_user_for(&mut self, keys: &[String]) -> String {
        // First key already in the map wins: shared keys group devices.
        if let Some(key) = keys.iter().find(|k| self.map.contains_key(*k)) {
            return self.map[key].clone();
        }
        let Some(first) = keys.first() else {
            // Anonymous session: unique per resolution, nothing persisted.
            return format!("anon-{}", next_counter());
        };
        // First key overall registers as a new canonical user; EVERY
        // announced key maps to it, so a later device sharing any key
        // (iCloud record or email, whichever both know) federates.
        let canonical = first.clone();
        for key in keys {
            self.map.insert(key.clone(), canonical.clone());
        }
        canonical
    }

    /// Persist the map: atomic write (tmp + rename), owner-only mode,
    /// parent directories created. Idempotent when nothing changed.
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| format!("create registry dir: {e}"))?;
            }
        }
        let bytes =
            serde_json::to_vec_pretty(&self.map).map_err(|e| format!("encode registry: {e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write registry: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod registry: {e}"))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename registry: {e}"))?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Monotonic counter for anonymous canonical ids (process-local; uniqueness
/// across restarts is not required — anon ids are never persisted).
fn next_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_user_for_registers_first_key() {
        let mut reg = IdentityRegistry::default();
        let canonical = reg.canonical_user_for(&["platform:P".into(), "email:e".into()]);
        assert_eq!(canonical, "platform:P");
        // A sibling key of the same device resolves to the same user.
        assert_eq!(reg.canonical_user_for(&["email:e".into()]), "platform:P");
    }
}
