//! Representative development dataset for a running dev stack (dev/README.md;
//! `dev/run_locally` runs it): API source `dataset` with thousands of hosts in
//! labels, a few expected states and assignments selecting hosts by labels
//! (D29: `baseline` for every host but the unassigned `bastion` ones, plus
//! `web`/`db`/`edge` where their labels match, and `ssh-managed` for the
//! passive hosts only, runners' SSH settings, never the regops agents) and
//! recurring orders, then a first Assess of everything so the map fills at
//! once.
//!
//!   cargo run -p backend --example dataset -- [--hosts N] [--url URL] [--no-run]
//!       [--password-file PATH] [--single-pool]
//!       [--ssh ADDR:PORT --ssh-user USER --ssh-key PATH [--host-key SHA256:…]...]
//!       [--agents N --endpoint URL --ca-file PATH [--server-name NAME]]
//!
//! Every host is this machine with its own directory
//! `/tmp/regency-dev/hosts/<host>/` (the files its expected states check) and
//! its own lock (`/tmp/regency-dev/locks/<host>`), so hosts are independent
//! for D4's host lock. This machine is reached as Localhost (regency runs
//! here, `dev/run_locally`) or, with `--ssh`, over SSH at ADDR:PORT as USER
//! with the private key at PATH **on the runners' side** (a file secret
//! reference resolved by the runner, D9), its host key checked against the
//! `--host-key` fingerprints (regency runs elsewhere, `dev/run_homelab`).
//! `--single-pool` puts every host in the `default` runner pool (a single
//! runner, e.g. the all-in-one). `--password-file`: the bootstrap admin's
//! password (default `dev/secrets/bootstrap-admin`).
//!
//! `--agents N` adds N **proactive** hosts (`regops-<region>-NNNN`), each
//! meant for its own local regops agent reporting to the endpoint at URL
//! (D5, built-in CA enrollment, D13): their files, an enrollment token
//! (issued here, only while the agent has no certificate), the expected state
//! of each one in the Git repository `/tmp/regency-dev/agents-repo/states/`
//! (committed by `dev/run_homelab`) and its regops configuration
//! `/tmp/regency-dev/agents/<host>/config.toml`, the endpoint's CA at PATH.
//! They are not in the catalog: until regops can take its state from regency
//! (D23), they show as unassigned hosts, with their outcomes and live logs. Each run rewrites those files with a fixed mix of
//! compliant hosts and drift (on about 4 % of the hosts, `etc/nginx` is
//! read-only with `server_tokens` missing: a Reach cannot fix "hide nginx
//! version" there, the web hosts among them stay not compliant on that one
//! attribute); `legacy` hosts are unreachable SSH hosts
//! (error outcomes); `edge` hosts belong to the `edge` runner pool.
//! The catalog is only written when it differs (deterministic command ids);
//! hosts beyond `--hosts` are removed from the catalog.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use regencylib::api::regency_client::RegencyClient;
use regencylib::labels::LabelSelector;
use regencylib::model::*;
use regencylib::proto as api;
use regent_sdk::ExpectedState;
use regent_sdk::hosts::handlers::ssh2::{Ssh2Auth, Ssh2HostKey};
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
    labels: BTreeMap<String, String>,
    /// Runner pool of its group (`edge`), unless `--single-pool`.
    pool: Option<&'static str>,
    unreachable: bool,
}

fn hosts(total: u32) -> Vec<Host> {
    let shares: u32 = INVENTORIES.iter().map(|i| i.1).sum();
    let mut list = Vec::new();
    for (inventory, share, pool, role) in INVENTORIES {
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
                ("kind".to_string(), inventory.to_string()),
                ("env".to_string(), env.to_string()),
                ("region".to_string(), region.to_string()),
                ("role".to_string(), role.to_string()),
            ]
            .into();
            if !matches!(*inventory, "bastion" | "monitoring" | "legacy") {
                let team = TEAMS[roll(&id, "team") as usize % TEAMS.len()];
                labels.insert("team".into(), team.into());
            }
            // labels putting a few hosts in two assignments' groups (dashed
            // links on the map)
            if *inventory == "k8s" || (*inventory == "worker" && n % 5 == 0) {
                labels.insert("platform".into(), "k8s".into());
            }
            if *inventory == "db" || (*inventory == "cache" && n % 4 == 0) {
                labels.insert("storage".into(), "db".into());
            }
            list.push(Host {
                id,
                labels,
                pool: *pool,
                unreachable: *inventory == "legacy",
            });
        }
    }
    list
}

/// How the runners reach this machine (`--ssh`), Localhost otherwise.
struct SshTarget {
    endpoint: String,
    user: String,
    key: String,
    host_keys: Vec<String>,
}

