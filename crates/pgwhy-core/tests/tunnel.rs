//! SSH tunnel integration tests: an sshd container and a Postgres container share a
//! private Docker network, and Postgres publishes no port — so every successful query here
//! went through the tunnel.

use pgwhy_core::config::{
    ConfigStore, ConnectionEntry, ConnectionId, EnvTag, SslMode, TunnelAuth, TunnelConfig,
};
use pgwhy_core::conn::{classify, ConnectionManager, ManagerOptions, Purpose, TunnelProvider};
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_core::tunnel::{TunnelEvent, TunnelManager, TunnelOptions, TunnelState};
use pgwhy_core::{AppError, ErrorClass, ErrorKind};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use testcontainers::core::{ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::sync::broadcast;

const PG_USER: &str = "pgwhy";
const PG_PASSWORD: &str = "pgwhy-test";
const PG_DB: &str = "pgwhy_test";
const SSH_USER: &str = "pgwhy";
const SSH_PASSWORD: &str = "ssh-test-pw";
const SSHD_CONFIG: &str = include_str!("fixtures/sshd/sshd_config");
const SSHD_START: &str = include_str!("fixtures/sshd/start.sh");

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

struct Fixture {
    sshd: ContainerAsync<GenericImage>,
    _pg: ContainerAsync<GenericImage>,
    ssh_port: u16,
    pg_host: String,
    dir: tempfile::TempDir,
    key_path: PathBuf,
}

/// Write a fresh ed25519 key (optionally passphrase-protected); returns (path, public line).
fn write_key(dir: &std::path::Path, name: &str, passphrase: Option<&str>) -> (PathBuf, String) {
    let key = PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519).unwrap();
    let public = key.public_key().to_openssh().unwrap();
    let stored = match passphrase {
        Some(p) => key.encrypt(&mut russh::keys::key::safe_rng(), p).unwrap(),
        None => key,
    };
    let path = dir.join(name);
    std::fs::write(&path, stored.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    (path, public)
}

/// sshd + Postgres on a private network, Postgres reachable only via the sshd.
async fn fixture(passphrase: Option<&str>) -> Option<Fixture> {
    if !docker_available() {
        eprintln!("NOTICE: docker unavailable (or PGWHY_SKIP_DOCKER=1) — skipping SSH tunnel integration test");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let (key_path, public) = write_key(dir.path(), "id_ed25519", passphrase);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let network = format!("pgwhy-tunnel-{suffix}");
    let pg_host = format!("pg-{}", &suffix[..12]);

    let sshd = GenericImage::new("alpine", "3.22")
        .with_exposed_port(22.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "Server listening on 0.0.0.0 port 22",
        ))
        .with_entrypoint("sh")
        .with_network(&network)
        .with_cmd(["/fixture/start.sh"])
        .with_env_var("SSH_PASSWORD", SSH_PASSWORD)
        .with_copy_to("/fixture/start.sh", SSHD_START.as_bytes().to_vec())
        .with_copy_to("/fixture/sshd_config", SSHD_CONFIG.as_bytes().to_vec())
        .with_copy_to(
            "/fixture/authorized_keys",
            format!("{public}\n").into_bytes(),
        )
        .with_startup_timeout(Duration::from_secs(120))
        .start();
    let pg = GenericImage::new("postgres", "16-alpine")
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_network(&network)
        .with_container_name(&pg_host)
        .with_env_var("POSTGRES_USER", PG_USER)
        .with_env_var("POSTGRES_PASSWORD", PG_PASSWORD)
        .with_env_var("POSTGRES_DB", PG_DB)
        .start();
    let (sshd, pg) = tokio::join!(sshd, pg);
    let sshd = sshd.expect("start sshd container");
    let pg = pg.expect("start postgres container");
    let ssh_port = sshd.get_host_port_ipv4(22.tcp()).await.unwrap();
    Some(Fixture {
        sshd,
        _pg: pg,
        ssh_port,
        pg_host,
        dir,
        key_path,
    })
}

impl Fixture {
    fn entry(&self) -> ConnectionEntry {
        self.entry_with(TunnelAuth::Key {
            key_path: self.key_path.clone(),
            passphrase_saved: false,
        })
    }

    fn entry_with(&self, auth: TunnelAuth) -> ConnectionEntry {
        ConnectionEntry {
            id: ConnectionId::new(),
            name: "tunneled".into(),
            group: None,
            // the real name, resolvable only from inside the Docker network
            host: self.pg_host.clone(),
            port: 5432,
            database: PG_DB.into(),
            user: PG_USER.into(),
            maintenance_db: "postgres".into(),
            sslmode: SslMode::Disable,
            ssl_root_cert: None,
            tunnel: Some(TunnelConfig {
                host: "127.0.0.1".into(),
                port: self.ssh_port,
                user: SSH_USER.into(),
                auth,
            }),
            env_tag: EnvTag::Local,
            password_saved: true,
        }
    }

    fn config(&self) -> ConfigStore {
        ConfigStore::new(self.dir.path().join("config"))
    }

    /// Kill every SSH session process, leaving the listener up (a dropped session).
    async fn kill_sessions(&self) {
        let mut r = self
            .sshd
            .exec(ExecCommand::new(["pkill", "-f", "sshd-session"]))
            .await
            .unwrap();
        let _ = r.stdout_to_vec().await;
    }
}

fn test_options() -> TunnelOptions {
    TunnelOptions {
        backoff_initial: Duration::from_millis(200),
        backoff_max: Duration::from_secs(2),
        keepalive_interval: Duration::from_secs(5),
        ..TunnelOptions::default()
    }
}

struct Setup {
    vault: Arc<Vault>,
    tunnels: TunnelManager,
    conns: ConnectionManager,
}

fn setup(fx: &Fixture, entries: &[&ConnectionEntry], options: TunnelOptions) -> Setup {
    let vault = Arc::new(Vault::new(Arc::new(MemoryStore::default())));
    for e in entries {
        vault
            .store(
                &SecretKey::new(&e.id, SecretKind::DbPassword),
                Secret::new(PG_PASSWORD),
                true,
            )
            .unwrap();
    }
    let tunnels = TunnelManager::new(vault.clone(), fx.config(), options).unwrap();
    let conns = ConnectionManager::new(
        vault.clone(),
        Arc::new(tunnels.clone()),
        ManagerOptions::default(),
    );
    conns.set_entries(entries.iter().map(|e| (*e).clone()));
    Setup {
        vault,
        tunnels,
        conns,
    }
}

/// First contact fails closed; trust what the server presented (as the UI's prompt would).
async fn trust_on_first_use(tunnels: &TunnelManager, entry: &ConnectionEntry) -> AppError {
    let err = tunnels.acquire(entry).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::HostKey, "{err:?}");
    let d = err.data.clone().expect("host key payload");
    tunnels
        .trust_host_key(
            d["host"].as_str().unwrap(),
            d["port"].as_u64().unwrap() as u16,
            d["key_type"].as_str().unwrap(),
            d["fingerprint"].as_str().unwrap(),
        )
        .unwrap();
    err
}

