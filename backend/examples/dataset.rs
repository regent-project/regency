//! Representative development dataset for a running dev stack (dev/README.md;
//! `dev/run_locally` runs it): API source `dataset` with thousands of hosts in
//! a dozen inventories, labels, a few expected states layered by
//! assignments (D23: `baseline` for every host but the unassigned `bastion`
//! ones, `web`/`db`/`edge` on top) and
//! recurring orders, then a first Assess of everything so the map fills at
//! once.
//!
//!   cargo run -p backend --example dataset -- [--hosts N] [--url URL] [--no-run]
//!
//! Every host is this machine (Localhost) with its own directory
//! `/tmp/regency-dev/hosts/<host>/` (the files its expected states check) and
//! its own lock (`/tmp/regency-dev/locks/<host>`), so hosts are independent
//! for D4's host lock. Each run rewrites those files with a fixed mix of
//! compliant hosts and drift; `legacy` hosts are unreachable SSH hosts
//! (error outcomes); `edge` hosts belong to the `edge` runner pool.
//! The catalog is only written when it differs (deterministic command ids);
//! hosts beyond `--hosts` are removed from the catalog.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use regencylib::api::regency_client::RegencyClient;
use regencylib::model::*;
use regencylib::proto as api;
use regent_sdk::ExpectedState;
use regent_sdk::hosts::handlers::ssh2::Ssh2Auth;
use regent_sdk::hosts::handlers::{ConnectionMethod, TargetUser};
use regent_sdk::hosts::lock::HostLockSettings;
use regent_sdk::hosts::managed_host::ManagedHostBuilder;
use regent_sdk::secrets::SecretReference;
use tonic::Request;
use tonic::transport::Channel;

const SOURCE: &str = "dataset";
const ROOT: &str = "/tmp/regency-dev";
/// Bump when the generated definitions change (part of the command ids).
const VERSION: &str = "v4";

/// (inventory, share of the hosts, runner pool, role)
const INVENTORIES: &[(&str, u32, Option<&str>, &str)] = &[
    ("web", 22, None, "frontend"),
    ("api", 16, None, "backend"),
    ("worker", 12, None, "batch"),
    ("k8s", 18, None, "platform"),
    ("db", 5, None, "database"),
    ("cache", 6, None, "database"),
    ("queue", 3, None, "platform"),
    ("monitoring", 2, None, "platform"),
    ("ci", 4, None, "tooling"),
    ("bastion", 1, None, "tooling"),
    ("edge", 8, Some("edge"), "edge"),
    ("legacy", 3, None, "legacy"),
];
const REGIONS: &[&str] = &["eu-west-1", "us-east-1", "ap-southeast-1"];
const TEAMS: &[&str] = &["payments", "search", "identity", "platform", "data"];

const BASELINE: &str = r#"---
Attributes:
  - Name: login banner
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/motd"
      State: !Present
        Create: true
      Line: !Raw "Managed by regency: unauthorized access is prohibited"
  - Name: no root login over SSH
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/ssh/sshd_config"
      State: !Absent
        FileMustExistAnyway: false
      Line: !Raw "PermitRootLogin yes"
  - Name: swappiness
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/sysctl.conf"
      State: !Present
        Create: true
      Line: !Raw "vm.swappiness = 10"
  - Name: inventory check
    Privilege: !None
    Timeout: 20s
    Detail: !Debug
      Msg: "baseline checked on {{ host }}"
"#;

const WEB: &str = r#"---
Attributes:
  - Name: nginx workers
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/nginx/nginx.conf"
      State: !Present
        Create: true
      Line: !Raw "worker_processes auto;"
  - Name: hide nginx version
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/nginx/nginx.conf"
      State: !Present
        Create: true
      Line: !Raw "server_tokens off;"
"#;

const DB: &str = r#"---
Attributes:
  - Name: connection limit
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/postgresql/postgresql.conf"
      State: !Present
        Create: true
      Line: !Raw "max_connections = 500"
  - Name: TLS only
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/postgresql/postgresql.conf"
      State: !Present
        Create: true
      Line: !Raw "ssl = on"
"#;

