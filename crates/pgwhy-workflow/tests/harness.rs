//! Testcontainers fixture (pattern copied from `crates/pgwhy-core/tests/harness.rs`, without
//! the TLS parts): `pg(major)` / `pg_with_args(major, args)` start a throwaway Postgres, or
//! return `None` with a NOTICE when Docker is unavailable (or `PGWHY_SKIP_DOCKER=1`).

#![allow(dead_code)]

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

pub const PG_USER: &str = "pgwhy";
pub const PG_PASSWORD: &str = "pgwhy-test";
pub const PG_DB: &str = "pgwhy_test";

pub struct PgFixture {
    pub container: ContainerAsync<GenericImage>,
    pub host: String,
    pub port: u16,
    pub major: u32,
}

impl PgFixture {
    pub fn conninfo(&self, db: &str) -> String {
        format!(
            "host={} port={} user={PG_USER} password={PG_PASSWORD} dbname={db}",
            self.host, self.port
        )
    }
}

/// Docker must be up and running Linux containers: the Windows runners have a daemon in
/// Windows-container mode, which cannot pull the Postgres images.
fn docker_available() -> bool {
    if std::env::var("PGWHY_SKIP_DOCKER").is_ok_and(|v| v == "1") {
        return false;
    }
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.OSType}}"])
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "linux")
}

pub async fn pg(major: u32) -> Option<PgFixture> {
    pg_with_args(major, &[]).await
}

/// Start Postgres with extra server arguments (e.g. `-c max_prepared_transactions=10`).
pub async fn pg_with_args(major: u32, args: &[&str]) -> Option<PgFixture> {
    if !docker_available() {
        eprintln!("NOTICE: docker unavailable (or PGWHY_SKIP_DOCKER=1) — skipping Postgres {major} integration test");
        return None;
    }
    let mut cmd = vec!["postgres".to_string()];
    cmd.extend(args.iter().map(|a| a.to_string()));
    let container = GenericImage::new("postgres", &format!("{major}-alpine"))
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        // the entrypoint logs "ready" once for the init server and once for the real one
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", PG_USER)
        .with_env_var("POSTGRES_PASSWORD", PG_PASSWORD)
        .with_env_var("POSTGRES_DB", PG_DB)
        .with_cmd(cmd)
        .start()
        .await
        .expect("start postgres container");
    let host = container
        .get_host()
        .await
        .expect("container host")
        .to_string();
    let port = container
        .get_host_port_ipv4(5432.tcp())
        .await
        .expect("container port");
    Some(PgFixture {
        container,
        host,
        port,
        major,
    })
}