async fn wait_state(
    rx: &mut broadcast::Receiver<TunnelEvent>,
    id: &ConnectionId,
    state: TunnelState,
    within: Duration,
) -> TunnelEvent {
    tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Ok(ev) if &ev.connection_id == id && ev.state == state => return ev,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("event stream ended: {e}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("tunnel never reached {state:?}"))
}

#[tokio::test]
async fn queries_run_through_the_tunnel_and_multiplex_over_one_session() {
    let Some(fx) = fixture(None).await else {
        return;
    };
    let e = fx.entry();
    let s = setup(&fx, &[&e], test_options());
    trust_on_first_use(&s.tunnels, &e).await;

    let a = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    let b = s.conns.open(&e.id, PG_DB, Purpose::Grid).await.unwrap();
    let status = s.tunnels.status(&e.id);
    assert_eq!(status.state, TunnelState::Up);
    assert_eq!(status.leases, 2, "both connections share one tunnel");
    assert!(status.local_port.is_some());
    assert_ne!(a.info.backend_pid, b.info.backend_pid);

    // two concurrent 1.5 s sleeps finish in ~1.5 s only if the session multiplexes
    let started = Instant::now();
    let (ra, rb) = tokio::join!(
        a.client.query_one("select pg_sleep(1.5), 1::int4", &[]),
        b.client.query_one("select pg_sleep(1.5), 2::int4", &[]),
    );
    let elapsed = started.elapsed();
    assert_eq!(ra.unwrap().get::<_, i32>(1), 1);
    assert_eq!(rb.unwrap().get::<_, i32>(1), 2);
    assert!(
        elapsed < Duration::from_millis(2800),
        "serialized? {elapsed:?}"
    );

    drop(a);
    drop(b);
    assert_eq!(s.tunnels.status(&e.id).leases, 0);
}

#[tokio::test]
async fn unknown_host_key_fails_closed_until_trusted() {
    let Some(fx) = fixture(None).await else {
        return;
    };
    let e = fx.entry();
    let s = setup(&fx, &[&e], test_options());

    let err = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::HostKey);
    assert_eq!(err.class, ErrorClass::FixThenRetry);
    let d = err.data.clone().unwrap();
    assert_eq!(d["status"], "unknown");
    assert_eq!(d["host"], "127.0.0.1");
    assert_eq!(d["port"], fx.ssh_port);
    assert_eq!(d["key_type"], "ssh-ed25519");
    assert!(d["fingerprint"].as_str().unwrap().starts_with("SHA256:"));
    assert_eq!(s.tunnels.status(&e.id).state, TunnelState::Down);
    assert!(
        s.tunnels.trusted_host_keys().is_empty(),
        "never silently trusted"
    );

    s.tunnels
        .trust_host_key(
            "127.0.0.1",
            fx.ssh_port,
            d["key_type"].as_str().unwrap(),
            d["fingerprint"].as_str().unwrap(),
        )
        .unwrap();
    let conn = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    let one: i32 = conn.client.query_one("select 1", &[]).await.unwrap().get(0);
    assert_eq!(one, 1);

    // the decision is recorded in the app-owned trust store
    let file = std::fs::read_to_string(fx.config().dir().join("known_hosts.json")).unwrap();
    assert!(file.contains(d["fingerprint"].as_str().unwrap()));
    assert_eq!(s.tunnels.trusted_host_keys().len(), 1);
}