fn builder(host: &Host, ssh: Option<&SshTarget>) -> ManagedHostBuilder {
    let (endpoint, method) = if host.unreachable {
        // nothing listens on port 1: an error outcome (connection refused)
        let key = SecretReference::from("dev/secrets/legacy-ssh-key", None);
        (
            "127.0.0.1:1",
            ConnectionMethod::Ssh2(Ssh2Auth::key("deploy", key)),
        )
    } else if let Some(ssh) = ssh {
        let mut auth = Ssh2Auth::key(&ssh.user, SecretReference::from(&ssh.key, None));
        if !ssh.host_keys.is_empty() {
            auth = auth.with_host_key(Ssh2HostKey::Fingerprints(ssh.host_keys.clone()));
        }
        (ssh.endpoint.as_str(), ConnectionMethod::Ssh2(auth))
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
    use std::os::unix::fs::PermissionsExt;
    let root = Path::new(ROOT).join("hosts").join(&host.id);
    for dir in ["etc/ssh", "etc/nginx", "etc/postgresql"] {
        std::fs::create_dir_all(root.join(dir))?;
    }
    // writable again, should an earlier run have locked it
    let nginx_dir = root.join("etc/nginx");
    std::fs::set_permissions(&nginx_dir, std::fs::Permissions::from_mode(0o755))?;
    if let Ok(meta) = std::fs::metadata(nginx_dir.join("nginx.conf")) {
        let mut mode = meta.permissions();
        mode.set_mode(0o644);
        std::fs::set_permissions(nginx_dir.join("nginx.conf"), mode)?;
    }
    let drift = |what: &str, percent: u64| roll(&host.id, what) < percent;
    // a host a Reach cannot fix: its nginx configuration is read-only
    let locked = drift("nginx-locked", 4);
    let mut motd = String::from("Welcome\n");
    if !drift("motd", 12) {
        motd.push_str("Managed by regency: unauthorized access is prohibited\n");
    }
    let mut sshd = String::from("Port 22\nPasswordAuthentication no\n");
    if drift("sshd", 6) {
        sshd.push_str("PermitRootLogin yes\n");
    }
    if !drift("sshd-alive", 10) {
        sshd.push_str("ClientAliveInterval 30\n");
    }
    if !drift("sshd-startups", 7) {
        sshd.push_str("MaxStartups 100:30:200\n");
    }
    let swappiness = if drift("sysctl", 8) { 60 } else { 10 };
    let mut nginx = String::from("worker_processes auto;\n");
    if !locked && !drift("nginx", 15) {
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
    if locked {
        std::fs::set_permissions(
            nginx_dir.join("nginx.conf"),
            std::fs::Permissions::from_mode(0o444),
        )?;
        std::fs::set_permissions(&nginx_dir, std::fs::Permissions::from_mode(0o555))?;
    }
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

/// What runners need from the sshd of the hosts they reach (passive hosts
/// only: regops agents are never reached over SSH).
const SSH_MANAGED: &str = r#"---
Attributes:
  - Name: SSH keep-alive
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/ssh/sshd_config"
      State: !Present
        Create: true
      Line: !Raw "ClientAliveInterval 30"
  - Name: SSH connection bursts
    Privilege: !None
    Timeout: 20s
    Detail: !LineInFile
      FilePath: "/tmp/regency-dev/hosts/{{ host }}/etc/ssh/sshd_config"
      State: !Present
        Create: true
      Line: !Raw "MaxStartups 100:30:200"
"#;

fn selector(assignments: &[&str]) -> api::HostSelector {
    let mut selector = api::HostSelector::new();
    for assignment in assignments {
        selector
            .assignments_mut()
            .push(format!("{SOURCE}/{assignment}").as_str());
    }
    selector
}

/// (assignment, selector, priority, expected states base first): like a
/// Salt top file, a host gets every assignment matching its labels (D29);
/// bastion is left unassigned like hosts not managed yet (grey on the map).
const ASSIGNMENTS: &[(&str, &str, i32, &[&str])] = &[
    ("baseline", r#"{kind!="bastion"}"#, 0, &["baseline"]),
    ("web", r#"{kind=~"web|api"}"#, 10, &["web"]),
    ("db", r#"{storage="db"}"#, 10, &["db"]),
    ("edge", r#"{kind="edge"}"#, 10, &["edge"]),
    // passive hosts only: a reporting agent's hosts carry `source="regops"`
    (
        "ssh-managed",
        r#"{kind!="bastion", source!="regops"}"#,
        5,
        &["ssh-managed"],
    ),
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
    password_file: String,
    single_pool: bool,
    ssh: Option<SshTarget>,
    agents: Option<Agents>,
}

/// Proactive hosts, one regops agent each (`--agents`).
struct Agents {
    count: u32,
    endpoint: String,
    ca_file: String,
    server_name: Option<String>,
}

fn args() -> anyhow::Result<Args> {
    let mut parsed = Args {
        url: "http://127.0.0.1:8080".into(),
        hosts: 3000,
        run: true,
        password_file: "dev/secrets/bootstrap-admin".into(),
        single_pool: false,
        ssh: None,
        agents: None,
    };
    let (mut ssh, mut ssh_user, mut ssh_key, mut host_keys) = (None, None, None, Vec::new());
    let (mut agents, mut endpoint, mut ca_file, mut server_name) = (None, None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--hosts" => parsed.hosts = value()?.parse()?,
            "--url" => parsed.url = value()?,
            "--no-run" => parsed.run = false,
            "--password-file" => parsed.password_file = value()?,
            "--single-pool" => parsed.single_pool = true,
            "--ssh" => ssh = Some(value()?),
            "--ssh-user" => ssh_user = Some(value()?),
            "--ssh-key" => ssh_key = Some(value()?),
            "--host-key" => host_keys.push(value()?),
            "--agents" => agents = Some(value()?.parse::<u32>()?),
            "--endpoint" => endpoint = Some(value()?),
            "--ca-file" => ca_file = Some(value()?),
            "--server-name" => server_name = Some(value()?),
            other => anyhow::bail!(
                "unknown argument {other} (--hosts N, --url URL, --no-run, --password-file PATH, \
                 --single-pool, --ssh ADDR:PORT, --ssh-user USER, --ssh-key PATH, --host-key FP, \
                 --agents N, --endpoint URL, --ca-file PATH, --server-name NAME)"
            ),
        }
    }
    anyhow::ensure!(parsed.hosts >= 1, "--hosts must be at least 1");
    match (ssh, ssh_user, ssh_key) {
        (Some(endpoint), Some(user), Some(key)) => {
            parsed.ssh = Some(SshTarget {
                endpoint,
                user,
                key,
                host_keys,
            })
        }
        (None, None, None) => {
            anyhow::ensure!(host_keys.is_empty(), "--host-key needs --ssh");
        }
        _ => anyhow::bail!("--ssh, --ssh-user and --ssh-key go together"),
    }
    match (agents, endpoint, ca_file) {
        (Some(0), ..) | (None, None, None) => {}
        (Some(count), Some(endpoint), Some(ca_file)) => {
            parsed.agents = Some(Agents {
                count,
                endpoint,
                ca_file,
                server_name,
            })
        }
        _ => anyhow::bail!("--agents needs --endpoint and --ca-file"),
    }
    Ok(parsed)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = args()?;
    let password = std::fs::read_to_string(&args.password_file)
        .map_err(|e| anyhow::anyhow!("{}: {e}", args.password_file))?;
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
    for (name, yaml) in [
        ("baseline", BASELINE),
        ("web", WEB),
        ("db", DB),
        ("edge", EDGE),
        ("ssh-managed", SSH_MANAGED),
    ] {
        items.push(CatalogBody::ExpectedState(ExpectedStateDef {
            source_id: SOURCE.into(),
            name: name.into(),
            state: ExpectedState::from_raw_yaml(yaml)
                .map_err(|e| anyhow::anyhow!("expected state {name}: {e:?}"))?,
        }));
    }
    for (name, selector, priority, states) in ASSIGNMENTS {
        items.push(CatalogBody::Assignment(AssignmentDef {
            source_id: SOURCE.into(),
            name: (*name).into(),
            selector: LabelSelector::parse(selector).map_err(anyhow::Error::msg)?,
            priority: *priority,
            expected_states: states.iter().map(|s| format!("{SOURCE}/{s}")).collect(),
        }));
    }
    for host in &hosts {
        items.push(CatalogBody::Hostdef(HostDef {
            source_id: SOURCE.into(),
            host_id: host.id.clone(),
            builder: builder(host, args.ssh.as_ref()),
            labels: host.labels.clone(),
            pool: host.pool.filter(|_| !args.single_pool).map(str::to_string),
        }));
    }
    sync_catalog(&mut client, &token, items).await?;
    if let Some(agents) = &args.agents {
        prepare_agents(&mut client, &token, agents).await?;
    }
    sync_orders(&mut client, &token).await?;

    if args.run {
        // first assessment of everything (each host's effective state, D23),
        // so the map is not all "unknown"
        let stamp = chrono::Utc::now().timestamp();
        let mut run = api::RunNowRequest::new();
        run.set_command_id(format!("dataset-assess-{stamp}").as_str());
        run.set_selector(selector(&["baseline"]));
        run.set_job(api::JobKind::Assess);
        client.run_now(authed(run, &token)).await?;
        println!("first Assess sent (every host, its effective expected state)");
    }
    Ok(())
}

/// Proactive hosts: one regops agent each, set up under `{ROOT}/agents/`.
async fn prepare_agents(
    client: &mut RegencyClient<Channel>,
    token: &str,
    agents: &Agents,
) -> anyhow::Result<()> {
    let repo = Path::new(ROOT).join("agents-repo");
    std::fs::create_dir_all(repo.join("states"))?;
    let mut ids = BTreeSet::new();
    let mut issued = 0;
    for n in 1..=agents.count {
        let region = REGIONS[(n as usize) % REGIONS.len()];
        let short: String = region.split('-').map(|p| &p[..1]).collect();
        let id = format!("regops-{short}-{n:04}");
        let host = Host {
            id: id.clone(),
            labels: BTreeMap::new(),
            pool: None,
            unreachable: false,
        };
        prepare_files(&host)?;
        // a third of them also check the web layer, a third the database one
        let layers: &[&str] = match n % 3 {
            0 => &[BASELINE, WEB],
            1 => &[BASELINE, DB],
            _ => &[BASELINE],
        };
        let mut state = String::from("---\nAttributes:\n");
        for layer in layers {
            let body = layer.split_once("Attributes:\n").map_or("", |(_, b)| b);
            state.push_str(body);
        }
        // regops reads it as is: no host variables
        let state = state.replace("{{ host }}", &id);
        ExpectedState::from_raw_yaml(&state)
            .map_err(|e| anyhow::anyhow!("expected state of {id}: {e:?}"))?;
        write_if_changed(&repo.join("states").join(format!("{id}.yaml")), &state)?;

        let dir = Path::new(ROOT).join("agents").join(&id);
        std::fs::create_dir_all(dir.join("spool"))?;
        // one agent in four remediates (regops Enforce = Reach)
        let mode = if n % 4 == 0 { "Enforce" } else { "Assess" };
        let server_name = agents
            .server_name
            .as_ref()
            .map(|name| format!("server_name = {name:?}\n"))
            .unwrap_or_default();
        let config = format!(
            r#"# regops agent of the dev dataset (backend/examples/dataset.rs, dev/run_homelab)
Reporting = "Regency"

[Git]
repo = "{repo}"
branch = "main"
local_path = "{dir}/repo"
expected_state_path = "states/{id}.yaml"

[SystemIntegration]
log_level = "Info"
log_format = "Raw"

[Behavior]
mode = "{mode}"
interval_sec = 60
auto_update = false

[HostLock]
path = "{ROOT}/locks/{id}"

[Regency]
endpoint = "{endpoint}"
host_id = "{id}"
{server_name}ca_cert_path = "{ca}"
tls_mode = "BuiltInCa"
cert_path = "{dir}/client.pem"
key_path = "{dir}/client.key"
enrollment_token_path = "{dir}/enrollment.token"
spool_dir = "{dir}/spool"
"#,
            repo = repo.display(),
            dir = dir.display(),
            endpoint = agents.endpoint,
            ca = agents.ca_file,
        );
        write_if_changed(&dir.join("config.toml"), &config)?;
        // the token only serves the first enrollment; then regops renews
        // its certificate itself
        if !dir.join("client.pem").exists() {
            let mut request = api::IssueEnrollmentTokenRequest::new();
            request.set_host_id(id.as_str());
            request.set_ttl_sec(86_400);
            let response = client
                .issue_enrollment_token(authed(request, token))
                .await?
                .into_inner();
            write_secret(&dir.join("enrollment.token"), &response.token().to_string())?;
            issued += 1;
        }
        ids.insert(id);
    }
    // agents of a larger previous run: their state files go (the agents
    // themselves are only started by dev/run_homelab, from the configs)
    for entry in std::fs::read_dir(repo.join("states"))? {
        let path = entry?.path();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if !ids.contains(stem) {
            std::fs::remove_file(&path)?;
        }
    }
    for entry in std::fs::read_dir(Path::new(ROOT).join("agents"))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if path.is_dir() && !ids.contains(name) {
            std::fs::remove_dir_all(&path)?;
        }
    }
    println!(
        "{} regops agents prepared under {ROOT}/agents ({issued} enrollment tokens issued)",
        agents.count
    );
    Ok(())
}

fn write_if_changed(path: &Path, content: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    std::fs::write(path, content)
}

/// Written readable by its owner only.
fn write_secret(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())
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
