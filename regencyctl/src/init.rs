//! `regencyctl init` (D15): idempotent setup of RabbitMQ (vhost, service
//! users, permissions, topology) and CouchDB (databases, `_security`, design
//! docs, service users, default settings, bootstrap admin). Safe to re-run;
//! re-running also rotates service passwords to the configured values.

use anyhow::{Context, bail};
use chrono::Utc;
use regencylib::config::resolve_secret;
use regencylib::couch::{Couch, CouchError};
use regencylib::model::{
    AccountKind, DB_SETTINGS, DB_USERS, SETTINGS_DOC_ID, SettingsDoc, Stored, UserDoc, jobs_db,
    month_offset, outcomes_db, user_doc_id,
};
use regencylib::topology;
use regent_sdk::secrets::SecretProvidersPool;
use serde_json::{Value, json};
use tracing::info;

use crate::config::Config;
use crate::permissions::{couch_role, for_service};

/// HTTP client trusting only the link's CA (if set), with its client
/// identity (if set), D19.
fn http_client(tls: &regencylib::config::TlsMaterial) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(ca) = &tls.ca_pem {
        builder = builder.tls_certs_only(reqwest::Certificate::from_pem_bundle(ca)?);
    }
    if let (Some(cert), Some(key)) = (&tls.cert_pem, &tls.key_pem) {
        let mut pem = cert.clone();
        pem.push(b'\n');
        pem.extend_from_slice(key.as_bytes());
        builder = builder.identity(reqwest::Identity::from_pem(&pem)?);
    }
    Ok(builder.build()?)
}

/// D19 check of regencyctl's admin links.
pub fn check(config: &Config) -> Result<(), String> {
    use regencylib::security::{Level, check_internal};
    let level = config.security.internal;
    let mut v = check_internal(level, Some(&config.rabbitmq), Some(&config.couchdb));
    if level >= Level::Encrypted {
        v.require(
            config.rabbitmq_management_url.starts_with("https://"),
            "rabbitmq_management_url must be https://",
        );
    }
    v.into_result("internal", level)
}

pub struct Init<'a> {
    pub config: &'a Config,
    pub secrets: SecretProvidersPool,
}

