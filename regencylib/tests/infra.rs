//! Integration tests against real RabbitMQ and CouchDB (docker needed):
//! `cargo test -p regencylib -- --ignored`.

use std::collections::BTreeSet;
use std::time::Duration;

use regencylib::couch::Couch;
use regencylib::model::{HostDoc, Stored};
use regencylib::shard::{ShardSpec, Watchdog, run};
use regencylib::testing::{infra, unique};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "needs docker"]
async fn shards_are_shared_and_handed_over() {
    let infra = infra().await;
    let prefix = unique("test.lock.");
    let spec = ShardSpec {
        lock_prefix: prefix.clone(),
        item: "shard".into(),
        members_queue: format!("{prefix}.members"),
        count: 8,
        retry: Duration::from_millis(300),
        watchdog_timeout: Duration::from_secs(30),
    };
    let cancel = CancellationToken::new();
    let mut members = Vec::new();
    for i in 0..2 {
        let connection =
            regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, &format!("test/{i}"))
                .await
                .unwrap();
        let (tx, rx) = watch::channel(BTreeSet::new());
        let spec = spec.clone();
        let cancel = cancel.clone();
        let watchdog = Watchdog::new();
        let ticker = watchdog.clone();
        let handle = tokio::spawn(async move {
            let ticking = async {
                loop {
                    ticker.tick();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            tokio::select! {
                result = run(&spec, &connection, &watchdog, &tx, &cancel) => { if let Err(e) = &result { eprintln!("member {i}: run failed: {e}"); } let _ = connection.close(200, "bye".into()).await; result }
                _ = ticking => unreachable!(),
            }
        });
        members.push((rx, handle));
    }
    // converge: 4 + 4, disjoint
    let mut converged = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let a = members[0].0.borrow().clone();
        let b = members[1].0.borrow().clone();
        if a.len() == 4 && b.len() == 4 && a.is_disjoint(&b) {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "shards did not converge to 4+4: {:?} / {:?}",
        members[0].0.borrow(),
        members[1].0.borrow()
    );

    // stop member 1: member 0 takes all 8
    let (_, handle) = members.pop().unwrap();
    handle.abort();
    let _ = handle.await;
    let mut took_over = false;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if members[0].0.borrow().len() == 8 {
            took_over = true;
            break;
        }
    }
    cancel.cancel();
    assert!(took_over, "remaining member did not take every shard");
}

#[tokio::test]
#[ignore = "needs docker"]
async fn merge_upsert_resolves_conflicts() {
    let infra = infra().await;
    let couch = Couch::from_config(&infra.couchdb, &infra.secrets)
        .await
        .unwrap();
    let db = unique("test_state_");
    assert!(couch.create_db(&db).await.unwrap());
    assert!(!couch.create_db(&db).await.unwrap());

    let t0 = chrono::Utc::now();
    let mut a = HostDoc::empty("h", t0);
    a.state.compliance = "not_compliant".into();
    a.state_at = Some(t0);
    let mut b = HostDoc::empty("h", t0 + chrono::Duration::seconds(5));
    b.state.compliance = "compliant".into();
    b.state_at = Some(t0 + chrono::Duration::seconds(5));
    b.last_seen = t0 + chrono::Duration::seconds(5);

    // concurrent writers
    let (ra, rb) = tokio::join!(
        couch.merge_upsert(&db, "host:h", &a, HostDoc::merge),
        couch.merge_upsert(&db, "host:h", &b, HostDoc::merge)
    );
    ra.unwrap();
    rb.unwrap();
    let stored: Stored<HostDoc> = couch.get(&db, "host:h").await.unwrap().unwrap();
    assert_eq!(stored.doc.state.compliance, "compliant");
    assert_eq!(stored.doc.first_seen, t0);

    // a CouchDB-level conflict (new_edits=false), as replication would create
    let mut leaf = serde_json::to_value(Stored::new("host:h", a.clone())).unwrap();
    leaf["_rev"] = serde_json::json!("9-deadbeef");
    let response = reqwest::Client::new()
        .post(format!("{}/{}/_bulk_docs", infra.couchdb.url, db))
        .basic_auth("admin", Some("admin"))
        .json(&serde_json::json!({"docs": [leaf], "new_edits": false}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let conflicted: Stored<HostDoc> = couch.get(&db, "host:h").await.unwrap().unwrap();
    assert_eq!(conflicted.conflicts.len(), 1);
    couch
        .merge_upsert(&db, "host:h", &b, HostDoc::merge)
        .await
        .unwrap();
    let resolved: Stored<HostDoc> = couch.get(&db, "host:h").await.unwrap().unwrap();
    assert!(resolved.conflicts.is_empty());
    assert_eq!(resolved.doc.state.compliance, "compliant");
    assert!(couch.delete_db(&db).await.unwrap());
}
