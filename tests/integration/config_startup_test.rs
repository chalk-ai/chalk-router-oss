//! Isolate environment-based configuration in subprocesses so parallel tests
//! cannot race with authentication settings.

use std::process::{Command, Output};

use openai_server::config::RouterConfig;

fn config_subprocess(settings: &[(&str, &str)]) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "config_startup_test::read_config_in_subprocess",
            "--nocapture",
        ])
        .env_clear()
        .env("ROUTER_CONFIG_TEST_CHILD", "1")
        .envs(settings.iter().copied())
        .output()
        .unwrap()
}

#[test]
fn read_config_in_subprocess() {
    if std::env::var("ROUTER_CONFIG_TEST_CHILD").as_deref() != Ok("1") {
        return;
    }
    let config = RouterConfig::from_env();
    println!(
        "dangerously_permit_unauthenticated={}",
        config.dangerously_permit_unauthenticated
    );
    println!("listen_addr={}", config.listen_addr);
    println!(
        "accept_cross_environment_tokens={}",
        config.accept_cross_environment_tokens
    );
    println!("default_environment={}", config.environment_id);
    println!("cluster={}", config.cluster.is_some());
    println!(
        "kube_cluster={}",
        config
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.kube_cluster_id.as_deref())
            .unwrap_or("")
    );
    println!(
        "migrate_check={}",
        openai_server::config::check_migrate_environment().is_ok()
    );
}

fn assert_config(settings: &[(&str, &str)], expected: bool) {
    let output = config_subprocess(settings);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains(&format!("dangerously_permit_unauthenticated={expected}")));
}

#[test]
fn anonymous_development_requires_explicit_opt_in() {
    assert_config(&[], false);
    for value in ["false", "FALSE", "0", "", "invalid"] {
        assert_config(
            &[("ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED", value)],
            false,
        );
    }
    for value in ["true", "TRUE", "True", "1"] {
        assert_config(
            &[("ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED", value)],
            true,
        );
    }
}

#[test]
fn removed_auth_settings_are_ignored() {
    for value in ["true", "false", "1", "0", ""] {
        assert_config(
            &[
                ("ROUTER_ALLOW_UNAUTHENTICATED", value),
                ("DEFAULT_API_KEY", "unused-bootstrap-key"),
            ],
            false,
        );
    }
}

const METADATA: [(&str, &str); 2] = [
    (
        "ROUTER_METADATA_API_ENDPOINT",
        "http://metadata.invalid:443",
    ),
    ("ROUTER_METADATA_API_TOKEN", "exchange-token"),
];

fn cluster_settings<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut settings = vec![("ROUTER_TENANCY_MODE", "cluster")];
    settings.extend(METADATA);
    settings.extend_from_slice(extra);
    settings
}

