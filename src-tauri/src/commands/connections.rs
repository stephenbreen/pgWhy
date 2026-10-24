//! Connection manager commands.
//!
//! Credential flow is one-way: secrets arrive in `SecretInput`s and go straight to the vault;
//! no response ever carries one back (entries only say whether one is saved).

use crate::state::AppState;
use pgwhy_core::config::{ConnectionEntry, ConnectionId, TunnelAuth};
use pgwhy_core::conn::{ConnInfo, Purpose};
use pgwhy_core::secret::{Secret, SecretKey, SecretKind};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use tauri::State;

/// What the form wants done with a secret field. Blank-with-"unchanged" semantics: editing an
/// entry never needs to round-trip the stored password.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SecretInput {
    Unchanged,
    Clear,
    Set { value: String, persist: bool },
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionsSnapshot {
    pub connections: Vec<ConnectionEntry>,
    pub recents: Vec<ConnectionId>,
    /// false → the OS credential store is unavailable; passwords are session-only
    pub keyring_available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SaveResult {
    pub entry: ConnectionEntry,
    /// e.g. another entry points at the same server+database with a different env tag
    pub warnings: Vec<String>,
    /// where the password went, if one was set
    pub password_stored: Option<pgwhy_core::secret::CredentialSource>,
}

fn snapshot(state: &AppState) -> ConnectionsSnapshot {
    let file = state.connections.lock().unwrap_or_else(|p| p.into_inner());
    ConnectionsSnapshot {
        connections: file.connections.clone(),
        recents: file.recents.clone(),
        keyring_available: state.vault.persistent_available(),
    }
}

#[tauri::command]
pub fn list_connections(state: State<'_, AppState>) -> ConnectionsSnapshot {
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| {
        if let Some(t0) = crate::STARTED.get() {
            tracing::info!(
                elapsed_ms = t0.elapsed().as_millis() as u64,
                "cold start: connection list requested by the UI"
            );
        }
    });
    snapshot(&state)
}

fn apply_secret(
    state: &AppState,
    id: &ConnectionId,
    kind: SecretKind,
    input: &SecretInput,
) -> AppResult<Option<pgwhy_core::secret::CredentialSource>> {
    let key = SecretKey::new(id, kind);
    match input {
        SecretInput::Unchanged => Ok(None),
        SecretInput::Clear => {
            state.vault.forget(&key)?;
            Ok(None)
        }
        SecretInput::Set { value, persist } => state
            .vault
            .store(&key, Secret::new(value.clone()), *persist)
            .map(Some),
    }
}

fn validate(entry: &ConnectionEntry) -> AppResult<()> {
    let missing = |what: &str| AppError::new(ErrorKind::Config, format!("{what} is required"));
    if entry.name.trim().is_empty() {
        return Err(missing("A name"));
    }
    if entry.host.trim().is_empty() {
        return Err(missing("A host"));
    }
    if entry.user.trim().is_empty() {
        return Err(missing("A user"));
    }
    if entry.database.trim().is_empty() {
        return Err(missing("A database"));
    }
    if let Some(t) = &entry.tunnel {
        if t.host.trim().is_empty() || t.user.trim().is_empty() {
            return Err(missing("An SSH host and user"));
        }
        if let TunnelAuth::Key { key_path, .. } = &t.auth {
            if key_path.as_os_str().is_empty() {
                return Err(missing("An SSH key file"));
            }
        }
    }
    Ok(())
}

fn tag_warnings(all: &[ConnectionEntry], entry: &ConnectionEntry) -> Vec<String> {
    all.iter()
        .filter(|o| {
            o.id != entry.id && o.server_key() == entry.server_key() && o.database == entry.database
        })
        .filter(|o| o.env_tag != entry.env_tag)
        .map(|o| {
            format!(
                "\"{}\" points at the same server and database but is tagged {} (this one is {})",
                o.name,
                o.env_tag.label(),
                entry.env_tag.label()
            )
        })
        .collect()
}

