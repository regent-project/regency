//! In-memory copies of what the scheduler reads (D8: orders, catalog,
//! policies), each following its database's `_changes`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use regencylib::assignment::Assignments;
use regencylib::couch::{Change, Couch, Since, follow};
use regencylib::model::{
    CatalogBody, CatalogItem, DB_CATALOG, DB_ORDERS, DB_POLICIES, HostDef, InventoryDef, Order,
    PolicyDoc,
};
use regencylib::policy::Authorizer;
use regencylib::telemetry::Health;
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Default)]
struct Inner {
    /// order id → (order, CouchDB `_rev`)
    orders: HashMap<String, (Order, String)>,
    catalog: HashMap<String, CatalogItem>,
    policies: HashMap<String, PolicyDoc>,
}

#[derive(Clone)]
pub struct State {
    inner: Arc<RwLock<Inner>>,
    authorizer: Arc<RwLock<Authorizer>>,
    bootstrap_admin: String,
}

fn parse<T: serde::de::DeserializeOwned>(change: &Change) -> Option<T> {
    let doc = change.doc.as_ref()?;
    match serde_json::from_value(doc.clone()) {
        Ok(value) => Some(value),
        Err(e) => {
            warn!(doc_id = %change.id, error = %e, "unreadable document ignored");
            None
        }
    }
}

impl State {
    pub fn new(bootstrap_admin: String) -> Self {
        Self {
            inner: Arc::default(),
            authorizer: Arc::new(RwLock::new(Authorizer::new(
                &[],
                Some(bootstrap_admin.clone()),
            ))),
            bootstrap_admin,
        }
    }

    pub fn spawn_followers(&self, couch: &Couch, health: &Health, cancel: &CancellationToken) {
        for (db, check) in [
            (DB_ORDERS, "orders"),
            (DB_CATALOG, "catalog"),
            (DB_POLICIES, "policies"),
        ] {
            health.register(check);
            let (state, couch, health, cancel) =
                (self.clone(), couch.clone(), health.clone(), cancel.clone());
            tokio::spawn(async move {
                follow(
                    &couch,
                    db,
                    Since::Start,
                    &cancel,
                    |change| state.apply(db, change),
                    || health.ready(check),
                )
                .await;
            });
        }
    }

    fn apply(&self, db: &str, change: Change) {
        if change.id.starts_with("_design/") {
            return;
        }
        let mut inner = self.inner.write().expect("state lock");
        match db {
            DB_ORDERS => {
                let Some(order_id) = change.id.strip_prefix("order:") else {
                    return;
                };
                inner.orders.remove(order_id);
                if !change.deleted
                    && let Some(order) = parse::<Order>(&change)
                {
                    let rev = change
                        .doc
                        .as_ref()
                        .and_then(|d| d["_rev"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    inner.orders.insert(order_id.to_string(), (order, rev));
                }
            }
            DB_CATALOG => {
                inner.catalog.remove(&change.id);
                if !change.deleted
                    && let Some(item) = parse::<CatalogItem>(&change)
                {
                    inner.catalog.insert(change.id.clone(), item);
                }
            }
            DB_POLICIES => {
                inner.policies.remove(&change.id);
                if !change.deleted
                    && let Some(policy) = parse::<PolicyDoc>(&change)
                {
                    inner.policies.insert(change.id.clone(), policy);
                }
                let docs: Vec<PolicyDoc> = inner.policies.values().cloned().collect();
                *self.authorizer.write().expect("authorizer lock") =
                    Authorizer::new(&docs, Some(self.bootstrap_admin.clone()));
            }
            _ => {}
        }
    }

    pub fn orders(&self) -> Vec<(Order, String)> {
        self.inner
            .read()
            .expect("state lock")
            .orders
            .values()
            .cloned()
            .collect()
    }

    /// Host definitions and inventories of the whole catalog.
    pub fn hosts(&self) -> (Vec<HostDef>, Vec<InventoryDef>) {
        let inner = self.inner.read().expect("state lock");
        let mut hostdefs = Vec::new();
        let mut inventories = Vec::new();
        for item in inner.catalog.values() {
            match &item.body {
                CatalogBody::Hostdef(def) => hostdefs.push(def.clone()),
                CatalogBody::Inventory(def) => inventories.push(def.clone()),
                _ => {}
            }
        }
        (hostdefs, inventories)
    }

    /// Assignments and expected states at their latest revision (D11, D23).
    pub fn assignments(&self) -> Assignments {
        Assignments::from_catalog(self.inner.read().expect("state lock").catalog.values())
    }

    pub fn authorizer(&self) -> Authorizer {
        self.authorizer.read().expect("authorizer lock").clone()
    }
}
