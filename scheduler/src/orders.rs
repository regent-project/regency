//! Firing recurring orders of the owned shards (D3, D12, D16).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use regencylib::hashing::bucket;
use regencylib::messages::recurring_key;
use regencylib::model::Order;
use regencylib::policy::{Principal, Resource, job_action};
use regencylib::schedule::Slot;
use regencylib::selector::resolve;
use regencylib::shard::Watchdog;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info, info_span, warn};

use crate::dispatch::{Body, Dispatcher, Plan};
use crate::state::State;

pub struct Skips(pub prometheus::IntCounterVec);

impl Skips {
    fn skip(&self, reason: &str) {
        self.0.with_label_values(&[reason]).inc();
    }
}

/// Fire due slots until `cancel`; ticks `watchdog` every round.
pub async fn fire_loop(
    state: State,
    dispatcher: Arc<Dispatcher>,
    mut owned: watch::Receiver<BTreeSet<u32>>,
    shards: u32,
    watchdog: Watchdog,
    cancel: CancellationToken,
    skips: Arc<Skips>,
) {
    // order id → (rev, slot) already fired by this process
    let mut fired: HashMap<String, (String, i64)> = HashMap::new();
    loop {
        watchdog.tick();
        let now = Utc::now();
        let shards_owned = owned.borrow_and_update().clone();
        if shards_owned.is_empty() {
            // a shard taken over later fires its current slot again: same
            // keys, duplicates dropped downstream (D3)
            fired.clear();
        }
        for (order, rev) in state.orders() {
            if !order.enabled || !owned.borrow().contains(&bucket(&order.order_id, shards)) {
                continue;
            }
            let slot = match order.schedule.slot_at(now) {
                Ok(slot) => slot,
                Err(e) => {
                    warn!(order_id = %order.order_id, error = %e, "order has an invalid schedule");
                    continue;
                }
            };
            if slot.start > now || fired.get(&order.order_id) == Some(&(rev.clone(), slot.number)) {
                continue;
            }
            let span =
                info_span!("fire", order_id = %order.order_id, slot = slot.number, rev = %rev);
            match fire(&state, &dispatcher, &order, &rev, slot, &skips, &watchdog)
                .instrument(span)
                .await
            {
                Ok(()) => {
                    fired.insert(order.order_id.clone(), (rev, slot.number));
                }
                Err(e) => {
                    warn!(order_id = %order.order_id, error = %e, "firing failed, retried next round")
                }
            }
            watchdog.tick();
        }
        let live: BTreeSet<String> = state
            .orders()
            .into_iter()
            .map(|(o, _)| o.order_id)
            .collect();
        fired.retain(|id, _| live.contains(id));
        // next round at the next second boundary
        let wait = 1000 - (Utc::now().timestamp_subsec_millis() as u64).min(999);
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
        }
    }
}

async fn fire(
    state: &State,
    dispatcher: &Dispatcher,
    order: &Order,
    rev: &str,
    slot: Slot,
    skips: &Skips,
    watchdog: &Watchdog,
) -> Result<(), String> {
    let assignments = state.assignments();
    let Some(assignment) = assignments.get(&order.assignment) else {
        warn!(assignment = %order.assignment, "slot skipped: unknown assignment");
        skips.skip("unknown_assignment");
        return Ok(());
    };
    let (hostdefs, inventories) = state.hosts();
    let resolution = resolve(
        &assignment.selector,
        &hostdefs,
        &inventories,
        order.pool.as_deref(),
    );
    for host_id in &resolution.conflicting {
        warn!(host_id = %host_id, "host skipped: conflicting definitions (D11)");
        skips.skip("conflicting_host");
    }
    for host_id in &resolution.unknown {
        warn!(host_id = %host_id, "host skipped: not in the catalog");
        skips.skip("unknown_host");
    }
    // D12: the author is re-checked at fire time
    let Some(mut author) = Principal::parse(&order.author) else {
        warn!(author = %order.author, "slot skipped: unreadable author");
        skips.skip("bad_author");
        return Ok(());
    };
    author.groups = order.author_groups.clone();
    let authorizer = state.authorizer();
    let action = job_action(order.job);
    let resources = Resource::hosts(&hostdefs);
    let (targets, denied): (Vec<_>, Vec<_>) = resolution.targets.into_iter().partition(|t| {
        let resource = resources
            .get(&t.host_id)
            .cloned()
            .unwrap_or_else(|| Resource::host_of(&t.host_id, []));
        authorizer.is_authorized(&author, action, &resource)
    });
    if !denied.is_empty() {
        warn!(
            principal = %order.author,
            denied = denied.len(),
            hosts = ?denied.iter().map(|t| t.host_id.as_str()).collect::<Vec<_>>(),
            "hosts skipped: the author may no longer run this order"
        );
        skips.skip("unauthorized");
    }
    if targets.is_empty() {
        info!("slot fired with no target");
        return Ok(());
    }
    let ttl = (slot.end - Utc::now())
        .to_std()
        .unwrap_or(Duration::from_millis(1));
    let order_id = order.order_id.clone();
    let rev_owned = rev.to_string();
    let key = move |host: &str| recurring_key(&order_id, &rev_owned, slot.number, host);
    let plan = Plan {
        job: order.job,
        body: Body::Effective(&assignments),
        created_at: slot.start,
        origin: &order.order_id,
        ttl: Some(ttl),
        key: &key,
        progress: &|| watchdog.tick(),
    };
    let published = dispatcher.dispatch(&targets, &plan).await?;
    info!(published, ttl_ms = ttl.as_millis() as u64, "slot fired");
    Ok(())
}
