//! Runs against a real server when `VIOLET_TEST_PG` names one, as
//! `host:port:user:password:database`. Skipped otherwise.
//!
//! ```sh
//! docker run -d --rm -e POSTGRES_PASSWORD=violet -e POSTGRES_USER=violet \
//!     -p 55499:5432 postgres:17-alpine
//! VIOLET_TEST_PG=localhost:55499:violet:violet:violet cargo test -p violet-postgres
//! ```

use std::time::Duration;

use violet_core::{
    ConnectionProfile, Credentials, Driver, DriverId, ErrorKind, ExecLimits, ObjectKind, Row,
    Secret, SslMode,
};
use violet_postgres::PostgresDriver;

fn target() -> Option<(ConnectionProfile, Credentials)> {
    let spec = std::env::var("VIOLET_TEST_PG").ok()?;
    let parts: Vec<&str> = spec.split(':').collect();
    let [host, port, user, password, database] = parts.as_slice() else {
        panic!("VIOLET_TEST_PG must be host:port:user:password:database");
    };
    let mut profile =
        ConnectionProfile::new(DriverId::new("postgres"), port.parse().expect("port"));
    profile.host = (*host).to_owned();
    profile.username = (*user).to_owned();
    profile.database = (*database).to_owned();
    profile.ssl = SslMode::Disable;
    Some((
        profile,
        Credentials {
            password: Some(Secret::new(*password)),
        },
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn browses_queries_and_cancels() {
    let Some((profile, credentials)) = target() else {
        eprintln!("VIOLET_TEST_PG not set; skipping");
        return;
    };
    let session = PostgresDriver
        .connect(&profile, &credentials)
        .await
        .expect("connect");

    let server = session.server_info().await.expect("server info");
    assert!(server.version.starts_with("PostgreSQL "));

    session
        .execute(
            "DROP SCHEMA IF EXISTS violet_test CASCADE;
             CREATE SCHEMA violet_test;
             CREATE TABLE violet_test.people (id serial PRIMARY KEY, name text NOT NULL, born date);
             COMMENT ON TABLE violet_test.people IS 'humans';
             CREATE VIEW violet_test.adults AS SELECT * FROM violet_test.people;
             CREATE FUNCTION violet_test.twice(x int) RETURNS int AS $$ SELECT x * 2; $$ LANGUAGE sql;
             INSERT INTO violet_test.people (name, born) VALUES ('Ada', '1815-12-10'), ('Alan', NULL);",
            ExecLimits::default(),
        )
        .await
        .expect("setup");

    assert!(
        session
            .schemas()
            .await
            .expect("schemas")
            .contains(&"violet_test".to_owned())
    );
    let objects = session.objects("violet_test").await.expect("objects");
    let kinds: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert!(kinds.contains(&("people", ObjectKind::Table)));
    assert!(kinds.contains(&("adults", ObjectKind::View)));
    assert!(kinds.contains(&("people_id_seq", ObjectKind::Sequence)));
    assert!(kinds.contains(&("twice(x integer)", ObjectKind::Function)));
    assert_eq!(
        objects
            .iter()
            .find(|o| o.name == "people")
            .and_then(|o| o.comment.clone()),
        Some("humans".to_owned())
    );

    let columns = session
        .columns("violet_test", "people")
        .await
        .expect("columns");
    assert_eq!(columns.len(), 3);
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert!(
        columns[0]
            .default
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval"))
    );
    assert_eq!(columns[2].data_type, "date");

    // A single statement: typed columns, NULL kept apart from text.
    let sets = session
        .execute(
            "SELECT id, name, born FROM violet_test.people ORDER BY id",
            ExecLimits::default(),
        )
        .await
        .expect("select");
    assert_eq!(sets.len(), 1);
    let set = &sets[0];
    assert_eq!(set.columns[0].type_name.as_deref(), Some("int4"));
    assert!(set.columns[0].numeric && !set.columns[1].numeric);
    assert_eq!(
        set.rows.row(0).map(Row::to_vec),
        Some(vec![
            Some("1".into()),
            Some("Ada".into()),
            Some("1815-12-10".into())
        ])
    );
    assert_eq!(set.rows.get(1, 2), Some(None));

    // Several statements: one set each, commands with counts.
    let sets = session
        .execute(
            "UPDATE violet_test.people SET name = name; SELECT 1 AS one; SELECT 1 WHERE false",
            ExecLimits::default(),
        )
        .await
        .expect("script");
    assert_eq!(sets.len(), 3);
    assert!(!sets[0].has_rows() && sets[0].affected == Some(2));
    assert_eq!(
        sets[1].rows.iter().map(Row::to_vec).collect::<Vec<_>>(),
        vec![vec![Some("1".into())]]
    );
    assert!(sets[2].has_rows() && sets[2].rows.is_empty());

    // Row limit.
    let sets = session
        .execute("SELECT generate_series(1, 50)", ExecLimits { max_rows: 10 })
        .await
        .expect("limit");
    assert_eq!(sets[0].rows.len(), 10);
    assert!(sets[0].truncated);

    // Errors carry SQLSTATE and position.
    let error = session
        .execute("SELECT nope FROM violet_test.people", ExecLimits::default())
        .await
        .expect_err("bad column");
    assert_eq!(error.kind, ErrorKind::Query);
    assert_eq!(error.code.as_deref(), Some("42703"));
    assert_eq!(error.position, Some(8));

    // Cancellation.
    let canceller = session.canceller().expect("cancel");
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel().await
    });
    let error = session
        .execute("SELECT pg_sleep(10)", ExecLimits::default())
        .await
        .expect_err("cancelled");
    assert_eq!(error.kind, ErrorKind::Cancelled);
    cancel.await.expect("join").expect("cancel sent");

    session
        .execute("DROP SCHEMA violet_test CASCADE", ExecLimits::default())
        .await
        .expect("cleanup");
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_is_an_authentication_error() {
    let Some((profile, _)) = target() else {
        return;
    };
    let credentials = Credentials {
        password: Some(Secret::new("wrong")),
    };
    let error = PostgresDriver
        .connect(&profile, &credentials)
        .await
        .err()
        .expect("rejected");
    assert_eq!(error.kind, ErrorKind::Authentication, "{error:?}");
}