#[tauri::command]
pub async fn save_connection(
    state: State<'_, AppState>,
    entry: ConnectionEntry,
    password: SecretInput,
    ssh_secret: SecretInput,
) -> AppResult<SaveResult> {
    let mut entry = entry;
    validate(&entry)?;
    let password_stored = apply_secret(&state, &entry.id, SecretKind::DbPassword, &password)?;
    let ssh_kind = match entry.tunnel.as_ref().map(|t| &t.auth) {
        Some(TunnelAuth::Password { .. }) => Some(SecretKind::SshPassword),
        Some(TunnelAuth::Key { .. }) => Some(SecretKind::SshKeyPassphrase),
        None => None,
    };
    let ssh_stored = match ssh_kind {
        Some(kind) => apply_secret(&state, &entry.id, kind, &ssh_secret)?,
        None => None,
    };

    // "saved" flags reflect the persistent store only; session secrets aren't "saved"
    let persisted = |kind: SecretKind,
                     stored: Option<pgwhy_core::secret::CredentialSource>,
                     input: &SecretInput,
                     was: bool| {
        match input {
            SecretInput::Unchanged => {
                was && state
                    .vault
                    .resolve(&SecretKey::new(&entry.id, kind))
                    .is_ok_and(|(s, _)| s.is_some())
            }
            SecretInput::Clear => false,
            SecretInput::Set { .. } => {
                stored == Some(pgwhy_core::secret::CredentialSource::Keyring)
            }
        }
    };
    entry.password_saved = persisted(
        SecretKind::DbPassword,
        password_stored,
        &password,
        entry.password_saved,
    );
    if let (Some(kind), Some(t)) = (ssh_kind, entry.tunnel.as_mut()) {
        match &mut t.auth {
            TunnelAuth::Password { password_saved } => {
                *password_saved = persisted(kind, ssh_stored, &ssh_secret, *password_saved)
            }
            TunnelAuth::Key {
                passphrase_saved, ..
            } => *passphrase_saved = persisted(kind, ssh_stored, &ssh_secret, *passphrase_saved),
        }
    }

    let warnings = {
        let mut file = state.connections.lock().unwrap_or_else(|p| p.into_inner());
        let warnings = tag_warnings(&file.connections, &entry);
        file.upsert(entry.clone());
        warnings
    };
    state.save_connections()?;
    state.manager.upsert_entry(entry.clone()).await;
    state.forget_catalog(&entry.id);
    Ok(SaveResult {
        entry,
        warnings,
        password_stored,
    })
}

#[tauri::command]
pub async fn delete_connection(state: State<'_, AppState>, id: ConnectionId) -> AppResult<()> {
    state.manager.remove_entry(&id).await;
    let _ = state.tunnels.disconnect(&id, true);
    state.forget_catalog(&id);
    let _ = state.expansion.forget(&id);
    state
        .connections
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id);
    state.save_connections()?;
    state.vault.forget_connection(&id)?;
    Ok(())
}

/// Try an (possibly unsaved) entry. `password` overrides any saved one, for the form's Test
/// button before saving.
#[tauri::command]
pub async fn test_connection(
    state: State<'_, AppState>,
    entry: ConnectionEntry,
    password: Option<String>,
) -> AppResult<ConnInfo> {
    validate(&entry)?;
    state.manager.test(&entry, password.map(Secret::new)).await
}

/// Connect an entry's default database for the explorer and record it as recently used.
/// A password typed at a prompt can be passed for this session (and optionally saved).
#[tauri::command]
pub async fn connect(
    state: State<'_, AppState>,
    id: ConnectionId,
    password: Option<SecretInput>,
    ssh_secret: Option<SecretInput>,
) -> AppResult<ConnInfo> {
    if let Some(input) = &ssh_secret {
        let entry = state.manager.entry(&id)?;
        let kind = match entry.tunnel.as_ref().map(|t| &t.auth) {
            Some(TunnelAuth::Password { .. }) => SecretKind::SshPassword,
            _ => SecretKind::SshKeyPassphrase,
        };
        apply_secret(&state, &id, kind, input)?;
    }
    if let Some(input) = &password {
        let stored = apply_secret(&state, &id, SecretKind::DbPassword, input)?;
        if stored == Some(pgwhy_core::secret::CredentialSource::Keyring) {
            let mut file = state.connections.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(mut e) = file.get(&id).cloned() {
                e.password_saved = true;
                file.upsert(e);
            }
        }
    }
    let entry = state.manager.entry(&id)?;
    let conn = state.manager.introspection(&id, &entry.database).await?;
    state
        .connections
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .touch_recent(&id);
    state.save_connections()?;
    Ok(conn.info.clone())
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>, id: ConnectionId) -> AppResult<()> {
    state.manager.disconnect(&id).await;
    // leases were released with the connections; a still-pinned tunnel (running workflow)
    // stays up and the refusal is not an error for a UI disconnect
    let _ = state.tunnels.disconnect(&id, false);
    Ok(())
}

/// Live connections, for the status bar and diagnostics.
#[tauri::command]
pub fn live_connections(state: State<'_, AppState>) -> Vec<ConnInfo> {
    state
        .manager
        .live_connections()
        .into_iter()
        .filter(|c| c.purpose != Purpose::Test)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, tag: pgwhy_core::config::EnvTag) -> ConnectionEntry {
        let mut e: ConnectionEntry = serde_json::from_value(serde_json::json!({
            "name": name, "host": "db.example.com", "database": "app", "user": "me"
        }))
        .unwrap();
        e.env_tag = tag;
        e
    }

    #[test]
    fn warns_when_same_target_has_different_tags() {
        use pgwhy_core::config::EnvTag;
        let a = entry("prod", EnvTag::Production);
        let b = entry("oops", EnvTag::Local);
        let w = tag_warnings(std::slice::from_ref(&a), &b);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("Production"));
        let c = entry("same", EnvTag::Production);
        assert!(tag_warnings(&[a], &c).is_empty());
    }

    #[test]
    fn validation_requires_core_fields() {
        let mut e = entry("x", pgwhy_core::config::EnvTag::Local);
        e.host = " ".into();
        assert_eq!(validate(&e).unwrap_err().kind, ErrorKind::Config);
    }
}
