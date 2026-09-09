//! Concrete backend dispatch and the host-owned write transaction boundary.
use super::{Result, db};

// Preserve rollback failures instead of hiding them behind the original error.
pub(super) async fn finish_transaction<DB: sqlx::Database, T>(
    transaction: sqlx::Transaction<'_, DB>,
    result: Result<T>,
) -> Result<T> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(db)?;
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().await.map_err(db)?;
            Err(error)
        }
    }
}

// Expand queries for each concrete driver; SQLx's Any driver is not a dialect
// abstraction. Callers hold the read barrier and perform their health checks.
macro_rules! rows {
    ($store:expr, $ty:ty, $sql:expr $(, $bind:expr)* $(,)?) => {{
        match &$store.inner.backend {
            Backend::Sqlite { pool, .. } => sqlx::query_as::<_, $ty>($sql)
                $(.bind($bind))*
                .fetch_all(pool)
                .await
                .map_err(db)?,
            Backend::Postgres { pool, .. } => sqlx::query_as::<_, $ty>($sql)
                $(.bind($bind))*
                .fetch_all(pool)
                .await
                .map_err(db)?,
        }
    }};
}

// Keep every guard alive through commit/rollback. In particular, PostgreSQL
// writes must run on the advisory-lock owner connection, never a pooled writer.
macro_rules! write_tx {
    ($store:expr, $tx:ident, $body:block) => {{
        let _barrier = $store.inner.barrier.read().await;
        $store.ensure_open()?;
        let _writer = match &$store.inner.writer {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        match &$store.inner.backend {
            Backend::Sqlite { pool, .. } => {
                let mut $tx = pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
                let result: Result<_> = async $body.await;
                $crate::transactions::finish_transaction($tx, result).await
            }
            Backend::Postgres { lease, .. } => {
                let mut owner = lease.lock().await;
                let conn = owner
                    .as_mut()
                    .ok_or_else(|| err(ErrorCode::StorageUnavailable))?;
                pg_health(&$store.inner, conn).await?;
                let mut $tx = conn.begin().await.map_err(db)?;
                let result: Result<_> = async $body.await;
                $crate::transactions::finish_transaction($tx, result).await
            }
        }
    }};
}
