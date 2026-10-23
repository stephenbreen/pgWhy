//! Connection core integration tests against containerized Postgres (U4).

mod harness;

use harness::{PgFixture, PG_DB, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{
    CloseReason, ConnEvent, ConnectionManager, ManagerOptions, NoTunnels, Purpose, TlsState,
};
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_core::{ErrorClass, ErrorKind};
use std::sync::Arc;
use std::time::Duration;

fn entry(pg: &PgFixture, host: &str, sslmode: SslMode) -> ConnectionEntry {
    ConnectionEntry {
        id: ConnectionId::new(),
        name: "test".into(),
        group: None,
        host: host.into(),
        port: pg.port,
        database: PG_DB.into(),
        user: PG_USER.into(),
        maintenance_db: "postgres".into(),
        sslmode,
        ssl_root_cert: None,
        tunnel: None,
        env_tag: EnvTag::Local,
        password_saved: true,
    }
}

fn manager_with(
    entries: &[&ConnectionEntry],
    password: &str,
    options: ManagerOptions,
) -> ConnectionManager {
    let vault = Arc::new(Vault::new(Arc::new(MemoryStore::default())));
    for e in entries {
        vault
            .store(
                &SecretKey::new(&e.id, SecretKind::DbPassword),
                Secret::new(password),
                true,
            )
            .unwrap();
    }
    let m = ConnectionManager::new(vault, Arc::new(NoTunnels), options);
    m.set_entries(entries.iter().map(|e| (*e).clone()));
    m
}

fn manager(entries: &[&ConnectionEntry]) -> ConnectionManager {
    manager_with(entries, PG_PASSWORD, ManagerOptions::default())
}

async fn terminate(admin: &tokio_postgres::Client, pid: i32) {
    admin
        .execute("select pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap();
}

#[tokio::test]
async fn plaintext_modes_connect_and_report_plaintext() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    for mode in [SslMode::Disable, SslMode::Prefer] {
        let e = entry(&pg, &pg.host, mode);
        let m = manager(&[&e]);
        let conn = m.open(&e.id, PG_DB, Purpose::EditorTab).await.unwrap();
        assert_eq!(
            conn.info.tls,
            TlsState::Plaintext,
            "{mode:?} against a non-TLS server"
        );
        assert_eq!(conn.info.server_major(), 16);
        let one: i32 = conn.client.query_one("select 1", &[]).await.unwrap().get(0);
        assert_eq!(one, 1);
    }
    // require against a server without TLS must fail, never fall back
    let e = entry(&pg, &pg.host, SslMode::Require);
    let err = manager(&[&e])
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Tls, "{err:?}");
}

#[tokio::test]
async fn tls_modes_verify_as_libpq_does() {
    let pki = harness::pki("localhost");
    let Some(pg) = harness::pg_tls(16, &pki).await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("root.crt");
    std::fs::write(&ca, &pki.ca_pem).unwrap();

    // prefer + require: encrypted, unverified
    for mode in [SslMode::Prefer, SslMode::Require] {
        let e = entry(&pg, "localhost", mode);
        let conn = manager(&[&e])
            .open(&e.id, PG_DB, Purpose::EditorTab)
            .await
            .unwrap();
        assert_eq!(conn.info.tls, TlsState::EncryptedUnverified, "{mode:?}");
    }
    // verify-full with the right CA and the certificate's host name
    let mut e = entry(&pg, "localhost", SslMode::VerifyFull);
    e.ssl_root_cert = Some(ca.clone());
    let conn = manager(&[&e])
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    assert_eq!(conn.info.tls, TlsState::EncryptedVerified);

    // verify-full dialing a name the certificate doesn't cover: hard TLS failure
    let mut wrong = entry(&pg, "127.0.0.1", SslMode::VerifyFull);
    wrong.ssl_root_cert = Some(ca.clone());
    let err = manager(&[&wrong])
        .open(&wrong.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Tls, "{err:?}");
    assert!(
        err.hint
            .as_deref()
            .unwrap_or("")
            .contains("different host name"),
        "{err:?}"
    );

    // verify-ca tolerates the name mismatch but still checks the chain
    let mut ca_only = entry(&pg, "127.0.0.1", SslMode::VerifyCa);
    ca_only.ssl_root_cert = Some(ca.clone());
    let conn = manager(&[&ca_only])
        .open(&ca_only.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap();
    assert_eq!(conn.info.tls, TlsState::EncryptedVerified);

    // verify-full against a different CA: unknown issuer
    let other = harness::pki("localhost");
    let other_ca = dir.path().join("other.crt");
    std::fs::write(&other_ca, &other.ca_pem).unwrap();
    let mut untrusted = entry(&pg, "localhost", SslMode::VerifyFull);
    untrusted.ssl_root_cert = Some(other_ca);
    let err = manager(&[&untrusted])
        .open(&untrusted.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Tls, "{err:?}");
}

#[tokio::test]
async fn auth_and_network_errors_are_classified() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let e = entry(&pg, &pg.host, SslMode::Disable);
    let err = manager_with(&[&e], "wrong", ManagerOptions::default())
        .open(&e.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert_eq!(err.class, ErrorClass::FixThenRetry);
    assert!(
        !format!("{err:?}").contains("wrong"),
        "error must not echo the password"
    );

    let mut closed = e.clone();
    closed.port = 1; // nothing listens there
    let err = manager(&[&closed])
        .open(&closed.id, PG_DB, Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Network, "{err:?}");
    assert_eq!(err.class, ErrorClass::Retriable);

    let err = manager(&[&e])
        .open(&e.id, "no_such_db", Purpose::EditorTab)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config, "{err:?}");
}