#[tokio::test]
async fn changed_host_key_hard_fails_naming_both_fingerprints() {
    let Some(fx) = fixture(None).await else {
        return;
    };
    let e = fx.entry();
    let s = setup(&fx, &[&e], test_options());
    let first = trust_on_first_use(&s.tunnels, &e).await;
    let real_fp = first.data.unwrap()["fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    // the server now presents a key other than the one trusted for it
    let old_fp = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    s.tunnels.forget_host_key("127.0.0.1", fx.ssh_port).unwrap();
    s.tunnels
        .trust_host_key("127.0.0.1", fx.ssh_port, "ssh-ed25519", old_fp)
        .unwrap();

    let err = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::HostKey);
    assert!(
        err.message.contains("host key changed — possible MITM"),
        "{}",
        err.message
    );
    assert!(err.message.contains(old_fp) && err.message.contains(&real_fp));
    let d = err.data.unwrap();
    assert_eq!(d["status"], "changed");
    assert_eq!(d["fingerprint"], real_fp.as_str());
    assert_eq!(d["previous_fingerprint"], old_fp);

    // a deliberate, recorded re-trust connects
    s.tunnels
        .trust_host_key("127.0.0.1", fx.ssh_port, "ssh-ed25519", &real_fp)
        .unwrap();
    let conn = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    conn.client.simple_query("select 1").await.unwrap();
    let rec = &s.tunnels.trusted_host_keys()[0].record;
    assert_eq!(rec.current.fingerprint, real_fp);
    assert_eq!(rec.replaced[0].key.fingerprint, old_fp);
}

#[tokio::test]
async fn dropped_session_fails_in_flight_retriably_and_reconnects_on_the_same_port() {
    let Some(fx) = fixture(None).await else {
        return;
    };
    let e = fx.entry();
    let s = setup(&fx, &[&e], test_options());
    trust_on_first_use(&s.tunnels, &e).await;
    let mut events = s.tunnels.subscribe();

    let a = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    let port_before = s.tunnels.status(&e.id).local_port.unwrap();
    let in_flight = {
        let client = &a.client;
        async move { client.query_one("select pg_sleep(20)", &[]).await }
    };
    let killer = async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        fx.kill_sessions().await;
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(in_flight, killer)
    })
    .await
    .expect("in-flight query should fail promptly when the session drops");
    let err = classify(&result.unwrap_err(), "run a query");
    assert!(err.is_retriable(), "tunnel drop must be retriable: {err:?}");

    wait_state(
        &mut events,
        &e.id,
        TunnelState::Reconnecting,
        Duration::from_secs(10),
    )
    .await;
    let up = wait_state(&mut events, &e.id, TunnelState::Up, Duration::from_secs(20)).await;
    assert_eq!(
        up.local_port,
        Some(port_before),
        "reconnect keeps the local port"
    );

    // the next attempt works through the re-established session
    let b = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    let one: i32 = b.client.query_one("select 1", &[]).await.unwrap().get(0);
    assert_eq!(one, 1);
    assert_eq!(s.tunnels.status(&e.id).local_port, Some(port_before));
    drop(a);
}

