use std::env;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use launchpad::eventually;
use mysql_async::prelude::Queryable;
use mysql_time::MysqlTime;
use nom_sql::Relation;
use readyset::consensus::{Authority, LocalAuthority, LocalAuthorityStore};
use readyset::recipe::changelist::ChangeList;
use readyset::{ControllerHandle, ReadySetError, ReadySetResult};
use readyset_data::{DfValue, TinyText};
use readyset_server::Builder;
use replicators::{Config, NoriaAdapter};
use test_utils::slow;
use tracing::{error, trace};

const MAX_ATTEMPTS: usize = 40;

// Postgres does not accept MySQL escapes, so rename the table before the query
const PGSQL_RENAME: (&str, &str) = ("`groups`", "groups");

const CREATE_SCHEMA: &str = "
    DROP TABLE IF EXISTS `groups` CASCADE;
    DROP VIEW IF EXISTS noria_view;
    CREATE TABLE `groups` (
        id int NOT NULL PRIMARY KEY,
        string varchar(20),
        bignum int
    );
    CREATE VIEW noria_view AS SELECT id,string,bignum FROM `groups` ORDER BY id ASC";

const POPULATE_SCHEMA: &str =
    "INSERT INTO `groups` VALUES (1, 'abc', 2), (2, 'bcd', 3), (3, NULL, NULL), (40, 'xyz', 4)";

/// A convenience init to convert 3 character byte slice to TinyText noria type
const fn tiny<const N: usize>(text: &[u8; N]) -> DfValue {
    DfValue::TinyText(TinyText::from_arr(text))
}

const SNAPSHOT_RESULT: &[&[DfValue]] = &[
    &[DfValue::Int(1), tiny(b"abc"), DfValue::Int(2)],
    &[DfValue::Int(2), tiny(b"bcd"), DfValue::Int(3)],
    &[DfValue::Int(3), DfValue::None, DfValue::None],
    &[DfValue::Int(40), tiny(b"xyz"), DfValue::Int(4)],
];

const TESTS: &[(&str, &str, &[&[DfValue]])] = &[
    (
        "Test UPDATE key column replication",
        "UPDATE `groups` SET id=id+10",
        &[
            &[DfValue::Int(11), tiny(b"abc"), DfValue::Int(2)],
            &[DfValue::Int(12), tiny(b"bcd"), DfValue::Int(3)],
            &[DfValue::Int(13), DfValue::None, DfValue::None],
            &[DfValue::Int(50), tiny(b"xyz"), DfValue::Int(4)],
        ],
    ),
    (
        "Test DELETE replication",
        "DELETE FROM `groups` WHERE string='bcd'",
        &[
            &[DfValue::Int(11), tiny(b"abc"), DfValue::Int(2)],
            &[DfValue::Int(13), DfValue::None, DfValue::None],
            &[DfValue::Int(50), tiny(b"xyz"), DfValue::Int(4)],
        ],
    ),
    (
        "Test INSERT replication",
        "INSERT INTO `groups` VALUES (1, 'abc', 2), (2, 'bcd', 3), (40, 'xyz', 4)",
        &[
            &[DfValue::Int(1), tiny(b"abc"), DfValue::Int(2)],
            &[DfValue::Int(2), tiny(b"bcd"), DfValue::Int(3)],
            &[DfValue::Int(11), tiny(b"abc"), DfValue::Int(2)],
            &[DfValue::Int(13), DfValue::None, DfValue::None],
            &[DfValue::Int(40), tiny(b"xyz"), DfValue::Int(4)],
            &[DfValue::Int(50), tiny(b"xyz"), DfValue::Int(4)],
        ],
    ),
    (
        "Test UPDATE non-key column replication",
        "UPDATE `groups` SET bignum=id+10",
        &[
            &[DfValue::Int(1), tiny(b"abc"), DfValue::Int(11)],
            &[DfValue::Int(2), tiny(b"bcd"), DfValue::Int(12)],
            &[DfValue::Int(11), tiny(b"abc"), DfValue::Int(21)],
            &[DfValue::Int(13), DfValue::None, DfValue::Int(23)],
            &[DfValue::Int(40), tiny(b"xyz"), DfValue::Int(50)],
            &[DfValue::Int(50), tiny(b"xyz"), DfValue::Int(60)],
        ],
    ),
];

/// Test query we issue after replicator disconnect
const DISCONNECT_QUERY: &str = "INSERT INTO `groups` VALUES (3, 'abc', 2), (5, 'xyz', 4)";
/// Test result after replicator reconnects and catches up
const RECONNECT_RESULT: &[&[DfValue]] = &[
    &[DfValue::Int(1), tiny(b"abc"), DfValue::Int(11)],
    &[DfValue::Int(2), tiny(b"bcd"), DfValue::Int(12)],
    &[DfValue::Int(3), tiny(b"abc"), DfValue::Int(2)],
    &[DfValue::Int(5), tiny(b"xyz"), DfValue::Int(4)],
    &[DfValue::Int(11), tiny(b"abc"), DfValue::Int(21)],
    &[DfValue::Int(13), DfValue::None, DfValue::Int(23)],
    &[DfValue::Int(40), tiny(b"xyz"), DfValue::Int(50)],
    &[DfValue::Int(50), tiny(b"xyz"), DfValue::Int(60)],
];