#[tokio::test]
async fn introspection_connection_reopens_transparently_after_server_kill() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let e = entry(&pg, &pg.host, SslMode::Disable);
    let m = manager_with(
        &[&e],
        PG_PASSWORD,
        ManagerOptions {
            health_check_after: Duration::ZERO,
            ..ManagerOptions::default()
        },
    );
    let first = m.introspection(&e.id, PG_DB).await.unwrap();
    let pid = first.info.backend_pid;
    // same connection is shared while healthy
    assert_eq!(
        m.introspection(&e.id, PG_DB)
            .await
            .unwrap()
            .info
            .backend_pid,
        pid
    );

    let admin = m.open(&e.id, PG_DB, Purpose::Test).await.unwrap();
    terminate(&admin.client, pid).await;
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let again = m.introspection(&e.id, PG_DB).await.unwrap();
    assert_ne!(
        again.info.backend_pid, pid,
        "a fresh connection replaced the killed one"
    );
    let n: i64 = again
        .client
        .query_one("select count(*) from pg_class", &[])
        .await
        .unwrap()
        .get(0);
    assert!(n > 0);
}

#[tokio::test]
async fn editor_connection_loss_is_reported_never_reconnected() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let e = entry(&pg, &pg.host, SslMode::Disable);
    let m = manager(&[&e]);
    let mut events = m.subscribe();
    let tab = m.open(&e.id, PG_DB, Purpose::EditorTab).await.unwrap();
    tab.client
        .batch_execute("begin; create temp table t(x int);")
        .await
        .unwrap();
    let admin = m.open(&e.id, PG_DB, Purpose::Test).await.unwrap();
    terminate(&admin.client, tab.info.backend_pid).await;

    tokio::time::timeout(Duration::from_secs(5), tab.closed())
        .await
        .expect("closed() resolves");
    assert!(tab.is_closed());
    let err = tab.client.query_one("select 1", &[]).await.unwrap_err();
    assert!(err.is_closed() || err.as_db_error().is_some(), "{err}");

    let lost = loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            ConnEvent::Closed {
                handle,
                close,
                purpose,
                ..
            } if handle == tab.handle() => break (close, purpose),
            _ => continue,
        }
    };
    assert!(matches!(lost.0, CloseReason::Lost { .. }), "{lost:?}");
    assert_eq!(lost.1, Purpose::EditorTab);
}

#[tokio::test]
async fn quiesce_closes_own_connections_and_blocks_reopens_until_released() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let e = entry(&pg, &pg.host, SslMode::Disable);
    let m = manager(&[&e]);
    let mut events = m.subscribe();
    let tree = m.introspection(&e.id, PG_DB).await.unwrap();
    let tab = m.open(&e.id, PG_DB, Purpose::EditorTab).await.unwrap();
    let other_db = m
        .open(&e.id, "postgres", Purpose::Maintenance)
        .await
        .unwrap();

    let guard = m.quiesce(&e.server_key(), &[PG_DB.to_string()]).await;
    assert!(
        tree.is_closed() && tab.is_closed(),
        "pgWhy's own connections to the target are closed"
    );
    assert!(!other_db.is_closed(), "other databases are untouched");

    // neither a tree click (transparent reopen) nor a new tab can get in mid-swap
    let err = m.introspection(&e.id, PG_DB).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Refused);
    assert!(m.open(&e.id, PG_DB, Purpose::EditorTab).await.is_err());

    let mut quiesced = 0;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
        if matches!(
            ev,
            ConnEvent::Closed {
                close: CloseReason::Quiesced,
                ..
            }
        ) {
            quiesced += 1;
        }
    }
    assert_eq!(quiesced, 2);

    drop(guard);
    assert!(
        m.introspection(&e.id, PG_DB).await.is_ok(),
        "released quiesce allows reopening"
    );
}

#[tokio::test]
async fn quiesce_waits_for_an_in_flight_grid_commit() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let e = entry(&pg, &pg.host, SslMode::Disable);
    let m = manager(&[&e]);
    let commit = m.edit_guard(&e.server_key(), PG_DB).await.unwrap();
    let m2 = m.clone();
    let server = e.server_key();
    let quiesce = tokio::spawn(async move { m2.quiesce(&server, &[PG_DB.to_string()]).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !quiesce.is_finished(),
        "quiesce waits while a commit holds the edit guard"
    );
    drop(commit);
    let guard = tokio::time::timeout(Duration::from_secs(2), quiesce)
        .await
        .unwrap()
        .unwrap();
    assert!(
        m.edit_guard(&e.server_key(), PG_DB).await.is_err(),
        "no new commits while quiesced"
    );
    drop(guard);
}

