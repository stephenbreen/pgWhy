//! Container-connect smoke test: proves the testcontainers harness and tokio-postgres wiring.

mod harness;

#[tokio::test]
async fn connects_to_containerized_postgres() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let (client, conn) = tokio_postgres::connect(&pg.conninfo(), tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(conn);
    let row = client
        .query_one("select current_setting('server_version_num')::int", &[])
        .await
        .unwrap();
    let version: i32 = row.get(0);
    assert_eq!(version / 10000, 16);
}