impl Init<'_> {
    fn username(&self, service: &str) -> String {
        self.config
            .accounts
            .get(service)
            .and_then(|a| a.username.clone())
            .unwrap_or_else(|| format!("regency-{service}"))
    }

    // ------------------------------------------------------------ RabbitMQ

    pub async fn rabbitmq(&self) -> anyhow::Result<()> {
        let config = self.config;
        let user = config
            .rabbitmq
            .username
            .clone()
            .context("rabbitmq.username (admin) is required")?;
        let password = match &config.rabbitmq.password {
            Some(reference) => resolve_secret(&self.secrets, reference).await?,
            None => bail!("rabbitmq.password (admin) is required"),
        };
        let http = http_client(&config.rabbitmq.tls(&self.secrets).await?)?;
        let base = config.rabbitmq_management_url.trim_end_matches('/');
        let vhost = url::form_urlencoded::byte_serialize(config.rabbitmq.vhost.as_bytes())
            .collect::<String>();
        let put = |path: String, body: Value| {
            let request = http
                .put(format!("{base}/api/{path}"))
                .basic_auth(&user, Some(&password))
                .json(&body);
            async move {
                let response = request.send().await?;
                if !response.status().is_success() {
                    bail!(
                        "PUT {path}: {} {}",
                        response.status(),
                        response.text().await.unwrap_or_default()
                    );
                }
                Ok(())
            }
        };
        put(
            format!("vhosts/{vhost}"),
            json!({ "description": "regency", "default_queue_type": "quorum" }),
        )
        .await?;
        info!(vhost = %config.rabbitmq.vhost, "vhost ready");
        for (service, account) in &config.accounts {
            let Some(permissions) = for_service(service) else {
                if couch_role(service).is_none() {
                    bail!("unknown service account [accounts.{service}]");
                }
                continue;
            };
            let Some(reference) = &account.rabbitmq_password else {
                continue;
            };
            let name = self.username(service);
            let secret = resolve_secret(&self.secrets, reference).await?;
            put(
                format!("users/{name}"),
                json!({ "password": secret, "tags": "" }),
            )
            .await?;
            put(
                format!("permissions/{vhost}/{name}"),
                json!({ "configure": permissions.configure, "write": permissions.write, "read": permissions.read }),
            )
            .await?;
            info!(user = %name, service, "RabbitMQ user and permissions ready");
        }

        // topology, so that no message is unroutable before services start
        let connection =
            regencylib::amqp::connect(&config.rabbitmq, &self.secrets, "regencyctl").await?;
        let channel = connection.create_channel().await?;
        topology::declare_exchanges(&channel).await?;
        topology::declare_dead(&channel).await?;
        topology::declare_outcomes_dbwriter(&channel, &config.topology).await?;
        topology::declare_lifecycle_dbwriter(&channel, &config.topology).await?;
        topology::declare_commands_dbwriter(&channel, &config.topology).await?;
        topology::declare_commands_scheduler(&channel, &config.topology).await?;
        topology::declare_control_stream(&channel, &config.topology).await?;
        for pool in &config.pools {
            topology::declare_pool(&channel, &config.topology, pool).await?;
            info!(
                pool,
                partitions = config.topology.reach_partitions,
                "pool queues ready"
            );
        }
        regencylib::amqp::close(&connection, 200, "done").await;
        info!("RabbitMQ topology ready");
        Ok(())
    }

    // ------------------------------------------------------------ CouchDB

    pub async fn couchdb(&self) -> anyhow::Result<()> {
        let config = self.config;
        let couch = Couch::from_config(&config.couchdb, &self.secrets).await?;
        couch.ping().await.context("CouchDB unreachable")?;
        for db in ["_users", "_replicator"] {
            couch.create_db(db).await?;
        }
        let now = Utc::now();
        let mut databases: Vec<String> = regencylib::databases::STATIC
            .iter()
            .map(|s| s.to_string())
            .collect();
        for offset in [0, 1] {
            let month = month_offset(now, offset);
            databases.push(jobs_db(&month));
            databases.push(outcomes_db(&month));
        }
        for db in &databases {
            let created = regencylib::databases::ensure(&couch, db).await?;
            info!(db, created, "database ready");
        }

        // service users
        let http = http_client(&config.couchdb.tls(&self.secrets).await?)?;
        let admin = config
            .couchdb
            .username
            .clone()
            .context("couchdb.username (admin) is required")?;
        let admin_password = match &config.couchdb.password {
            Some(reference) => resolve_secret(&self.secrets, reference).await?,
            None => bail!("couchdb.password (admin) is required"),
        };
        let base = config.couchdb.url.trim_end_matches('/');
        for (service, account) in &config.accounts {
            let (Some(role), Some(reference)) = (couch_role(service), &account.couchdb_password)
            else {
                continue;
            };
            let name = self.username(service);
            let secret = resolve_secret(&self.secrets, reference).await?;
            let id = format!("org.couchdb.user:{name}");
            let mut doc = json!({ "_id": id, "name": name, "type": "user", "roles": [role], "password": secret });
            if let Some(existing) = couch.get::<Value>("_users", &id).await? {
                doc["_rev"] = json!(existing.rev);
            }
            let stored: Stored<Value> = serde_json::from_value(doc)?;
            couch.put("_users", &stored).await?;
            // D8: monthly databases are created and dropped by dbwriter, which
            // needs CouchDB server admin (set on every node)
            if service == "dbwriter" {
                let membership: Value = http
                    .get(format!("{base}/_membership"))
                    .basic_auth(&admin, Some(&admin_password))
                    .send()
                    .await?
                    .json()
                    .await?;
                let nodes: Vec<String> = membership["all_nodes"]
                    .as_array()
                    .map(|n| {
                        n.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["_local".to_string()]);
                for node in nodes {
                    let response = http
                        .put(format!("{base}/_node/{node}/_config/admins/{name}"))
                        .basic_auth(&admin, Some(&admin_password))
                        .json(&secret)
                        .send()
                        .await?;
                    if !response.status().is_success() {
                        bail!("cannot make {name} admin on {node}: {}", response.status());
                    }
                }
            }
            info!(user = %name, role, "CouchDB user ready");
        }

        // default settings (create only)
        match couch
            .put(
                DB_SETTINGS,
                &Stored::new(SETTINGS_DOC_ID, SettingsDoc::with_defaults()),
            )
            .await
        {
            Ok(_) => info!("default settings created"),
            Err(CouchError::Conflict(_)) => info!("settings already present"),
            Err(e) => return Err(e.into()),
        }

        // bootstrap admin (create only, D12)
        if let Some(bootstrap) = &config.bootstrap {
            let password = resolve_secret(&self.secrets, &bootstrap.password).await?;
            let user = UserDoc {
                name: bootstrap.name.clone(),
                account: AccountKind::User,
                password_hash: Some(
                    regencylib::passwords::hash(&password).map_err(anyhow::Error::msg)?,
                ),
                tokens: Vec::new(),
                groups: Default::default(),
                disabled: false,
                bootstrap: true,
            };
            match couch
                .put(DB_USERS, &Stored::new(user_doc_id(&bootstrap.name), user))
                .await
            {
                Ok(_) => info!(user = %bootstrap.name, "bootstrap admin created"),
                Err(CouchError::Conflict(_)) => {
                    info!(user = %bootstrap.name, "bootstrap admin already present")
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}