fn stdout_of(settings: &[(&str, &str)]) -> String {
    let output = config_subprocess(settings);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn environment_mode_is_the_default_and_needs_no_metadata_settings() {
    let stdout = stdout_of(&[("CHALK_ENVIRONMENT_ID", "demo")]);
    assert!(stdout.contains("cluster=false"));
    assert!(stdout.contains("default_environment=demo"));
    assert!(stdout.contains("migrate_check=true"));
    let explicit = stdout_of(&[("ROUTER_TENANCY_MODE", "environment")]);
    assert!(explicit.contains("cluster=false"));
    // The migrate Job keeps its environment tripwire in environment mode.
    assert!(stdout_of(&[]).contains("migrate_check=false"));
    assert!(
        !config_subprocess(&[("ROUTER_DATABASE_URL", "postgres://unused/config-only")])
            .status
            .success()
    );
}

#[test]
fn cluster_mode_needs_metadata_access_but_no_environment() {
    let stdout = stdout_of(&cluster_settings(&[
        ("ROUTER_DATABASE_URL", "postgres://unused/config-only"),
        ("ROUTER_KUBE_CLUSTER_ID", "kube-1"),
    ]));
    assert!(stdout.contains("cluster=true"));
    assert!(stdout.contains("default_environment=chalk-router-cluster-template"));
    assert!(stdout.contains("kube_cluster=kube-1"));
    assert!(stdout.contains("migrate_check=true"));
    let legacy = stdout_of(&cluster_settings(&[("CHALK_ENVIRONMENT_ID", "demo")]));
    assert!(legacy.contains("default_environment=demo"));
    for missing in METADATA {
        let settings: Vec<_> = cluster_settings(&[])
            .into_iter()
            .filter(|setting| *setting != missing)
            .collect();
        assert!(
            !config_subprocess(&settings).status.success(),
            "{}",
            missing.0
        );
    }
}

#[test]
fn cluster_static_admin_requires_explicit_tenant_selection() {
    let invalid = config_subprocess(&cluster_settings(&[("ROUTER_ADMIN_KEY", "test-admin")]));
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr)
        .contains("requires an explicit CHALK_ENVIRONMENT_ID"));
    let valid = stdout_of(&cluster_settings(&[
        ("ROUTER_ADMIN_KEY", "test-admin"),
        ("CHALK_ENVIRONMENT_ID", "zeta"),
    ]));
    assert!(valid.contains("default_environment=zeta"));
}

#[test]
fn tenancy_mode_fails_closed_on_invalid_configuration() {
    assert!(!config_subprocess(&[("ROUTER_TENANCY_MODE", "inventory")])
        .status
        .success());
    assert!(!config_subprocess(&cluster_settings(&[(
        "ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED",
        "true"
    )]))
    .status
    .success());
}

#[test]
fn local_listener_defaults_are_authenticated_and_loopback_only() {
    let output = stdout_of(&[]);
    assert!(output.contains("listen_addr=127.0.0.1:4001"));
    assert!(output.contains("dangerously_permit_unauthenticated=false"));
    assert!(output.contains("accept_cross_environment_tokens=false"));
    assert!(
        stdout_of(&[("ROUTER_BIND_ADDRESS", "::1"), ("PORT", "8088")])
            .contains("listen_addr=[::1]:8088")
    );
}

#[test]
fn anonymous_non_loopback_listener_requires_two_independent_opt_ins() {
    let anonymous = "ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED";
    let external = "ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED_NON_LOOPBACK";
    for host in ["0.0.0.0", "::", "192.0.2.1", "::ffff:127.0.0.1"] {
        let base = [("ROUTER_BIND_ADDRESS", host), (anonymous, "true")];
        let output = config_subprocess(&base);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(external));
        for invalid in ["false", "0", "", "invalid"] {
            let mut settings = base.to_vec();
            settings.push((external, invalid));
            assert!(!config_subprocess(&settings).status.success());
        }
        let mut settings = base.to_vec();
        settings.push((external, "true"));
        assert_config(&settings, true);
        // The network override does not itself disable authentication.
        assert_config(&[("ROUTER_BIND_ADDRESS", host), (external, "true")], false);
        assert_config(&[("ROUTER_BIND_ADDRESS", host)], false);
    }
    for host in ["127.0.0.1", "127.0.0.2", "::1"] {
        assert_config(&[("ROUTER_BIND_ADDRESS", host), (anonymous, "true")], true);
    }
    // No pair of development flags weakens cluster-mode authentication.
    assert!(!config_subprocess(&cluster_settings(&[
        ("ROUTER_BIND_ADDRESS", "0.0.0.0"),
        (anonymous, "true"),
        (external, "true"),
    ]))
    .status
    .success());
}

#[test]
fn invalid_listener_configuration_fails_closed() {
    for host in ["", "localhost", "127.0.0.1:4001", "[::1]", "invalid"] {
        assert!(!config_subprocess(&[("ROUTER_BIND_ADDRESS", host)])
            .status
            .success());
    }
    for port in ["", "-1", "65536", "invalid"] {
        assert!(!config_subprocess(&[("PORT", port)]).status.success());
    }
}
