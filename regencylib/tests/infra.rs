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

/// Several consumers on one channel (`amqp::consume_on`, the runner's Reach
/// partitions): cancelling one gives back what it was delivered but had not
/// started, keeps the channel open and the other consumers running.
#[tokio::test]
#[ignore = "needs docker"]
async fn consumers_share_a_channel_and_cancel_alone() {
    use lapin::BasicProperties;
    use lapin::options::{BasicPublishOptions, QueueDeclareOptions};
    use lapin::types::{FieldTable, ShortString};
    use regencylib::amqp::{Handled, consume_on};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let infra = infra().await;
    let connection = regencylib::amqp::connect(&infra.rabbitmq, &infra.secrets, "test/shared")
        .await
        .unwrap();
    let setup = connection.create_channel().await.unwrap();
    let queues: Vec<String> = (0..3)
        .map(|i| unique(&format!("test.shared.{i}.")))
        .collect();
    for queue in &queues {
        setup
            .queue_declare(
                ShortString::from(queue.as_str()),
                QueueDeclareOptions::default(),
                FieldTable::default(),
            )
            .await
            .unwrap();
    }
    let publish = |queue: String, n: usize| {
        let setup = setup.clone();
        async move {
            for i in 0..n {
                setup
                    .basic_publish(
                        ShortString::from(""),
                        ShortString::from(queue.as_str()),
                        BasicPublishOptions::default(),
                        format!("{i}").as_bytes(),
                        BasicProperties::default(),
                    )
                    .await
                    .unwrap()
                    .await
                    .unwrap();
            }
        }
    };

    let shared = connection.create_channel().await.unwrap();
    let handled: Vec<Arc<AtomicUsize>> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let gate = Arc::new(tokio::sync::Notify::new()); // holds consumer 0's first message
    let tokens: Vec<CancellationToken> = (0..3).map(|_| CancellationToken::new()).collect();
    let mut tasks = Vec::new();
    for i in 0..3 {
        let (channel, queue, count, token, gate) = (
            shared.clone(),
            queues[i].clone(),
            handled[i].clone(),
            tokens[i].clone(),
            gate.clone(),
        );
        tasks.push(tokio::spawn(async move {
            consume_on(&channel, &queue, &format!("c{i}"), 1, &token, move |_| {
                let (count, gate) = (count.clone(), gate.clone());
                async move {
                    if i == 0 {
                        gate.notified().await;
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    Handled::Ack
                }
            })
            .await
        }));
    }
    // consumer 0 is stuck on its first message (prefetch 1: the others stay
    // in the queue); consumers 1 and 2 work
    publish(queues[0].clone(), 3).await;
    publish(queues[1].clone(), 5).await;
    publish(queues[2].clone(), 5).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while handled[1].load(Ordering::SeqCst) < 5 || handled[2].load(Ordering::SeqCst) < 5 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "consumers 1 and 2 stalled"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // cancel consumer 0: its in-flight message is finished first
    tokens[0].cancel();
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.notify_one();
    tasks.remove(0).await.unwrap().unwrap();
    assert_eq!(handled[0].load(Ordering::SeqCst), 1);

    // the channel is still open and consumer 1 still consumes
    assert!(shared.status().connected());
    publish(queues[1].clone(), 2).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while handled[1].load(Ordering::SeqCst) < 7 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "consumer 1 stopped with consumer 0"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // what consumer 0 had not handled is still in its queue
    let left = setup
        .queue_declare(
            ShortString::from(queues[0].as_str()),
            QueueDeclareOptions {
                passive: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap()
        .message_count();
    assert_eq!(left, 2);

    // a closed channel ends the others with `Ended` (the runner restarts them)
    shared.close(200, ShortString::from("test")).await.unwrap();
    for task in tasks {
        assert!(matches!(
            task.await.unwrap(),
            Err(regencylib::amqp::ConsumeError::Ended)
        ));
    }
}
