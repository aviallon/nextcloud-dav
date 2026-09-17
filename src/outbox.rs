// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The `oc_dav_event_outbox` producer interface.
//!
//! Native `PUT`/`DELETE` writes the card and a single outbox row in the *same*
//! transaction, then wakes the PHP dispatcher with a transactional
//! `pg_notify`. This module owns the two things that must stay frozen with the
//! companion PHP app:
//!
//! * the **effect ownership registry** — every effect id is claimed by exactly
//!   one backend (`php` in phase 1, optionally `rust` later). The ownership map
//!   is frozen into each row at write time so a later config change never
//!   re-routes an in-flight event.
//! * the **schema probe** — the sidecar never creates the table (the PHP app's
//!   migration owns it) and refuses native writes when the table or a column is
//!   missing.
//!
//! Phase 1 performs **no** dispatch itself: it only queues rows for the PHP
//! worker. See `docs/recon/event-dispatch.md`.

use crate::config::EventDispatchConfig;
use crate::model::Card;
use indexmap::IndexMap;
use serde_json::{json, Value};

/// Operation values stored in `oc_dav_event_outbox.event_type` and
/// `oc_addressbookchanges.operation`. Mirrors `CardDavBackend::addChange()`.
pub const EVENT_CREATE: i16 = 1;
pub const EVENT_UPDATE: i16 = 2;
pub const EVENT_DELETE: i16 = 3;

/// Every effect the CardDAV listeners produce, in the order PHP registers
/// them. A later phase may move an entry to the `rust` backend through
/// [`EventDispatchConfig::handlers`]; names must never be removed or reordered
/// without a coordinated companion-app change.
pub const KNOWN_EFFECTS: &[&str] = &[
    "activity_stream",
    "activity_mail",
    "notification_push",
    "birthday_calendar",
    "calendar_reminders",
    "photo_cache",
    "redis_cloud_id",
];

/// The table columns the producer relies on. Used by the startup probe.
pub const OUTBOX_COLUMNS: &[&str] = &[
    "seq",
    "created_at",
    "event_type",
    "addressbookid",
    "card_uri",
    "card_row",
    "card_data",
    "effects",
    "state",
    "attempts",
    "next_attempt_at",
    "reserved_by",
    "reserved_at",
    "processed_at",
    "last_error",
];

/// The per-effect owner map, frozen into each outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRegistry {
    owners: IndexMap<String, String>,
}

impl Default for EffectRegistry {
    fn default() -> Self {
        Self::from_config(&EventDispatchConfig::default())
    }
}

impl EffectRegistry {
    /// Builds the registry from config: every known effect defaults to `php`,
    /// then explicit `handlers` entries move an effect to `rust`. Unknown effect
    /// ids in the config are ignored; an unknown *backend* value is treated as
    /// `php` (defensive: never silently claim an effect for a backend that does
    /// not exist).
    pub fn from_config(config: &EventDispatchConfig) -> Self {
        let mut owners = IndexMap::new();
        for effect in KNOWN_EFFECTS {
            owners.insert((*effect).to_string(), "php".to_string());
        }
        for (effect, backend) in &config.handlers {
            if !owners.contains_key(effect) {
                log::warn!("event_dispatch.handlers ignores unknown effect {effect:?}");
                continue;
            }
            let backend = if backend == "rust" { "rust" } else { "php" };
            owners.insert(effect.clone(), backend.to_string());
        }
        Self { owners }
    }

    /// The `effects` JSON frozen into the outbox row:
    /// `{"php":[...],"rust":[...]}`.
    pub fn effects_json(&self) -> String {
        let mut php: Vec<&str> = Vec::new();
        let mut rust: Vec<&str> = Vec::new();
        for (effect, backend) in &self.owners {
            if backend == "rust" {
                rust.push(effect);
            } else {
                php.push(effect);
            }
        }
        serde_json::to_string(&json!({ "php": php, "rust": rust }))
            .expect("effect registry always serialises")
    }

    /// The effects this sidecar would execute in-process. Phase 1 is always
    /// empty; it is exposed so the deviation test can assert the sidecar queues
    /// but never dispatches.
    pub fn rust_effects(&self) -> Vec<&str> {
        self.owners
            .iter()
            .filter(|(_, backend)| backend.as_str() == "rust")
            .map(|(effect, _)| effect.as_str())
            .collect()
    }
}

/// The `card_row` JSON snapshot: `{id,uri,lastmodified,etag,size,uid}`.
///
/// `card` is the post-`readBlob()` row (the same shape as PHP's `getCard()`),
/// so the ETag is the **quoted** wire form and `size` already accounts for a
/// filtered photo blob.
pub fn card_row_json(card: &Card, uid: &str) -> Value {
    json!({
        "id": card.id,
        "uri": card.uri,
        "lastmodified": card.lastmodified,
        "etag": card.quoted_etag(),
        "size": card.size,
        "uid": uid,
    })
}

/// Verifies that `<prefix>dav_event_outbox` exists with every column the
/// producer needs. The sidecar must never create it.
pub async fn verify_schema(pool: &sqlx::AnyPool, prefix: &str) -> Result<(), String> {
    // A `SELECT <cols> ... LIMIT 0` probes both the table and the columns in one
    // statement. `render_placeholders` is irrelevant: there are no bind values.
    let columns = OUTBOX_COLUMNS.join(", ");
    let sql = format!("SELECT {columns} FROM {prefix}dav_event_outbox LIMIT 0");
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_one_registry_is_all_php() {
        let registry = EffectRegistry::default();
        assert_eq!(
            registry.effects_json(),
            r#"{"php":["activity_stream","activity_mail","notification_push","birthday_calendar","calendar_reminders","photo_cache","redis_cloud_id"],"rust":[]}"#
        );
        assert!(registry.rust_effects().is_empty());
    }

    #[test]
    fn a_configured_effect_moves_backend_exactly_once() {
        let mut config = EventDispatchConfig::default();
        config
            .handlers
            .insert("redis_cloud_id".into(), "rust".into());
        config.handlers.insert("bogus".into(), "rust".into());
        let registry = EffectRegistry::from_config(&config);
        assert_eq!(registry.rust_effects(), vec!["redis_cloud_id"]);
        let parsed: Value = serde_json::from_str(&registry.effects_json()).unwrap();
        let php = parsed["php"].as_array().unwrap();
        let rust = parsed["rust"].as_array().unwrap();
        assert_eq!(rust.len(), 1);
        assert_eq!(php.len(), KNOWN_EFFECTS.len() - 1);
        assert!(!php.iter().any(|v| v == "redis_cloud_id"));
    }
}