#[tokio::test]
async fn pinned_tunnel_refuses_disconnect_until_released() {
    let Some(fx) = fixture(None).await else {
        return;
    };
    let e = fx.entry();
    let options = TunnelOptions {
        linger: Duration::from_millis(300),
        ..test_options()
    };
    let s = setup(&fx, &[&e], options);
    trust_on_first_use(&s.tunnels, &e).await;
    let mut events = s.tunnels.subscribe();

    let conn = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    let err = s.tunnels.disconnect(&e.id, false).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Refused);
    assert_eq!(err.data.as_ref().unwrap()["leases"], 1);
    assert_eq!(
        s.tunnels.status(&e.id).state,
        TunnelState::Up,
        "refusal left it up"
    );
    conn.client.simple_query("select 1").await.unwrap();

    drop(conn);
    s.tunnels.disconnect(&e.id, false).unwrap();
    wait_state(
        &mut events,
        &e.id,
        TunnelState::Down,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(s.tunnels.status(&e.id).local_port, None);

    // releasing the last lease closes it on its own after the linger
    let conn = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    drop(conn);
    wait_state(
        &mut events,
        &e.id,
        TunnelState::Down,
        Duration::from_secs(5),
    )
    .await;

    // a forced disconnect tears down even while pinned; the pinned connection fails
    let conn = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    s.tunnels.disconnect(&e.id, true).unwrap();
    wait_state(
        &mut events,
        &e.id,
        TunnelState::Down,
        Duration::from_secs(5),
    )
    .await;
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        conn.client.simple_query("select 1"),
    )
    .await
    .expect("query on a torn-down tunnel must fail, not hang");
    assert!(r.is_err());
}

#[tokio::test]
async fn credentials_are_resolved_before_connecting() {
    let Some(fx) = fixture(Some("key-pass")).await else {
        return;
    };
    let e = fx.entry();
    let pw_entry = fx.entry_with(TunnelAuth::Password {
        password_saved: false,
    });
    let s = setup(&fx, &[&e, &pw_entry], test_options());

    // encrypted key, no passphrase available → Credentials, nothing attempted
    assert_eq!(
        s.tunnels.missing_secrets(&e).unwrap(),
        vec![SecretKind::SshKeyPassphrase]
    );
    let err = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials, "{err:?}");
    let d = err.data.unwrap();
    assert_eq!(d["secret"], "ssh_key_passphrase");
    assert_eq!(d["reason"], "missing");
    assert!(
        s.tunnels.trusted_host_keys().is_empty(),
        "no SSH contact was made"
    );

    // wrong passphrase → Credentials (incorrect)
    let pass_key = SecretKey::new(&e.id, SecretKind::SshKeyPassphrase);
    s.vault
        .store(&pass_key, Secret::new("nope"), false)
        .unwrap();
    let err = s.tunnels.acquire(&e).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
    assert_eq!(err.data.unwrap()["reason"], "incorrect");

    // right passphrase → host-key prompt → connects
    s.vault
        .store(&pass_key, Secret::new("key-pass"), false)
        .unwrap();
    assert!(s.tunnels.missing_secrets(&e).unwrap().is_empty());
    trust_on_first_use(&s.tunnels, &e).await;
    let conn = s
        .conns
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    conn.client.simple_query("select 1").await.unwrap();

    // password auth: missing → Credentials; wrong → Auth; right → connects
    let err = s.tunnels.acquire(&pw_entry).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
    assert_eq!(err.data.unwrap()["secret"], "ssh_password");
    let pw_key = SecretKey::new(&pw_entry.id, SecretKind::SshPassword);
    s.vault.store(&pw_key, Secret::new("wrong"), false).unwrap();
    let err = s.tunnels.acquire(&pw_entry).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(
        !err.message.contains("wrong"),
        "no secret material in messages"
    );
    s.vault
        .store(&pw_key, Secret::new(SSH_PASSWORD), false)
        .unwrap();
    let conn = s
        .conns
        .open(&pw_entry.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    conn.client.simple_query("select 1").await.unwrap();

    // a key the server doesn't authorize → Auth
    let (other, _) = write_key(fx.dir.path(), "other", None);
    let mut stranger = fx.entry();
    if let Some(t) = stranger.tunnel.as_mut() {
        t.auth = TunnelAuth::Key {
            key_path: other,
            passphrase_saved: false,
        };
    }
    let err = s.tunnels.acquire(&stranger).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
}