struct TestHandle {
    url: String,
    noria: readyset_server::Handle,
    authority: Arc<Authority>,
    // We spin a whole runtime for the replication task because the tokio postgres
    // connection spawns a background task we can only terminate by dropping the runtime
    replication_rt: Option<tokio::runtime::Runtime>,
    ready_notify: Option<Arc<tokio::sync::Notify>>,
}

impl Drop for TestHandle {
    fn drop(&mut self) {
        if let Some(rt) = self.replication_rt.take() {
            rt.shutdown_background();
        }
    }
}

enum DbConnection {
    MySQL(mysql_async::Conn),
    PostgreSQL(
        tokio_postgres::Client,
        tokio::task::JoinHandle<ReadySetResult<()>>,
    ),
}

impl DbConnection {
    async fn connect(url: &str) -> ReadySetResult<Self> {
        if url.starts_with("mysql") {
            let opts: mysql_async::Opts = url.parse().unwrap();
            let test_db_name = opts.db_name().unwrap();
            let no_db_opts =
                mysql_async::OptsBuilder::from_opts(opts.clone()).db_name::<String>(None);

            // First, connect without a db
            let mut client = mysql_async::Conn::new(no_db_opts).await?;

            // Then drop and recreate the test db
            client
                .query_drop(format!("DROP SCHEMA IF EXISTS {test_db_name};"))
                .await?;
            client
                .query_drop(format!("CREATE SCHEMA {test_db_name};"))
                .await?;

            // Then switch to the test db
            client.query_drop(format!("USE {test_db_name};")).await?;

            // Set very low execution time, to make sure we override it later on
            client.query_drop("SET GLOBAL MAX_EXECUTION_TIME=1").await?;

            Ok(DbConnection::MySQL(client))
        } else if url.starts_with("postgresql") {
            let opts = tokio_postgres::Config::from_str(url)?;

            // Drop and recreate the test db
            {
                let test_db_name = opts.get_dbname().unwrap();
                let mut no_db_opts = opts.clone();
                no_db_opts.dbname("postgres");
                let (no_db_client, conn) = no_db_opts.connect(tokio_postgres::NoTls).await?;
                tokio::spawn(conn);

                no_db_client
                    .simple_query(&format!("DROP SCHEMA IF EXISTS {test_db_name} CASCADE"))
                    .await?;
                no_db_client
                    .simple_query(&format!("CREATE SCHEMA {test_db_name}"))
                    .await?;
            }

            let (client, conn) = tokio_postgres::connect(&pgsql_url(), tokio_postgres::NoTls)
                .await
                .unwrap();
            let connection_handle = tokio::spawn(async move { conn.await.map_err(Into::into) });
            Ok(DbConnection::PostgreSQL(client, connection_handle))
        } else {
            unimplemented!()
        }
    }

    async fn query(&mut self, query: &str) -> ReadySetResult<()> {
        match self {
            DbConnection::MySQL(c) => {
                c.query_drop(query).await?;
            }
            DbConnection::PostgreSQL(c, _) => {
                let query = query.replace(PGSQL_RENAME.0, PGSQL_RENAME.1);
                c.simple_query(query.as_str()).await?;
            }
        }
        Ok(())
    }

    async fn stop(self) {
        match self {
            DbConnection::MySQL(_) => {}
            DbConnection::PostgreSQL(_, h) => {
                h.abort();
                let _ = h.await;
            }
        }
    }
}

impl TestHandle {
    async fn start_noria(url: String, config: Option<Config>) -> ReadySetResult<TestHandle> {
        let authority_store = Arc::new(LocalAuthorityStore::new());
        let authority = Arc::new(Authority::from(LocalAuthority::new_with_store(
            authority_store,
        )));
        TestHandle::start_with_authority(url, authority, config).await
    }

    async fn start_with_authority(
        url: String,
        authority: Arc<Authority>,
        config: Option<Config>,
    ) -> ReadySetResult<TestHandle> {
        readyset_tracing::init_test_logging();
        let mut builder = Builder::for_tests();
        let mut persistence = readyset_server::PersistenceParameters::default();
        persistence.mode = readyset_server::DurabilityMode::DeleteOnExit;
        builder.set_persistence(persistence);
        let noria = builder.start(Arc::clone(&authority)).await.unwrap();

        let mut handle = TestHandle {
            url,
            noria,
            authority,
            replication_rt: None,
            ready_notify: Some(Default::default()),
        };

        handle.start_repl(config).await?;

        Ok(handle)
    }

    async fn controller(&self) -> ControllerHandle {
        ControllerHandle::new(Arc::clone(&self.authority)).await
    }

    async fn stop(mut self) {
        self.stop_repl().await;
        self.noria.shutdown();
        self.noria.wait_done().await;
    }

    async fn stop_repl(&mut self) {
        if let Some(rt) = self.replication_rt.take() {
            rt.shutdown_background();
        }
    }

