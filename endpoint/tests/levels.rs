//! D19 startup checks of the endpoint's external level (no infrastructure).

async fn run(external: u8, extra: &str) -> (Option<i32>, String) {
    let dir = std::env::temp_dir().join(format!("endpoint-levels-{:08x}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = format!(
        r#"
# no CouchDB here: no central configuration (D21)
[central]
enabled = false
[security]
internal = 0
external = {external}
[endpoint]
listen = "127.0.0.1:0"
cert = "/nonexistent/cert.pem"
key = {{ SecRef = "/nonexistent/key.pem" }}
{extra}
[log]
format = "text"
"#
    );
    let path = dir.join("endpoint.toml");
    std::fs::write(&path, config).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_regency-endpoint"))
        .args(["--config", path.to_str().unwrap()])
        .output()
        .await
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn trust_on_first_use_is_refused_from_level_2() {
    let (code, stderr) = run(2, "[pinning]\nenabled = true\ntrust_on_first_use = true\n").await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("trust_on_first_use"), "{stderr}");
}

#[tokio::test]
async fn level_3_wants_a_ca_and_no_self_signed() {
    let (code, stderr) = run(3, "[pinning]\nenabled = true\n").await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("pinning) are forbidden"), "{stderr}");
    assert!(
        stderr.contains("external CA bundle or the built-in CA"),
        "{stderr}"
    );
}

#[tokio::test]
async fn level_0_needs_no_trust_mode() {
    // passes the security gate, then fails on the missing certificate file
    let (code, stderr) = run(0, "").await;
    assert_ne!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("security levels") || !stderr.contains("refusing to start"),
        "{stderr}"
    );
}
