//! The default configuration files shipped in packages
//! (`packaging/config/*.toml`) parse into each service's configuration type.

use regencylib::central::deserialize;

fn table(service: &str) -> toml::Table {
    table_in("config", service)
}

fn table_in(dir: &str, service: &str) -> toml::Table {
    let path = format!(
        "{}/../packaging/{dir}/{service}.toml",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .parse()
        .unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn packaged_configurations_parse() {
    use regencylib::services::*;
    deserialize::<scheduler::Config>(&table("scheduler")).unwrap();
    deserialize::<runner::Config>(&table("runner")).unwrap();
    deserialize::<dbwriter::Config>(&table("dbwriter")).unwrap();
    deserialize::<backend::Config>(&table("backend")).unwrap();
    deserialize::<endpoint::Config>(&table("endpoint")).unwrap();
    deserialize::<sourcesync::Config>(&table("sourcesync")).unwrap();
}

#[test]
fn packaged_configurations_are_hardened() {
    use regencylib::security::Level;
    for service in [
        "scheduler",
        "runner",
        "dbwriter",
        "backend",
        "endpoint",
        "sourcesync",
    ] {
        let common: regencylib::config::CommonConfig = deserialize(&table(service)).unwrap();
        assert_eq!(common.security.internal, Level::Hardened, "{service}");
        assert_eq!(common.security.external, Level::Hardened, "{service}");
        // level 3 internal requirements are all present in the template
        common
            .check_internal(true, true)
            .unwrap_or_else(|e| panic!("{service}: {e}"));
    }
    let backend: regencylib::services::backend::Config = deserialize(&table("backend")).unwrap();
    for check in regencylib::services::backend::external_checks(&backend) {
        check.unwrap();
    }
}

#[test]
fn windows_configurations_parse_and_differ_only_by_paths() {
    use regencylib::services::*;
    let windows = |s| table_in("windows", s);
    deserialize::<scheduler::Config>(&windows("scheduler")).unwrap();
    deserialize::<runner::Config>(&windows("runner")).unwrap();
    deserialize::<dbwriter::Config>(&windows("dbwriter")).unwrap();
    deserialize::<backend::Config>(&windows("backend")).unwrap();
    deserialize::<endpoint::Config>(&windows("endpoint")).unwrap();
    deserialize::<sourcesync::Config>(&windows("sourcesync")).unwrap();
    let runner: runner::Config = deserialize(&windows("runner")).unwrap();
    assert!(
        runner
            .common
            .rabbitmq
            .ca_file
            .unwrap()
            .starts_with(r"C:\ProgramData\regency\")
    );
}

#[test]
fn health_ports_differ_between_services() {
    let mut seen = std::collections::BTreeSet::new();
    for service in [
        "scheduler",
        "runner",
        "dbwriter",
        "backend",
        "endpoint",
        "sourcesync",
    ] {
        let common: regencylib::config::CommonConfig = deserialize(&table(service)).unwrap();
        assert!(seen.insert(common.health.listen.clone()), "{service}");
    }
}