const EDGE: &str = r#"---
Attributes:
  - Name: drop invalid packets
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/firewall.rules"
      State: !Present
        Create: true
      Line: !Raw "-A INPUT -m conntrack --ctstate INVALID -j DROP"
  - Name: edge cache warm-up
    Privilege: !None
    Timeout: 20s
    Detail: !Debug
      Msg: "edge {{ host }} ready"
"#;

/// FNV-1a: stable across runs and machines.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Stable pseudo-random value of a host, in 0..100.
fn roll(host: &str, salt: &str) -> u64 {
    fnv(&format!("{salt}:{host}")) % 100
}

struct Host {
    id: String,
    inventories: BTreeSet<String>,
    labels: BTreeMap<String, String>,
    unreachable: bool,
}

fn hosts(total: u32) -> Vec<Host> {
    let shares: u32 = INVENTORIES.iter().map(|i| i.1).sum();
    let mut list = Vec::new();
    for (inventory, share, _, role) in INVENTORIES {
        let count = (total * share).div_ceil(shares).max(1);
        for n in 1..=count {
            let region = REGIONS[(n as usize) % REGIONS.len()];
            let short: String = region.split('-').map(|p| &p[..1]).collect();
            let id = format!("{inventory}-{short}-{n:04}");
            let env = match roll(&id, "env") {
                0..70 => "prod",
                70..90 => "staging",
                _ => "dev",
            };
            let mut labels: BTreeMap<String, String> = [
                ("env".to_string(), env.to_string()),
                ("region".to_string(), region.to_string()),
                ("role".to_string(), role.to_string()),
            ]
            .into();
            if !matches!(*inventory, "bastion" | "monitoring" | "legacy") {
                let team = TEAMS[roll(&id, "team") as usize % TEAMS.len()];
                labels.insert("team".into(), team.into());
            }
            let mut inventories: BTreeSet<String> = [inventory.to_string()].into();
            // a few hosts in two inventories (dashed links on the map)
            if *inventory == "worker" && n % 5 == 0 {
                inventories.insert("k8s".into());
            }
            if *inventory == "cache" && n % 4 == 0 {
                inventories.insert("db".into());
            }
            list.push(Host {
                id,
                inventories,
                labels,
                unreachable: *inventory == "legacy",
            });
        }
    }
    list
}

fn builder(host: &Host) -> ManagedHostBuilder {
    let (endpoint, method) = if host.unreachable {
        // nothing listens on port 1: an error outcome (connection refused)
        let key = SecretReference::from("dev/secrets/legacy-ssh-key", None);
        (
            "127.0.0.1:1",
            ConnectionMethod::Ssh2(Ssh2Auth::key("deploy", key)),
        )
    } else {
        (
            "localhost",
            ConnectionMethod::Localhost(TargetUser::current_user()),
        )
    };
    let mut builder = ManagedHostBuilder::new(&host.id, endpoint, Some(method));
    builder.host_vars = Some([("host".to_string(), host.id.clone())].into());
    builder.set_host_lock(Some(HostLockSettings {
        path: format!("{ROOT}/locks/{}", host.id),
        ..Default::default()
    }));
    builder
}

/// Write the host's files: most hosts compliant, some drifting.
fn prepare_files(host: &Host) -> std::io::Result<()> {
    let root = Path::new(ROOT).join("hosts").join(&host.id);
    for dir in ["etc/ssh", "etc/nginx", "etc/postgresql"] {
        std::fs::create_dir_all(root.join(dir))?;
    }
    let drift = |what: &str, percent: u64| roll(&host.id, what) < percent;
    let mut motd = String::from("Welcome\n");
    if !drift("motd", 12) {
        motd.push_str("Managed by regency: unauthorized access is prohibited\n");
    }
    let mut sshd = String::from("Port 22\nPasswordAuthentication no\n");
    if drift("sshd", 6) {
        sshd.push_str("PermitRootLogin yes\n");
    }
    let swappiness = if drift("sysctl", 8) { 60 } else { 10 };
    let mut nginx = String::from("worker_processes auto;\n");
    if !drift("nginx", 15) {
        nginx.push_str("server_tokens off;\n");
    }
    let mut postgres = String::from("max_connections = 500\n");
    if !drift("postgres", 20) {
        postgres.push_str("ssl = on\n");
    }
    let firewall = if drift("firewall", 25) {
        "-A INPUT -j ACCEPT\n"
    } else {
        "-A INPUT -m conntrack --ctstate INVALID -j DROP\n"
    };
    std::fs::write(root.join("etc/motd"), motd)?;
    std::fs::write(root.join("etc/ssh/sshd_config"), sshd)?;
    std::fs::write(
        root.join("etc/sysctl.conf"),
        format!("vm.swappiness = {swappiness}\n"),
    )?;
    std::fs::write(root.join("etc/nginx/nginx.conf"), nginx)?;
    std::fs::write(root.join("etc/postgresql/postgresql.conf"), postgres)?;
    std::fs::write(root.join("etc/firewall.rules"), firewall)?;
    Ok(())
}

