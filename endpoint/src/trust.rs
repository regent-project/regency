//! Read-only caches for client trust (D13): certificate pins of hosts
//! (`regency_state`) and the deny list (`regency_settings`), both following
//! CouchDB `_changes`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use regencylib::couch::{Couch, Since, follow};
use regencylib::model::{CertPins, DB_SETTINGS, DB_STATE, SETTINGS_DOC_ID, SettingsDoc};
use regencylib::telemetry::Health;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::identity::Identity;

#[derive(Default)]
pub struct TrustState {
    pins: RwLock<HashMap<String, CertPins>>,
    settings: RwLock<SettingsDoc>,
    /// (host, fingerprint) already announced as pending by this replica.
    announced: Mutex<HashSet<(String, String)>>,
}

/// Pinning decision for a self-signed certificate.
#[derive(Debug, PartialEq, Eq)]
pub enum Pin {
    Pinned,
    /// Another fingerprint is pinned for this host.
    Mismatch,
    /// Unknown; `announce` says whether this replica must publish it.
    Unpinned {
        announce: bool,
    },
}

impl TrustState {
    /// Follow both databases; readiness checks `pins` and `denylist`.
    pub fn spawn_followers(
        self: &Arc<Self>,
        couch: Couch,
        health: Health,
        cancel: CancellationToken,
    ) {
        health.register("pins");
        health.register("denylist");
        let state = self.clone();
        let (c, h, k) = (couch.clone(), health.clone(), cancel.clone());
        tokio::spawn(async move {
            follow(
                &c,
                DB_STATE,
                Since::Start,
                &k,
                |change| {
                    let Some(host_id) = change.id.strip_prefix("host:") else {
                        return;
                    };
                    let mut pins = state.pins.write().expect("pins lock");
                    if change.deleted {
                        pins.remove(host_id);
                        return;
                    }
                    let certs = change
                        .doc
                        .as_ref()
                        .and_then(|doc| doc.get("certs"))
                        .and_then(|certs| serde_json::from_value::<CertPins>(certs.clone()).ok())
                        .unwrap_or_default();
                    pins.insert(host_id.to_string(), certs);
                },
                || h.ready("pins"),
            )
            .await;
        });
        let state = self.clone();
        tokio::spawn(async move {
            follow(
                &couch,
                DB_SETTINGS,
                Since::Start,
                &cancel,
                |change| {
                    if change.id != SETTINGS_DOC_ID {
                        return;
                    }
                    let settings = if change.deleted {
                        SettingsDoc::default()
                    } else {
                        match change.doc.map(serde_json::from_value::<SettingsDoc>) {
                            Some(Ok(settings)) => settings,
                            other => {
                                warn!(error = ?other.map(|r| r.err()), "unreadable settings document, keeping the previous one");
                                return;
                            }
                        }
                    };
                    *state.settings.write().expect("settings lock") = settings;
                },
                || health.ready("denylist"),
            )
            .await;
        });
    }

    /// Why `identity` is revoked, if it is.
    pub fn revoked(&self, identity: &Identity) -> Option<&'static str> {
        let settings = self.settings.read().expect("settings lock");
        if settings.revoked_hosts().contains(&identity.host_id) {
            Some("host is revoked")
        } else if settings.revoked_serials().contains(&identity.serial) {
            Some("certificate serial is revoked")
        } else if settings
            .revoked_fingerprints()
            .contains(&identity.fingerprint)
        {
            Some("certificate fingerprint is revoked")
        } else {
            None
        }
    }

    pub fn pin(&self, identity: &Identity) -> Pin {
        let pins = self.pins.read().expect("pins lock");
        let certs = pins.get(&identity.host_id);
        match certs.and_then(|c| c.pinned.as_ref()) {
            Some((fingerprint, _)) if *fingerprint == identity.fingerprint => Pin::Pinned,
            Some(_) => Pin::Mismatch,
            None => {
                let pending = certs.is_some_and(|c| c.pending.contains(&identity.fingerprint));
                let first_time = self
                    .announced
                    .lock()
                    .expect("announced lock")
                    .insert((identity.host_id.clone(), identity.fingerprint.clone()));
                Pin::Unpinned {
                    announce: first_time && !pending,
                }
            }
        }
    }

    /// Trust-on-first-use: pin locally at once (dbwriter persists it).
    pub fn pin_locally(&self, identity: &Identity) {
        self.pins
            .write()
            .expect("pins lock")
            .entry(identity.host_id.clone())
            .or_default()
            .pinned = Some((identity.fingerprint.clone(), chrono::Utc::now()));
    }

    /// Forget an announcement whose publish failed (retried next time).
    pub fn unannounce(&self, identity: &Identity) {
        self.announced
            .lock()
            .expect("announced lock")
            .remove(&(identity.host_id.clone(), identity.fingerprint.clone()));
    }
}