#[tokio::test]
async fn idle_introspection_connections_close_and_reopen_lazily() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let e = entry(&pg, &pg.host, SslMode::Disable);
    let m = manager_with(
        &[&e],
        PG_PASSWORD,
        ManagerOptions {
            idle_close: Duration::ZERO,
            ..ManagerOptions::default()
        },
    );
    let c = m.introspection(&e.id, PG_DB).await.unwrap();
    assert_eq!(m.close_idle().await, 1);
    assert!(c.is_closed());
    assert!(m.introspection(&e.id, PG_DB).await.is_ok());
}

#[tokio::test]
async fn a_changed_idle_policy_applies_without_a_restart() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let e = entry(&pg, &pg.host, SslMode::Disable);
    let m = manager_with(&[&e], PG_PASSWORD, ManagerOptions::default());
    let c = m.introspection(&e.id, PG_DB).await.unwrap();
    assert_eq!(m.close_idle().await, 0, "ten minutes have not passed");
    m.set_idle_close(Duration::ZERO);
    assert_eq!(m.close_idle().await, 1);
    assert!(c.is_closed());
}

/// A TCP stub in front of the fixture that accepts at once but only starts relaying once
/// released — a connect that is "in flight" for as long as the test wants.
struct GatedProxy {
    port: u16,
    release: tokio::sync::watch::Sender<bool>,
}

impl GatedProxy {
    async fn start(pg: &PgFixture) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, rx) = tokio::sync::watch::channel(false);
        let target = format!("{}:{}", pg.host, pg.port);
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let mut rx = rx.clone();
                let target = target.clone();
                tokio::spawn(async move {
                    let _ = rx.wait_for(|r| *r).await;
                    if let Ok(mut server) = tokio::net::TcpStream::connect(target).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
            }
        });
        Self { port, release }
    }

    fn release(&self) {
        self.release.send_replace(true);
    }
}

/// An unreachable (here: stalled) host must only stall callers of its own connection, and an
/// edit or delete that lands while a connect is in flight must not let that connection register.
#[tokio::test]
async fn stalled_connect_blocks_nobody_and_a_stale_result_is_discarded() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    macro_rules! five {
        ($f:expr) => {
            tokio::time::timeout(Duration::from_secs(5), $f)
        };
    }
    for edit in [false, true] {
        let proxy = GatedProxy::start(&pg).await;
        let mut slow = entry(&pg, "127.0.0.1", SslMode::Disable);
        slow.port = proxy.port;
        let healthy = entry(&pg, &pg.host, SslMode::Disable);
        let m = manager(&[&slow, &healthy]);

        let (m2, id) = (m.clone(), slow.id.clone());
        let stalled = tokio::spawn(async move { m2.introspection(&id, PG_DB).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(300)).await;
        // a second caller for the same key waits on the first rather than opening its own
        let (m3, id) = (m.clone(), slow.id.clone());
        let twin = tokio::spawn(async move { m3.introspection(&id, PG_DB).await.map(|_| ()) });

        // everything else keeps working while that connect hangs
        five!(m.introspection(&healthy.id, PG_DB))
            .await
            .expect("another entry's introspection is not stalled")
            .unwrap();
        five!(m.disconnect(&healthy.id)).await.expect("disconnect");

        if edit {
            let mut changed = slow.clone();
            changed.sslmode = SslMode::Prefer;
            five!(m.upsert_entry(changed)).await.expect("upsert_entry");
        } else {
            five!(m.remove_entry(&slow.id)).await.expect("remove_entry");
        }
        proxy.release();
        let res = five!(stalled).await.unwrap().unwrap();
        assert!(res.is_err(), "stale connection discarded (edit={edit})");
        let res = five!(twin).await.unwrap().unwrap();
        let registered = m
            .live_connections()
            .iter()
            .filter(|c| c.connection_id == slow.id)
            .count();
        if edit {
            // the waiting caller opened afresh under the new settings, and only that one lives
            assert!(res.is_ok(), "{res:?}");
            assert_eq!(registered, 1, "only the post-edit connection is registered");
        } else {
            assert!(res.is_err(), "deleted entry cannot connect");
            assert_eq!(registered, 0, "nothing registered for a deleted entry");
        }
    }
}

/// A quiesce that starts while a connect is in flight must win: the late connection is dropped.
#[tokio::test]
async fn quiesce_during_connect_discards_the_late_connection() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let proxy = GatedProxy::start(&pg).await;
    let mut e = entry(&pg, "127.0.0.1", SslMode::Disable);
    e.port = proxy.port;
    let m = manager(&[&e]);
    let (m2, id) = (m.clone(), e.id.clone());
    let opening = tokio::spawn(async move { m2.open(&id, PG_DB, Purpose::EditorTab).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let guard = m.quiesce(&e.server_key(), &[PG_DB.to_string()]).await;
    proxy.release();
    let res = tokio::time::timeout(Duration::from_secs(5), opening)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res.unwrap_err().kind, ErrorKind::Refused);
    assert!(m.live_connections().is_empty());
    drop(guard);
}
