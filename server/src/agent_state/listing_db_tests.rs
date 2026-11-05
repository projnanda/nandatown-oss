//! What a listing fetches from the database, which the HTTP surface cannot show: a page answers
//! the same whether its rows past the byte budget were fetched and dropped or never fetched.
//!
//! Needs a disposable PostgreSQL (an admin URL; a database is created and dropped per test):
//!   TEST_PG_URL=... cargo test -p nasiko-server --lib agent_state::listing_db_tests -- --ignored

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::payload::sealed_bytes_bound;
use super::repo::{self, KvQuery, TaskQuery};
use crate::state::AppState;

/// Stored size of every row these tests write: large enough that a page size's worth of them
/// would be many times the budget.
const ROW_SEALED_BYTES: i64 = 512 * 1024;
/// Rows written, more than the bound lets one page fetch.
const ROWS: i64 = 20;
/// The page's payload budget: a little over two rows' worth of stored bytes once sealed.
const PAYLOAD_BUDGET: usize = 1024 * 1024;
const PAGE_SIZE: i64 = 50;

struct Scratch {
    admin: PgPool,
    db: PgPool,
    name: String,
}

impl Scratch {
    async fn create() -> Self {
        let admin_url =
            std::env::var("TEST_PG_URL").expect("TEST_PG_URL names a disposable server");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect to TEST_PG_URL");
        let name = format!("nasiko_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .expect("create scratch database");
        let base = admin_url
            .rsplit_once('/')
            .map_or(admin_url.as_str(), |(base, _)| base);
        let db = PgPool::connect(&format!("{base}/{name}"))
            .await
            .expect("connect to scratch database");
        AppState::run_migrations(&db).await;
        Self { admin, db, name }
    }

    async fn drop_database(self) {
        self.db.close().await;
        sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ))
        .execute(&self.admin)
        .await
        .expect("drop scratch database");
    }
}

async fn seed_agent(db: &PgPool) -> Uuid {
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO users (username, email) VALUES ('lister', 'lister@test.local') RETURNING id",
    )
    .fetch_one(db)
    .await
    .expect("seed user");
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, image, status) \
         VALUES ('lister', $1, 'nasiko/echo:1.0.0', 'running') RETURNING id",
    )
    .bind(owner)
    .fetch_one(db)
    .await
    .expect("seed agent")
}

/// `ROWS` tasks of `ROW_SEALED_BYTES` stored bytes each, newest first by name. The listing reads
/// stored sizes only, so the stored text need not be a real ciphertext.
async fn seed_large_tasks(db: &PgPool, agent_id: Uuid) {
    sqlx::query(
        "INSERT INTO agent_tasks (agent_id, task_id, state, body, version, updated_at, expires_at)
         SELECT $1, 't' || lpad(i::text, 2, '0'), 'working', repeat('x', $2), 1,
                now() - make_interval(secs => i), now() + interval '1 day'
         FROM generate_series(1, $3) AS i",
    )
    .bind(agent_id)
    .bind(i32::try_from(ROW_SEALED_BYTES).unwrap())
    .bind(ROWS)
    .execute(db)
    .await
    .expect("seed tasks");
}

async fn seed_large_entries(db: &PgPool, agent_id: Uuid) {
    sqlx::query(
        "INSERT INTO agent_kv (agent_id, key, value, version, expires_at)
         SELECT $1, 'k' || lpad(i::text, 2, '0'), repeat('x', $2), 1, now() + interval '1 day'
         FROM generate_series(1, $3) AS i",
    )
    .bind(agent_id)
    .bind(i32::try_from(ROW_SEALED_BYTES).unwrap())
    .bind(ROWS)
    .execute(db)
    .await
    .expect("seed entries");
}

/// How many rows the bound lets one page fetch: every row whose predecessors' stored bytes are
/// within it.
fn rows_within_bound(bound: i64) -> usize {
    usize::try_from(bound / ROW_SEALED_BYTES + 1).unwrap()
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn a_task_listing_of_large_rows_fetches_only_what_the_byte_bound_allows() {
    let scratch = Scratch::create().await;
    let agent_id = seed_agent(&scratch.db).await;
    seed_large_tasks(&scratch.db, agent_id).await;
    let query = TaskQuery {
        context_id: None,
        state: None,
        after: None,
        page_size: PAGE_SIZE,
    };
    let bound = sealed_bytes_bound(PAYLOAD_BUDGET, PAGE_SIZE);

    let fetched = repo::list_tasks(&scratch.db, agent_id, &query, bound)
        .await
        .unwrap();

    assert_eq!(fetched.rows.len(), rows_within_bound(bound));
    assert!(fetched.rows.len() < usize::try_from(ROWS).unwrap());
    assert!(fetched.has_more, "the rows left out come on the next page");
    assert_eq!(fetched.rows[0].task_id, "t01", "in listing order");

    // A bound smaller than one row still fetches that row, alone, so a listing moves on.
    let alone = repo::list_tasks(&scratch.db, agent_id, &query, 1)
        .await
        .unwrap();
    let names: Vec<_> = alone.rows.iter().map(|row| row.task_id.as_str()).collect();
    assert_eq!(names, ["t01"]);
    assert!(alone.has_more);
    scratch.drop_database().await;
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn a_kv_listing_of_large_rows_fetches_only_what_the_byte_bound_allows() {
    let scratch = Scratch::create().await;
    let agent_id = seed_agent(&scratch.db).await;
    seed_large_entries(&scratch.db, agent_id).await;
    let query = KvQuery {
        prefix: None,
        after: None,
        page_size: PAGE_SIZE,
    };
    let bound = sealed_bytes_bound(PAYLOAD_BUDGET, PAGE_SIZE);

    let fetched = repo::list_kv(&scratch.db, agent_id, &query, bound)
        .await
        .unwrap();

    assert_eq!(fetched.rows.len(), rows_within_bound(bound));
    assert!(fetched.has_more);
    assert_eq!(fetched.rows[0].key, "k01");
    scratch.drop_database().await;
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn without_a_byte_bound_the_page_size_decides_and_the_last_page_says_so() {
    let scratch = Scratch::create().await;
    let agent_id = seed_agent(&scratch.db).await;
    seed_large_entries(&scratch.db, agent_id).await;
    let bound = i64::MAX;

    let page = |page_size| KvQuery {
        prefix: None,
        after: None,
        page_size,
    };
    let full = repo::list_kv(&scratch.db, agent_id, &page(5), bound)
        .await
        .unwrap();
    assert_eq!(full.rows.len(), 5);
    assert!(full.has_more, "the probe row was found");

    let all = repo::list_kv(&scratch.db, agent_id, &page(ROWS), bound)
        .await
        .unwrap();
    assert_eq!(all.rows.len(), usize::try_from(ROWS).unwrap());
    assert!(!all.has_more, "nothing follows the last row");
    scratch.drop_database().await;
}