fn authed<T>(message: T, token: &str) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

fn doc_id(body: &CatalogBody) -> String {
    match body {
        CatalogBody::Source(s) => format!("source:{}", s.source_id),
        CatalogBody::Inventory(i) => format!("inventory:{}:{}", i.source_id, i.name),
        CatalogBody::Hostdef(h) => format!("hostdef:{}:{}", h.source_id, h.host_id),
        CatalogBody::ExpectedState(e) => format!("expected_state:{}:{}", e.source_id, e.name),
        CatalogBody::Assignment(a) => format!("assignment:{}:{}", a.source_id, a.name),
    }
}

fn selector(inventories: &[&str]) -> api::HostSelector {
    let mut selector = api::HostSelector::new();
    for inventory in inventories {
        selector
            .inventories_mut()
            .push(format!("{SOURCE}/{inventory}").as_str());
    }
    selector
}

/// (assignment = expected state, priority, inventories; empty = all but
/// `bastion`, left unassigned like hosts not managed yet: grey on the map)
const ASSIGNMENTS: &[(&str, i32, &[&str])] = &[
    ("baseline", 0, &[]),
    ("web", 100, &["web", "api"]),
    ("db", 100, &["db"]),
    ("edge", 100, &["edge"]),
];

enum Every {
    Seconds(u64),
    Cron(&'static str, &'static str),
}

struct OrderSpec {
    id: &'static str,
    every: Every,
    assignment: &'static str,
    job: api::JobKind,
    enabled: bool,
}

fn orders() -> Vec<OrderSpec> {
    vec![
        OrderSpec {
            id: "dataset-baseline-assess",
            every: Every::Seconds(600),
            assignment: "baseline",
            job: api::JobKind::Assess,
            enabled: true,
        },
        OrderSpec {
            id: "dataset-web-assess",
            every: Every::Seconds(300),
            assignment: "web",
            job: api::JobKind::Assess,
            enabled: true,
        },
        OrderSpec {
            id: "dataset-db-assess",
            every: Every::Cron("*/15 * * * *", "UTC"),
            assignment: "db",
            job: api::JobKind::Assess,
            enabled: true,
        },
        OrderSpec {
            id: "dataset-edge-reach",
            every: Every::Cron("0 3 * * *", "Europe/Paris"),
            assignment: "edge",
            job: api::JobKind::Reach,
            enabled: true,
        },
        OrderSpec {
            id: "dataset-baseline-reach",
            every: Every::Seconds(3600),
            assignment: "baseline",
            job: api::JobKind::Reach,
            enabled: false,
        },
    ]
}

struct Args {
    url: String,
    hosts: u32,
    run: bool,
}

