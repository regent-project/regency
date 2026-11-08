//! Full-stack smoke test against a running dev stack (dev/README.md):
//! logs in as the bootstrap admin, defines an API source with this machine
//! as a localhost host, runs an Assess and follows the job to its end.
//!
//!   cargo run -p backend --example smoke -- [http://127.0.0.1:8080]

use std::time::Duration;

use futures::StreamExt;
use regencylib::api::regency_client::RegencyClient;
use regencylib::model::*;
use regencylib::proto as api;
use regencylib::selector::HostSelector;
use regent_sdk::ExpectedState;
use regent_sdk::hosts::handlers::{ConnectionMethod, TargetUser};
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use tonic::Request;

fn authed<T>(message: T, token: &str) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let password = std::fs::read_to_string("dev/secrets/bootstrap-admin")?;
    let mut client = RegencyClient::connect(url).await?;
    let mut login = api::LoginRequest::new();
    login.set_username("admin");
    login.set_password(password.trim());
    let token = client.login(login).await?.into_inner().token().to_string();
    println!("logged in as admin");

    let stamp = chrono::Utc::now().timestamp();
    let items = [
        CatalogBody::Source(SourceDef {
            source_id: "smoke".into(),
            kind: SourceKind::Api,
            repo: None,
            branch: None,
            path: None,
            poll_interval_sec: 300,
            credentials: None,
        }),
        CatalogBody::Hostdef(HostDef {
            source_id: "smoke".into(),
            host_id: "smoke-localhost".into(),
            builder: ManagedHostBuilder::new(
                "smoke-localhost",
                "localhost",
                Some(ConnectionMethod::Localhost(TargetUser::current_user())),
            ),
            inventories: Default::default(),
            labels: [("env".to_string(), "dev".to_string())].into(),
            pool: None,
        }),
        CatalogBody::ExpectedState(ExpectedStateDef {
            source_id: "smoke".into(),
            name: "empty".into(),
            state: ExpectedState::new(),
        }),
        CatalogBody::Assignment(AssignmentDef {
            source_id: "smoke".into(),
            name: "dev".into(),
            selector: HostSelector::parse_terms(["label:env=dev"]).map_err(anyhow::Error::msg)?,
            expected_state: "smoke/empty".into(),
            priority: 0,
        }),
    ];
    for (i, body) in items.into_iter().enumerate() {
        let item = CatalogItem {
            body,
            revision: "1".into(),
        };
        let mut request = api::UpsertCatalogItemRequest::new();
        request.set_command_id(format!("smoke-catalog-{stamp}-{i}"));
        request.set_item_json(serde_json::to_string(&item)?);
        client.upsert_catalog_item(authed(request, &token)).await?;
        // the source must exist before its items are accepted (D11)
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("catalog items sent");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let command_id = format!("smoke-run-{stamp}");
    let mut watch = api::WatchJobsRequest::new();
    watch.set_origin(command_id.as_str());
    let mut jobs = client.watch_jobs(authed(watch, &token)).await?.into_inner();
    let mut run = api::RunNowRequest::new();
    run.set_command_id(command_id.as_str());
    let mut selector = api::HostSelector::new();
    selector.labels_mut().insert("env", "dev");
    run.set_selector(selector);
    run.set_job(api::JobKind::Assess);
    client.run_now(authed(run, &token)).await?;
    println!("run {command_id} accepted");

    while let Some(event) = tokio::time::timeout(Duration::from_secs(60), jobs.next()).await? {
        let event = event?;
        if event.has_upsert() {
            let job = event.upsert();
            println!(
                "job {} on {}: {} {}",
                job.job_id(),
                job.host_id(),
                job.state(),
                job.compliance()
            );
            if job.state() == "FINISHED" {
                println!("smoke test passed");
                return Ok(());
            }
        }
    }
    anyhow::bail!("the job did not finish")
}
