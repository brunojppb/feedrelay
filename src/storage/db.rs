use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

/// Build a SQLite connection pool from the given `database_url`.
pub async fn build_pool(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let connect_options = SqliteConnectOptions::from_str(database_url)?
        // Create the database file on first boot if it does not already exist.
        .create_if_missing(true)
        // WAL mode allows concurrent readers alongside a single writer, but SQLite
        // still serialises all writers. Tasks 5 and 6 will add concurrent actors —
        // keep this in mind when designing their write paths.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(connect_options)
        .await?;

    Ok(pool)
}

/// Run pending migrations from the `./migrations/` directory.
///
/// We share the SQLite database with Apalis (which has its own migration set).
/// Because both use the same `_sqlx_migrations` tracking table, we must set
/// `ignore_missing = true` so that sqlx skips version-missing validation for
/// Apalis's migration versions that are present in the table but not in our
/// migration directory.
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(pool)
        .await
}

#[cfg(test)]
pub(crate) async fn test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("failed to open in-memory pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations");
    pool
}

/// Like [`test_pool`] but also runs Apalis storage setup (required for tests
/// that enqueue jobs).  Apalis migrations must be applied before ours because
/// they share the same `_sqlx_migrations` table and Apalis expects version 1
/// to be absent when it first runs.
#[cfg(test)]
pub(crate) async fn test_pool_with_jobs() -> SqlitePool {
    use apalis_sqlite::SqliteStorage;

    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("failed to open in-memory pool");

    // Apalis first (matches main.rs boot order).
    SqliteStorage::<(), (), ()>::setup(&pool)
        .await
        .expect("failed to run apalis migrations in test");

    // ignore_missing = true: skip validation of Apalis versions that are in
    // the _sqlx_migrations table but unknown to our migration set.
    sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(&pool)
        .await
        .expect("failed to run app migrations in test");

    pool
}