fn args() -> anyhow::Result<Args> {
    let mut parsed = Args {
        url: "http://127.0.0.1:8080".into(),
        hosts: 3000,
        run: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hosts" => parsed.hosts = args.next().unwrap_or_default().parse()?,
            "--url" => parsed.url = args.next().unwrap_or_default(),
            "--no-run" => parsed.run = false,
            other => anyhow::bail!("unknown argument {other} (--hosts N, --url URL, --no-run)"),
        }
    }
    anyhow::ensure!(parsed.hosts >= 1, "--hosts must be at least 1");
    Ok(parsed)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = args()?;
    let password = std::fs::read_to_string("dev/secrets/bootstrap-admin")?;
    let mut client = RegencyClient::connect(args.url.clone()).await?;
    let mut login = api::LoginRequest::new();
    login.set_username("admin");
    login.set_password(password.trim());
    let token = client.login(login).await?.into_inner().token().to_string();

    let hosts = hosts(args.hosts);
    std::fs::create_dir_all(format!("{ROOT}/locks"))?;
    for host in hosts.iter().filter(|h| !h.unreachable) {
        prepare_files(host)?;
    }
    println!(
        "{} hosts, files written under {ROOT}/hosts (compliant and drifting)",
        hosts.len()
    );

    let mut items = vec![CatalogBody::Source(SourceDef {
        source_id: SOURCE.into(),
        kind: SourceKind::Api,
        repo: None,
        branch: None,
        path: None,
        poll_interval_sec: 300,
        credentials: None,
    })];
    for (name, _, pool, _) in INVENTORIES {
        items.push(CatalogBody::Inventory(InventoryDef {
            source_id: SOURCE.into(),
            name: (*name).into(),
            default_connection_method: None,
            global_vars: None,
            pool: pool.map(str::to_string),
        }));
    }
    for (name, yaml) in [
        ("baseline", BASELINE),
        ("web", WEB),
        ("db", DB),
        ("edge", EDGE),
    ] {
        items.push(CatalogBody::ExpectedState(ExpectedStateDef {
            source_id: SOURCE.into(),
            name: name.into(),
            state: ExpectedState::from_raw_yaml(yaml)
                .map_err(|e| anyhow::anyhow!("expected state {name}: {e:?}"))?,
        }));
    }
    let all: Vec<&str> = INVENTORIES
        .iter()
        .map(|i| i.0)
        .filter(|i| *i != "bastion")
        .collect();
    for (name, priority, inventories) in ASSIGNMENTS {
        let inventories = if inventories.is_empty() {
            &all[..]
        } else {
            inventories
        };
        items.push(CatalogBody::Assignment(AssignmentDef {
            source_id: SOURCE.into(),
            name: (*name).into(),
            selector: regencylib::selector::HostSelector {
                inventories: inventories
                    .iter()
                    .map(|i| format!("{SOURCE}/{i}"))
                    .collect(),
                ..Default::default()
            },
            expected_state: format!("{SOURCE}/{name}"),
            priority: *priority,
        }));
    }
    for host in &hosts {
        items.push(CatalogBody::Hostdef(HostDef {
            source_id: SOURCE.into(),
            host_id: host.id.clone(),
            builder: builder(host),
            inventories: host.inventories.clone(),
            labels: host.labels.clone(),
            pool: None,
        }));
    }
    sync_catalog(&mut client, &token, items).await?;
    sync_orders(&mut client, &token).await?;

    if args.run {
        // first assessment of everything (each host's effective state, D23),
        // so the map is not all "unknown"
        let stamp = chrono::Utc::now().timestamp();
        let mut run = api::RunNowRequest::new();
        run.set_command_id(format!("dataset-assess-{stamp}").as_str());
        run.set_selector(selector(&all));
        run.set_job(api::JobKind::Assess);
        client.run_now(authed(run, &token)).await?;
        println!("first Assess sent (every host, its effective expected state)");
    }
    Ok(())
}

