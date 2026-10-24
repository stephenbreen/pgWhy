//! Testcontainers fixture for pgtools integration tests (pattern copied from
//! `crates/pgwhy-core/tests/harness.rs`; pgtools must not depend on pgwhy-core).
//!
//! `pg(major)` starts a throwaway Postgres container, preferring `postgres:<major>-alpine` and
//! falling back to any locally cached `postgres:<major>*` tag (so the suite runs offline).
//! Returns `None` with a NOTICE when Docker (or a usable image) is unavailable, or when
//! `PGWHY_SKIP_DOCKER=1`.

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
    pub image: String,
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

/// `<major>-alpine` if cached locally, else another cached `<major>…` tag, else
/// `<major>-alpine` (testcontainers will try to pull it).
fn pick_tag(major: u32) -> String {
    let preferred = format!("{major}-alpine");
    let out = std::process::Command::new("docker")
        .args(["images", "postgres", "--format", "{{.Tag}}"])
        .output();
    let tags: Vec<String> = out
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if tags.contains(&preferred) {
        return preferred;
    }
    let prefix_dot = format!("{major}.");
    tags.into_iter()
        .find(|t| {
            t == &major.to_string()
                || t.starts_with(&prefix_dot)
                || t.starts_with(&format!("{major}-"))
        })
        .unwrap_or(preferred)
}

pub async fn pg(major: u32) -> Option<PgFixture> {
    if !docker_available() {
        eprintln!("NOTICE: docker unavailable (or PGWHY_SKIP_DOCKER=1) — skipping Postgres {major} integration test");
        return None;
    }
    let tag = pick_tag(major);
    let started = GenericImage::new("postgres", &tag)
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
        .await;
    let container = match started {
        Ok(c) => c,
        Err(e) => {
            eprintln!("NOTICE: could not start postgres:{tag} ({e}) — skipping Postgres {major} integration test");
            return None;
        }
    };
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
        image: format!("postgres:{tag}"),
    })
}