    async fn start_repl(&mut self, config: Option<Config>) -> ReadySetResult<()> {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let controller = ControllerHandle::new(Arc::clone(&self.authority)).await;

        let url = self.url.clone().into();
        let ready_notify = self.ready_notify.clone();
        let _ = runtime.spawn(async move {
            if let Err(error) = NoriaAdapter::start(
                controller,
                Config {
                    replication_url: Some(url),
                    ..config.unwrap_or_default()
                },
                ready_notify.clone(),
            )
            .await
            {
                error!(%error, "Error in replicator");
                if let Some(notify) = ready_notify {
                    notify.notify_one();
                }
            }
        });

        if let Some(rt) = self.replication_rt.replace(runtime) {
            rt.shutdown_background();
        }

        Ok(())
    }

    #[track_caller]
    async fn check_results(
        &mut self,
        view_name: &str,
        test_name: &str,
        test_results: &[&[DfValue]],
    ) -> ReadySetResult<()> {
        let mut attempt: usize = 0;
        loop {
            match self.check_results_inner(view_name).await {
                Err(_) if attempt < MAX_ATTEMPTS => {
                    // Sometimes things are slow in CI, so we retry a few times before giving up
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Ok(res) if res != test_results && attempt < MAX_ATTEMPTS => {
                    // Sometimes things are slow in CI, so we retry a few times before giving up
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Ok(res) => {
                    assert_eq!(res, *test_results, "{} incorrect", test_name);
                    break Ok(());
                }
                Err(err) => break Err(err),
            }
        }
    }

    async fn check_results_inner(&mut self, view_name: &str) -> ReadySetResult<Vec<Vec<DfValue>>> {
        let mut getter = self
            .controller()
            .await
            .view(Relation {
                schema: Some("public".into()),
                name: view_name.into(),
            })
            .await?;
        let results = getter.lookup(&[0.into()], true).await?;
        let mut results = results.into_vec();
        results.sort(); // Simple `lookup` does not sort the results, so we just sort them ourselves
        Ok(results)
    }

    #[track_caller]
    async fn assert_table_exists(&mut self, schema: &str, name: &str) {
        self.noria
            .table(Relation {
                schema: Some(schema.into()),
                name: name.into(),
            })
            .await
            .unwrap();
    }

    #[track_caller]
    async fn assert_table_missing(&mut self, schema: &str, name: &str) {
        self.noria
            .table(Relation {
                schema: Some(schema.into()),
                name: name.into(),
            })
            .await
            .unwrap_err();
    }
}

async fn replication_test_inner(url: &str) -> ReadySetResult<()> {
    let mut client = DbConnection::connect(url).await?;
    client.query(CREATE_SCHEMA).await?;
    client.query(POPULATE_SCHEMA).await?;

    let mut ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;

    ctx.check_results("noria_view", "Snapshot", SNAPSHOT_RESULT)
        .await?;

    for (test_name, test_query, test_results) in TESTS {
        client.query(test_query).await?;
        ctx.check_results("noria_view", test_name, test_results)
            .await?;
    }

    // Stop the replication task, issue some queries then check they are picked up after reconnect
    ctx.stop_repl().await;
    client.query(DISCONNECT_QUERY).await?;

    // Make sure no replication takes place for real
    ctx.check_results("noria_view", "Disconnected", TESTS[TESTS.len() - 1].2)
        .await?;

    // Resume replication
    ctx.start_repl(None).await?;
    ctx.check_results("noria_view", "Reconnect", RECONNECT_RESULT)
        .await?;

    client.stop().await;
    ctx.stop().await;

    Ok(())
}

fn pgsql_url() -> String {
    format!(
        "postgresql://postgres:noria@{}:{}/noria",
        env::var("PGHOST").unwrap_or_else(|_| "127.0.0.1".into()),
        env::var("PGPORT").unwrap_or_else(|_| "5432".into()),
    )
}

fn mysql_url() -> String {
    format!(
        "mysql://root:noria@{}:{}/public",
        env::var("MYSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        env::var("MYSQL_TCP_PORT").unwrap_or_else(|_| "3306".into()),
    )
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn pgsql_replication() -> ReadySetResult<()> {
    replication_test_inner(&pgsql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn mysql_replication() -> ReadySetResult<()> {
    replication_test_inner(&mysql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
#[slow]
async fn pgsql_replication_catch_up() {
    replication_catch_up_inner(&pgsql_url()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
#[slow]
async fn mysql_replication_catch_up() {
    replication_catch_up_inner(&mysql_url()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
#[slow]
async fn pgsql_replication_many_tables() {
    replication_many_tables_inner(&pgsql_url()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
#[slow]
async fn mysql_replication_many_tables() {
    replication_many_tables_inner(&mysql_url()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
#[slow]
async fn pgsql_replication_big_tables() {
    replication_big_tables_inner(&pgsql_url()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
#[slow]
async fn mysql_replication_big_tables() {
    replication_big_tables_inner(&mysql_url()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn mysql_datetime_replication() -> ReadySetResult<()> {
    mysql_datetime_replication_inner().await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn pgsql_skip_unparsable() -> ReadySetResult<()> {
    replication_skip_unparsable_inner(&pgsql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn mysql_skip_unparsable() -> ReadySetResult<()> {
    replication_skip_unparsable_inner(&mysql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn pgsql_replication_filter() -> ReadySetResult<()> {
    replication_filter_inner(&pgsql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn mysql_replication_filter() -> ReadySetResult<()> {
    replication_filter_inner(&mysql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn pgsql_replication_all_schemas() -> ReadySetResult<()> {
    replication_all_schemas_inner(&pgsql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn mysql_replication_all_schemas() -> ReadySetResult<()> {
    replication_all_schemas_inner(&mysql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn pgsql_replication_resnapshot() -> ReadySetResult<()> {
    resnapshot_inner(&pgsql_url()).await
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn mysql_replication_resnapshot() -> ReadySetResult<()> {
    resnapshot_inner(&mysql_url()).await
}

/// This test checks that when writes and replication happen in parallel
/// noria correctly catches up from binlog
/// NOTE: If this test flakes, please notify Vlad
async fn replication_catch_up_inner(url: &str) -> ReadySetResult<()> {
    const TOTAL_INSERTS: usize = 5000;
    static INSERTS_DONE: AtomicUsize = AtomicUsize::new(0);

    let mut client = DbConnection::connect(url).await?;
    client
        .query(
            "
            DROP TABLE IF EXISTS catch_up CASCADE;
            DROP TABLE IF EXISTS catch_up_pk CASCADE;
            DROP VIEW IF EXISTS catch_up_view;
            DROP VIEW IF EXISTS catch_up_pk_view;
            CREATE TABLE catch_up (
                id int,
                val varchar(255)
            );
            CREATE TABLE catch_up_pk (
                id int PRIMARY KEY,
                val varchar(255)
            );
            CREATE VIEW catch_up_view AS SELECT * FROM catch_up;
            CREATE VIEW catch_up_pk_view AS SELECT * FROM catch_up_pk;",
        )
        .await?;

    // Begin the inserter task before we begin replication. It should have sufficient
    // inserts to perform to last past the replication process. We use a keyless table
    // to make sure we not only get all of the inserts, but also all of the inserts are
    // processed exactly once
    let inserter: tokio::task::JoinHandle<ReadySetResult<DbConnection>> =
        tokio::spawn(async move {
            for idx in 0..TOTAL_INSERTS {
                client
                    .query("INSERT INTO catch_up VALUES (100, 'I am a teapot')")
                    .await?;

                client
                    .query(&format!(
                        "INSERT INTO catch_up_pk VALUES ({}, 'I am a teapot')",
                        idx
                    ))
                    .await?;

                INSERTS_DONE.fetch_add(1, Ordering::Relaxed);
            }

            Ok(client)
        });

    while INSERTS_DONE.load(Ordering::Relaxed) < TOTAL_INSERTS / 5 {
        // Sleep a bit to let some writes happen first
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let mut ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;
    let mut client = inserter.await.unwrap()?;

    let rs: Vec<_> = std::iter::repeat([DfValue::from(100), DfValue::from("I am a teapot")])
        .take(TOTAL_INSERTS)
        .collect();
    let rs: Vec<&[DfValue]> = rs.iter().map(|r| r.as_slice()).collect();
    ctx.check_results("catch_up_view", "Catch up", rs.as_slice())
        .await
        .unwrap();

    let rs: Vec<_> = (0..TOTAL_INSERTS)
        .map(|i| [DfValue::from(i as i32), DfValue::from("I am a teapot")])
        .collect();
    let rs: Vec<&[DfValue]> = rs.iter().map(|r| r.as_slice()).collect();
    ctx.check_results("catch_up_pk_view", "Catch up with pk", rs.as_slice())
        .await
        .unwrap();

    ctx.stop().await;

    client
        .query(
            "
        DROP TABLE IF EXISTS catch_up CASCADE;
        DROP TABLE IF EXISTS catch_up_pk CASCADE;
        DROP VIEW IF EXISTS catch_up_view;
        DROP VIEW IF EXISTS catch_up_pk_view;",
        )
        .await?;

    client.stop().await;

    Ok(())
}

async fn replication_many_tables_inner(url: &str) -> ReadySetResult<()> {
    const TOTAL_TABLES: usize = 300;
    let mut client = DbConnection::connect(url).await?;

    for t in 0..TOTAL_TABLES {
        let tbl_name = format!("t{}", t);
        client
            .query(&format!(
                "DROP TABLE IF EXISTS {0} CASCADE; CREATE TABLE {0} (id int); INSERT INTO {0} VALUES (1);",
                tbl_name
            ))
            .await?;
    }

    let ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;

    for t in 0..TOTAL_TABLES {
        // Just check that all of the tables are really there
        ctx.controller()
            .await
            .table(Relation {
                schema: Some("public".into()),
                name: format!("t{t}").into(),
            })
            .await
            .unwrap();
    }

    ctx.stop().await;

    for t in 0..TOTAL_TABLES {
        client
            .query(&format!("DROP TABLE IF EXISTS t{} CASCADE;", t))
            .await?;
    }

    client.stop().await;

    Ok(())
}

// This test will definitely trigger the global timeout if a session one is not set
async fn replication_big_tables_inner(url: &str) -> ReadySetResult<()> {
    const TOTAL_TABLES: usize = 2;
    const TOTAL_ROWS: usize = 10_000;

    let mut client = DbConnection::connect(url).await?;

    for t in 0..TOTAL_TABLES {
        let tbl_name = format!("t{t}");
        client
            .query(&format!(
                "DROP TABLE IF EXISTS {tbl_name} CASCADE; CREATE TABLE {tbl_name} (id int);",
            ))
            .await?;
        for r in 0..TOTAL_ROWS {
            client
                .query(&format!("INSERT INTO {tbl_name} VALUES ({r})"))
                .await?;
        }
    }

    let ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;

    for t in 0..TOTAL_TABLES {
        // Just check that all of the tables are really there
        ctx.controller()
            .await
            .table(Relation {
                schema: Some("public".into()),
                name: format!("t{t}").into(),
            })
            .await
            .unwrap();
    }

    ctx.stop().await;

    for t in 0..TOTAL_TABLES {
        client
            .query(&format!("DROP TABLE IF EXISTS t{t} CASCADE;"))
            .await?;
    }

    client.stop().await;

    Ok(())
}

async fn mysql_datetime_replication_inner() -> ReadySetResult<()> {
    let url = &mysql_url();
    let mut client = DbConnection::connect(url).await?;
    client
        .query(
            "
            DROP TABLE IF EXISTS `dt_test` CASCADE;
            DROP VIEW IF EXISTS dt_test_view;
            CREATE TABLE `dt_test` (
                id int NOT NULL PRIMARY KEY,
                dt datetime,
                ts timestamp,
                d date,
                t time
            );
            CREATE VIEW dt_test_view AS SELECT * FROM `dt_test` ORDER BY id ASC",
        )
        .await?;

    // Allow invalid values for dates
    client.query("SET @@sql_mode := ''").await?;
    client
        .query(
            "INSERT INTO `dt_test` VALUES
                (0, '0000-00-00', '0000-00-00', '0000-00-00', '25:27:89'),
                (1, '0002-00-00', '0020-00-00', '0200-00-00', '14:27:89')",
        )
        .await?;

    let mut ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;

    // TODO: Those are obviously not the right answers, but at least we don't panic
    ctx.check_results(
        "dt_test_view",
        "Snapshot",
        &[
            &[
                DfValue::Int(0),
                DfValue::None,
                DfValue::None,
                DfValue::None,
                DfValue::Time(MysqlTime::from_hmsus(false, 0, 0, 0, 0)),
            ],
            &[
                DfValue::Int(1),
                DfValue::None,
                DfValue::None,
                DfValue::None,
                DfValue::Time(MysqlTime::from_hmsus(false, 0, 0, 0, 0)),
            ],
        ],
    )
    .await?;

    // Repeat, but this time using binlog replication
    client
        .query(
            "INSERT INTO `dt_test` VALUES
                (2, '0000-00-00', '0000-00-00', '0000-00-00', '25:27:89'),
                (3, '0002-00-00', '0020-00-00', '0200-00-00', '14:27:89')",
        )
        .await?;

    ctx.check_results(
        "dt_test_view",
        "Replication",
        &[
            &[
                DfValue::Int(0),
                DfValue::None,
                DfValue::None,
                DfValue::None,
                DfValue::Time(MysqlTime::from_hmsus(false, 0, 0, 0, 0)),
            ],
            &[
                DfValue::Int(1),
                DfValue::None,
                DfValue::None,
                DfValue::None,
                DfValue::Time(MysqlTime::from_hmsus(false, 0, 0, 0, 0)),
            ],
            &[
                DfValue::Int(2),
                DfValue::None,
                DfValue::None,
                DfValue::None,
                DfValue::Time(MysqlTime::from_hmsus(false, 0, 0, 0, 0)),
            ],
            &[
                DfValue::Int(3),
                DfValue::None,
                DfValue::None,
                DfValue::None,
                DfValue::Time(MysqlTime::from_hmsus(false, 0, 0, 0, 0)),
            ],
        ],
    )
    .await?;

    client.stop().await;
    ctx.stop().await;
    Ok(())
}

async fn replication_skip_unparsable_inner(url: &str) -> ReadySetResult<()> {
    readyset_tracing::init_test_logging();
    let mut client = DbConnection::connect(url).await?;

    client
        .query(
            "
            DROP TABLE IF EXISTS t1 CASCADE; CREATE TABLE t1 (id polygon);
            DROP TABLE IF EXISTS t2 CASCADE; CREATE TABLE t2 (id int);
            DROP VIEW IF EXISTS t1_view; CREATE VIEW t1_view AS SELECT * FROM t1;
            DROP VIEW IF EXISTS t2_view; CREATE VIEW t2_view AS SELECT * FROM t2;
            INSERT INTO t2 VALUES (1),(2),(3);
            ",
        )
        .await?;

    let mut ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;

    ctx.check_results(
        "t2_view",
        "skip_unparsable",
        &[&[DfValue::Int(1)], &[DfValue::Int(2)], &[DfValue::Int(3)]],
    )
    .await?;

    ctx.noria
        .table("t1")
        .await
        .expect_err("Table should be unparsable");

    ctx.noria
        .view("t1_view")
        .await
        .expect_err("Can't have view for nonexistent table");

    client
        .query(
            "
            DROP TABLE IF EXISTS t3 CASCADE; CREATE TABLE t3 (id polygon);
            DROP VIEW IF EXISTS t3_view; CREATE VIEW t3_view AS SELECT * FROM t3;
            INSERT INTO t2 VALUES (4),(5),(6);
            ",
        )
        .await?;

    ctx.check_results(
        "t2_view",
        "skip_unparsable",
        &[
            &[DfValue::Int(1)],
            &[DfValue::Int(2)],
            &[DfValue::Int(3)],
            &[DfValue::Int(4)],
            &[DfValue::Int(5)],
            &[DfValue::Int(6)],
        ],
    )
    .await?;

    ctx.stop().await;
    client.stop().await;

    Ok(())
}

async fn replication_filter_inner(url: &str) -> ReadySetResult<()> {
    readyset_tracing::init_test_logging();

    let mut client = DbConnection::connect(url).await?;

    let cascade = if url.starts_with("postgresql") {
        "CASCADE"
    } else {
        ""
    };

    let query = format!(
        "
    DROP TABLE IF EXISTS t1 CASCADE; CREATE TABLE t1 (id int);
    DROP TABLE IF EXISTS t2 CASCADE; CREATE TABLE t2 (id int);
    DROP TABLE IF EXISTS t3 CASCADE; CREATE TABLE t3 (id int);
    DROP SCHEMA IF EXISTS noria2 {cascade}; CREATE SCHEMA noria2; CREATE TABLE noria2.t4 (id int);
    DROP SCHEMA IF EXISTS noria3 {cascade}; CREATE SCHEMA noria3;
    DROP VIEW IF EXISTS t1_view; CREATE VIEW t1_view AS SELECT * FROM t1;
    DROP VIEW IF EXISTS t2_view; CREATE VIEW t2_view AS SELECT * FROM t2;
    DROP VIEW IF EXISTS t3_view; CREATE VIEW t3_view AS SELECT * FROM t3;
    "
    );

    client.query(&query).await?;

    let mut ctx = TestHandle::start_noria(
        url.to_string(),
        Some(Config {
            replication_tables: Some(
                "public.t3, public.t1, noria3.*, noria2.t4"
                    .to_string()
                    .into(),
            ),
            ..Default::default()
        }),
    )
    .await?;

    ctx.ready_notify.as_ref().unwrap().notified().await;
    ctx.assert_table_exists("public", "t1").await;
    ctx.assert_table_missing("public", "t2").await;
    ctx.assert_table_exists("public", "t3").await;
    ctx.assert_table_exists("noria2", "t4").await;

    client
        .query(
            "
            CREATE TABLE noria2.t5 (id int);
            CREATE TABLE noria3.t6 (id int);
            INSERT INTO t1 VALUES (1),(2),(3);
            INSERT INTO t2 VALUES (1),(2),(3);
            INSERT INTO t3 VALUES (1),(2),(3);
            INSERT INTO noria2.t4 VALUES (1),(2),(3);
            ",
        )
        .await?;

    ctx.noria
        .extend_recipe(
            ChangeList::from_str("CREATE VIEW public.t4_view AS SELECT * FROM noria2.t4;").unwrap(),
        )
        .await
        .unwrap();

    for view in ["t1_view", "t3_view", "t4_view"] {
        ctx.check_results(
            view,
            "replication_filter",
            &[&[DfValue::Int(1)], &[DfValue::Int(2)], &[DfValue::Int(3)]],
        )
        .await?;
    }

    ctx.noria
        .view("t2")
        .await
        .expect_err("View should not exist, since viewed table does not exist");

    ctx.assert_table_missing("public", "t5").await;
    ctx.assert_table_exists("noria3", "t6").await;

    ctx.stop().await;
    client.stop().await;

    Ok(())
}

async fn replication_all_schemas_inner(url: &str) -> ReadySetResult<()> {
    readyset_tracing::init_test_logging();
    let mut client = DbConnection::connect(url).await?;

    let cascade = if url.starts_with("postgresql") {
        "CASCADE"
    } else {
        ""
    };

    let query = format!(
        "
    DROP TABLE IF EXISTS t1 CASCADE; CREATE TABLE t1 (id int);
    DROP TABLE IF EXISTS t2 CASCADE; CREATE TABLE t2 (id int);
    DROP TABLE IF EXISTS t3 CASCADE; CREATE TABLE t3 (id int);
    DROP SCHEMA IF EXISTS noria2 {cascade}; CREATE SCHEMA noria2; CREATE TABLE noria2.t4 (id int);
    DROP SCHEMA IF EXISTS noria3 {cascade}; CREATE SCHEMA noria3;
    "
    );

    client.query(&query).await?;

    let mut ctx = TestHandle::start_noria(
        url.to_string(),
        Some(Config {
            replication_tables: Some("*.*".to_string().into()),
            ..Default::default()
        }),
    )
    .await?;

    ctx.ready_notify.as_ref().unwrap().notified().await;
    ctx.assert_table_exists("public", "t1").await;
    ctx.assert_table_exists("public", "t2").await;
    ctx.assert_table_exists("public", "t3").await;
    ctx.assert_table_exists("noria2", "t4").await;

    client
        .query(
            "
            CREATE TABLE noria2.t5 (id int);
            CREATE TABLE noria3.t6 (id int);
            INSERT INTO t1 VALUES (1),(2),(3);
            INSERT INTO t2 VALUES (1),(2),(3);
            INSERT INTO t3 VALUES (1),(2),(3);
            INSERT INTO noria2.t4 VALUES (1),(2),(3);
            ",
        )
        .await?;

    ctx.noria
        .extend_recipe(
            ChangeList::from_str("CREATE VIEW public.t4_view AS SELECT * FROM noria2.t4;").unwrap(),
        )
        .await
        .unwrap();

    ctx.check_results(
        "t4_view",
        "replication_filter",
        &[&[DfValue::Int(1)], &[DfValue::Int(2)], &[DfValue::Int(3)]],
    )
    .await?;

    ctx.assert_table_missing("public", "t5").await;
    ctx.assert_table_exists("noria3", "t6").await;

    ctx.stop().await;
    client.stop().await;

    Ok(())
}

/// Tests that on encountering an ALTER TABLE statement the replicator does a proper resnapshot that
/// results in the proper schema being present.
async fn resnapshot_inner(url: &str) -> ReadySetResult<()> {
    let mut client = DbConnection::connect(url).await?;
    client
        .query(
            "
            DROP TABLE IF EXISTS repl1 CASCADE;
            DROP TABLE IF EXISTS repl2 CASCADE;
            DROP VIEW IF EXISTS repl1_view;
            DROP VIEW IF EXISTS repl2_view;
            CREATE TABLE repl1 (
                id int,
                val varchar(255)
            );
            CREATE TABLE repl2 (
                id int,
                val varchar(255)
            );
            CREATE VIEW repl1_view AS SELECT * FROM repl1;
            CREATE VIEW repl2_view AS SELECT * FROM repl2;",
        )
        .await?;

    const ROWS: usize = 20;

    // Populate both tables
    for i in 0..ROWS {
        client
            .query(&format!("INSERT INTO repl1 VALUES ({i}, 'I am a teapot')"))
            .await?;
        client
            .query(&format!("INSERT INTO repl2 VALUES ({i}, 'I am a teapot')"))
            .await?;
    }

    let mut ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;
    // Initial snapshot is done
    let rs: Vec<_> = (0..ROWS)
        .map(|i| [DfValue::from(i as i32), DfValue::from("I am a teapot")])
        .collect();
    let rs: Vec<&[DfValue]> = rs.iter().map(|r| r.as_slice()).collect();
    ctx.check_results("repl1_view", "Resnapshot initial", rs.as_slice())
        .await
        .unwrap();
    ctx.check_results("repl2_view", "Resnapshot initial", rs.as_slice())
        .await
        .unwrap();

    // Issue a few ALTER TABLE statements
    client.query("ALTER TABLE repl2 ADD COLUMN x int").await?;
    client.query("ALTER TABLE repl2 ADD COLUMN y int").await?;
    // Add some more rows in between
    for i in 0..ROWS {
        client
            .query(&format!("INSERT INTO repl1 VALUES ({i}, 'I am a teapot')"))
            .await?;
        client
            .query(&format!(
                "INSERT INTO repl2 VALUES ({i}, 'I am a teapot', {i}, {i})"
            ))
            .await?;
    }
    client.query("ALTER TABLE repl2 DROP COLUMN x").await?;

    // Check everything adds up for both the altered and unaltered tables
    let rs: Vec<_> = (0..ROWS)
        .map(|i| {
            [
                [DfValue::from(i as i32), DfValue::from("I am a teapot")],
                [DfValue::from(i as i32), DfValue::from("I am a teapot")],
            ]
        })
        .flatten()
        .collect();
    let rs: Vec<&[DfValue]> = rs.iter().map(|r| r.as_slice()).collect();
    ctx.check_results("repl1_view", "Resnapshot repl1", rs.as_slice())
        .await
        .unwrap();

    // TODO(fran): In theory the view should have been recreated properly, and this step should be
    // redundant
    client.query("DROP VIEW repl2_view").await?;
    client
        .query("CREATE VIEW repl2_view AS SELECT * FROM repl2")
        .await?;

    let rs: Vec<_> = (0..ROWS)
        .map(|i| {
            [
                [
                    DfValue::from(i as i32),
                    DfValue::from("I am a teapot"),
                    DfValue::None,
                ],
                [
                    DfValue::from(i as i32),
                    DfValue::from("I am a teapot"),
                    DfValue::from(i as i32),
                ],
            ]
        })
        .flatten()
        .collect();
    let rs: Vec<&[DfValue]> = rs.iter().map(|r| r.as_slice()).collect();
    ctx.check_results("repl2_view", "Resnapshot repl2", rs.as_slice())
        .await
        .unwrap();

    ctx.stop().await;

    client
        .query(
            "DROP TABLE IF EXISTS repl1 CASCADE;
             DROP TABLE IF EXISTS repl2 CASCADE;
             DROP VIEW IF EXISTS repl1_view;
             DROP VIEW IF EXISTS repl2_view;",
        )
        .await?;

    client.stop().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn mysql_enum_replication() -> ReadySetResult<()> {
    let url = &mysql_url();
    let mut client = DbConnection::connect(url).await?;
    client
        .query(
            "
            DROP TABLE IF EXISTS `enum_test` CASCADE;
            DROP VIEW IF EXISTS enum_test_view;
            CREATE TABLE `enum_test` (
                id int NOT NULL PRIMARY KEY,
                e enum('red', 'yellow', 'green')
            );
            CREATE VIEW enum_test_view AS SELECT * FROM `enum_test` ORDER BY id ASC",
        )
        .await?;

    // Allow invalid values for enums
    client.query("SET @@sql_mode := ''").await?;
    client
        .query(
            "
            INSERT INTO enum_test VALUES
                (0, 'green'),
                (1, 'yellow'),
                (2, 'purple')",
        )
        .await?;

    let mut ctx = TestHandle::start_noria(url.to_string(), None).await?;
    ctx.ready_notify.as_ref().unwrap().notified().await;

    ctx.check_results(
        "enum_test_view",
        "Snapshot",
        &[
            &[DfValue::Int(0), DfValue::Int(3)],
            &[DfValue::Int(1), DfValue::Int(2)],
            &[DfValue::Int(2), DfValue::Int(0)],
        ],
    )
    .await?;

    // Repeat, but this time using binlog replication
    client
        .query(
            "
            INSERT INTO enum_test VALUES
                (3, 'red'),
                (4, 'yellow')",
        )
        .await?;

    ctx.check_results(
        "enum_test_view",
        "Replication",
        &[
            &[DfValue::Int(0), DfValue::Int(3)],
            &[DfValue::Int(1), DfValue::Int(2)],
            &[DfValue::Int(2), DfValue::Int(0)],
            &[DfValue::Int(3), DfValue::Int(1)],
            &[DfValue::Int(4), DfValue::Int(2)],
        ],
    )
    .await?;

    client.stop().await;
    ctx.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn postgresql_ddl_replicate_drop_table() {
    readyset_tracing::init_test_logging();
    let mut client = DbConnection::connect(&pgsql_url()).await.unwrap();
    client
        .query("DROP TABLE IF EXISTS t1 CASCADE; CREATE TABLE t1 (id int);")
        .await
        .unwrap();
    let mut ctx = TestHandle::start_noria(pgsql_url(), None).await.unwrap();
    ctx.ready_notify.as_ref().unwrap().notified().await;
    ctx.noria
        .table(Relation {
            schema: Some("public".into()),
            name: "t1".into(),
        })
        .await
        .unwrap();

    trace!("Dropping table");
    client.query("DROP TABLE t1 CASCADE;").await.unwrap();

    eventually! {
        let res = ctx
            .noria
            .table(Relation {
                schema: Some("public".into()),
                name: "t1".into(),
            })
            .await;
        matches!(
            res.err(),
            Some(ReadySetError::RpcFailed { source, .. })
                if matches!(&*source, ReadySetError::TableNotFound{
                    name,
                    schema: Some(schema)
                } if name == "t1" && schema == "public")
        )
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn postgresql_ddl_replicate_create_table() {
    readyset_tracing::init_test_logging();
    let mut client = DbConnection::connect(&pgsql_url()).await.unwrap();
    client
        .query("DROP TABLE IF EXISTS t2 CASCADE")
        .await
        .unwrap();
    let mut ctx = TestHandle::start_noria(pgsql_url(), None).await.unwrap();
    ctx.ready_notify.as_ref().unwrap().notified().await;

    trace!("Creating table");
    client.query("CREATE TABLE t2 (id int);").await.unwrap();

    eventually!(ctx
        .noria
        .table(Relation {
            schema: Some("public".into()),
            name: "t2".into(),
        })
        .await
        .is_ok());
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn postgresql_ddl_replicate_drop_view() {
    readyset_tracing::init_test_logging();
    let mut client = DbConnection::connect(&pgsql_url()).await.unwrap();
    client
        .query(
            "DROP TABLE IF EXISTS t2 CASCADE; CREATE TABLE t2 (id int);
             DROP VIEW IF EXISTS t2_view; CREATE VIEW t2_view AS SELECT * FROM t2;",
        )
        .await
        .unwrap();
    let mut ctx = TestHandle::start_noria(pgsql_url(), None).await.unwrap();
    ctx.ready_notify.as_ref().unwrap().notified().await;
    ctx.noria
        .table(Relation {
            schema: Some("public".into()),
            name: "t2".into(),
        })
        .await
        .unwrap();
    ctx.noria
        .view(Relation {
            schema: Some("public".into()),
            name: "t2_view".into(),
        })
        .await
        .unwrap();

    trace!("Dropping view");
    client.query("DROP VIEW t2_view;").await.unwrap();

    eventually! {
        let res = ctx.noria.view("t2_view").await;
        matches!(
            res.err(),
            Some(ReadySetError::ViewNotFound(view)) if view == "`t2_view`"
        )
    };
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn postgresql_ddl_replicate_create_view() {
    readyset_tracing::init_test_logging();
    let mut client = DbConnection::connect(&pgsql_url()).await.unwrap();
    client
        .query(
            "DROP TABLE IF EXISTS t2 CASCADE; CREATE TABLE t2 (id int);
            DROP VIEW IF EXISTS t2_view",
        )
        .await
        .unwrap();
    let mut ctx = TestHandle::start_noria(pgsql_url(), None).await.unwrap();
    ctx.ready_notify.as_ref().unwrap().notified().await;
    ctx.noria
        .table(Relation {
            schema: Some("public".into()),
            name: "t2".into(),
        })
        .await
        .unwrap();

    trace!("CREATING view");
    client
        .query("CREATE VIEW t2_view AS SELECT * FROM t2;")
        .await
        .unwrap();

    eventually!(ctx
        .noria
        .view(Relation {
            schema: Some("public".into()),
            name: "t2_view".into(),
        })
        .await
        .is_ok());
}