/// Upsert the items whose JSON differs from the catalog, delete the
/// source's hosts that are no longer generated.
async fn sync_catalog(
    client: &mut RegencyClient<Channel>,
    token: &str,
    items: Vec<CatalogBody>,
) -> anyhow::Result<()> {
    let current = catalog(client, token).await?;
    let source_exists = current.contains_key(&format!("source:{SOURCE}"));
    let wanted: BTreeSet<String> = items.iter().map(doc_id).collect();

    let mut upserts = 0;
    let mut expected = BTreeMap::new();
    for body in items {
        let id = doc_id(&body);
        let item = CatalogItem {
            body,
            revision: VERSION.into(),
        };
        let json = serde_json::to_value(&item)?;
        let same = current
            .get(&id)
            .is_some_and(|stored| same_item(&json, stored));
        expected.insert(id.clone(), json.clone());
        if same {
            continue;
        }
        let mut request = api::UpsertCatalogItemRequest::new();
        request.set_command_id(format!("dataset:{VERSION}:{id}:{}", fingerprint(&json)).as_str());
        request.set_item_json(serde_json::to_string(&item)?.as_str());
        client.upsert_catalog_item(authed(request, token)).await?;
        upserts += 1;
        if !source_exists && id.starts_with("source:") {
            // items of a source are refused until the source exists
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    let mut deletes = 0;
    for id in current.keys().filter(|id| !wanted.contains(*id)) {
        let mut request = api::DeleteCatalogItemRequest::new();
        request.set_command_id(format!("dataset:{VERSION}:delete:{id}").as_str());
        request.set_doc_id(id.as_str());
        request.set_source_id(SOURCE);
        client.delete_catalog_item(authed(request, token)).await?;
        deletes += 1;
    }
    println!("catalog: {upserts} items upserted, {deletes} deleted, the rest unchanged");
    if upserts + deletes == 0 {
        return Ok(());
    }
    // commands are applied asynchronously (backend -> RabbitMQ -> dbwriter):
    // wait for the catalog before running anything on it
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        let current = catalog(client, token).await?;
        let pending = expected
            .iter()
            .filter(|(id, json)| !current.get(*id).is_some_and(|s| same_item(json, s)))
            .count()
            + current
                .keys()
                .filter(|id| !expected.contains_key(*id))
                .count();
        if pending == 0 {
            println!("catalog written by dbwriter");
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "{pending} catalog items still not written after 10 min (see dbwriter's logs)"
        );
        println!("waiting for dbwriter: {pending} catalog items to go");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The source's catalog items, by doc id.
async fn catalog(
    client: &mut RegencyClient<Channel>,
    token: &str,
) -> anyhow::Result<BTreeMap<String, serde_json::Value>> {
    let mut request = api::ListCatalogRequest::new();
    request.set_source_id(SOURCE);
    Ok(client
        .list_catalog(authed(request, token))
        .await?
        .into_inner()
        .items()
        .iter()
        .filter_map(|entry| {
            let item = serde_json::from_str(&entry.item_json().to_string()).ok()?;
            Some((entry.doc_id().to_string(), item))
        })
        .collect())
}

/// Same content, ignoring the fields dbwriter adds to stored items.
fn same_item(wanted: &serde_json::Value, stored: &serde_json::Value) -> bool {
    wanted
        .as_object()
        .is_some_and(|o| o.iter().all(|(k, v)| stored.get(k) == Some(v)))
}

/// Content hash, so a changed item gets a new command id.
fn fingerprint(json: &serde_json::Value) -> String {
    format!("{:016x}", fnv(&json.to_string()))
}

async fn sync_orders(client: &mut RegencyClient<Channel>, token: &str) -> anyhow::Result<()> {
    for spec in orders() {
        let mut order = api::Order::new();
        order.set_order_id(spec.id);
        match spec.every {
            Every::Seconds(seconds) => order.set_every_seconds(seconds),
            Every::Cron(expression, timezone) => {
                let mut cron = api::CronSchedule::new();
                cron.set_expression(expression);
                cron.set_timezone(timezone);
                order.set_cron(cron);
            }
        }
        order.set_assignment(format!("{SOURCE}/{}", spec.assignment).as_str());
        order.set_job(spec.job);
        order.set_enabled(spec.enabled);
        let mut request = api::UpsertOrderRequest::new();
        // same id on every run: dbwriter's write is idempotent
        request.set_command_id(format!("dataset:{VERSION}:order:{}", spec.id).as_str());
        request.set_order(order);
        client.upsert_order(authed(request, token)).await?;
    }
    // orders of older dataset versions
    for id in ["dataset-staging-reach"] {
        let mut request = api::DeleteOrderRequest::new();
        request.set_command_id(format!("dataset:{VERSION}:order-delete:{id}").as_str());
        request.set_order_id(id);
        client.delete_order(authed(request, token)).await?;
    }
    println!("{} recurring orders defined", orders().len());
    Ok(())
}
