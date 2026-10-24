//! Shared testcontainers fixture for integration tests.
//!
//! `pg(version)` starts a throwaway Postgres container of the given major (13–18). When Docker
//! is unavailable — or `PGWHY_SKIP_DOCKER=1` — it returns `None` after printing a visible
//! notice, so integration tests skip instead of failing CI.

#![allow(dead_code)]

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

pub const PG_USER: &str = "pgwhy";
pub const PG_PASSWORD: &str = "pgwhy-test";
pub const PG_DB: &str = "pgwhy_test";

/// Supported server majors for the integration matrix.
pub const PG_MAJORS: &[u32] = &[13, 14, 15, 16, 17, 18];

pub struct PgFixture {
    pub container: ContainerAsync<GenericImage>,
    pub host: String,
    pub port: u16,
    pub major: u32,
}

impl PgFixture {
    pub fn conninfo(&self) -> String {
        format!(
            "host={} port={} user={PG_USER} password={PG_PASSWORD} dbname={PG_DB}",
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

/// Start a Postgres container, or `None` (with a notice) when Docker is unavailable.
pub async fn pg(major: u32) -> Option<PgFixture> {
    if !docker_available() {
        eprintln!("NOTICE: docker unavailable (or PGWHY_SKIP_DOCKER=1) — skipping Postgres {major} integration test");
        return None;
    }
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
