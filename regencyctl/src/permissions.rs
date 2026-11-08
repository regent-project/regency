//! RabbitMQ permissions per service (D4, D7, D10, D11, D13 tables), as the
//! (configure, write, read) regular expressions of the management API.
//!
//! Every service declares the exchanges it uses (configure on them);
//! binding a queue needs write on the queue and read on the exchange;
//! publishing needs write on the exchange; consuming needs read on the queue.

const EXCHANGES: &str = r"regency\.(tasks|outcomes|lifecycle|commands|dead|status)";
/// Every service publishes its instance status (D21).
const STATUS: &str = r"regency\.status";
const DEAD: &str = r"regency\.dead(\.parked)?";

pub struct Permissions {
    pub configure: String,
    pub write: String,
    pub read: String,
}

fn any(parts: &[&str]) -> String {
    if parts.is_empty() {
        "^$".to_string()
    } else {
        format!("^({})$", parts.join("|"))
    }
}

/// `None` for an unknown service.
pub fn for_service(service: &str) -> Option<Permissions> {
    let p = |c: &[&str], w: &[&str], r: &[&str]| Permissions {
        configure: any(c),
        write: any(w),
        read: any(r),
    };
    Some(match service {
        "scheduler" => p(
            &[
                EXCHANGES,
                r"regency\.tasks\..+",
                r"regency\.commands\.scheduler",
                DEAD,
                r"scheduler\..+",
            ],
            &[
                STATUS,
                r"regency\.(tasks|lifecycle)",
                r"regency\.tasks\..+",
                r"regency\.commands\.scheduler",
                DEAD,
                r"scheduler\..+",
            ],
            &[
                r"regency\.(tasks|commands|dead)",
                r"regency\.commands\.scheduler",
                r"scheduler\..+",
            ],
        ),
        "runner" => p(
            &[EXCHANGES, r"regency\.tasks\..+", DEAD, r"runner\..+"],
            &[
                STATUS,
                r"regency\.(outcomes|lifecycle)",
                r"regency\.tasks\..+",
                DEAD,
                r"runner\..+",
            ],
            &[
                r"regency\.(tasks|dead)",
                r"regency\.tasks\..+",
                r"regency\.control",
                r"runner\..+",
            ],
        ),
        "endpoint" => p(
            &[EXCHANGES],
            &[STATUS, r"regency\.(outcomes|commands)"],
            &[],
        ),
        "dbwriter" => p(
            &[
                EXCHANGES,
                r"regency\.(outcomes|lifecycle|commands|status)\.dbwriter",
                DEAD,
            ],
            &[
                STATUS,
                r"regency\.(outcomes|lifecycle|commands|status)\.dbwriter",
                DEAD,
            ],
            &[
                EXCHANGES,
                r"regency\.(outcomes|lifecycle|commands|status)\.dbwriter",
                r"regency\.dead",
            ],
        ),
        "backend" => p(
            &[EXCHANGES],
            &[
                STATUS,
                r"regency\.(commands|lifecycle)",
                r"regency\.control",
            ],
            &[],
        ),
        "sourcesync" => p(
            &[EXCHANGES, r"amq\.gen-.+", r"sourcesync\..+"],
            &[
                STATUS,
                r"regency\.commands",
                r"amq\.gen-.+",
                r"sourcesync\..+",
            ],
            &[r"regency\.commands", r"amq\.gen-.+", r"sourcesync\..+"],
        ),
        _ => return None,
    })
}

/// CouchDB role of a service (`regencylib::databases`).
pub fn couch_role(service: &str) -> Option<&'static str> {
    use regencylib::databases::*;
    Some(match service {
        "dbwriter" => ROLE_DBWRITER,
        "backend" => ROLE_BACKEND,
        "scheduler" => ROLE_SCHEDULER,
        "endpoint" => ROLE_ENDPOINT,
        "sourcesync" => ROLE_SOURCESYNC,
        "runner" => ROLE_RUNNER,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allows(pattern: &str, name: &str) -> bool {
        regex::Regex::new(pattern).unwrap().is_match(name)
    }

    #[test]
    fn least_privilege() {
        for service in [
            "scheduler",
            "runner",
            "endpoint",
            "dbwriter",
            "backend",
            "sourcesync",
        ] {
            assert!(for_service(service).is_some(), "{service}");
        }
        assert!(for_service("nope").is_none());
        let endpoint = for_service("endpoint").unwrap();
        assert!(allows(&endpoint.write, "regency.outcomes"));
        assert!(
            !allows(&endpoint.write, "regency.tasks"),
            "a compromised endpoint cannot publish tasks"
        );
        let runner = for_service("runner").unwrap();
        assert!(allows(&runner.read, "regency.tasks.default.assess"));
        assert!(allows(&runner.read, "regency.control"));
        assert!(allows(&runner.configure, "runner.lock.default.256.part-3"));
        assert!(!allows(&runner.write, "regency.tasks"));
        let scheduler = for_service("scheduler").unwrap();
        assert!(allows(&scheduler.write, "regency.tasks"));
        assert!(allows(&scheduler.configure, "scheduler.lock.256.shard-0"));
        assert!(!allows(&scheduler.write, "regency.outcomes"));
        let backend = for_service("backend").unwrap();
        assert!(allows(&backend.write, "regency.control"));
        for service in [
            "scheduler",
            "runner",
            "endpoint",
            "dbwriter",
            "backend",
            "sourcesync",
        ] {
            let permissions = for_service(service).unwrap();
            assert!(allows(&permissions.write, "regency.status"), "{service}");
            assert!(
                allows(&permissions.configure, "regency.status"),
                "{service}"
            );
        }
        let dbwriter = for_service("dbwriter").unwrap();
        assert!(allows(&dbwriter.read, "regency.status.dbwriter"));
        assert!(!allows(&backend.write, "regency.tasks"));
    }
}
