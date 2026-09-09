use super::*;
use oracle_core::WorkflowRepository;
const MAX_VALUE: usize = 64 * 1024;
const MAX_PAGE: usize = 512 * 1024;
fn key_valid(key: &str) -> bool {
    !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control)
}
fn decode(row: (String, i64, String)) -> Result<WorkflowRecord> {
    Ok(WorkflowRecord {
        key: row.0,
        revision: u64::try_from(row.1).map_err(integrity)?,
        value: serde_json::from_str(&row.2).map_err(integrity)?,
    })
}
#[async_trait]
impl WorkflowRepository for Storage {
    async fn workflow_get(
        &self,
        guild: &GuildId,
        kind: WorkflowKind,
        key: &str,
    ) -> Result<Option<WorkflowRecord>> {
        if !key_valid(key) {
            return Err(err(ErrorCode::InvalidInput));
        }
        let _barrier = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let rows = rows!(
            self,
            (String, i64, String),
            "SELECT key,revision,value FROM oracle_workflows WHERE guild=$1 AND kind=$2 AND key=$3",
            guild.as_str(),
            kind.as_str(),
            key
        );
        rows.into_iter().next().map(decode).transpose()
    }
    async fn workflow_put(
        &self,
        guild: &GuildId,
        kind: WorkflowKind,
        key: &str,
        expected_revision: Option<u64>,
        value: &Value,
    ) -> Result<WorkflowRecord> {
        let encoded = serde_json::to_string(value).map_err(integrity)?;
        if !key_valid(key) || encoded.len() > MAX_VALUE || expected_revision == Some(0) {
            return Err(err(ErrorCode::InvalidInput));
        }
        let expected = expected_revision.map(revision).transpose()?;
        let next = expected
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| err(ErrorCode::InvalidInput))?;
        write_tx!(self, tx, {
            let found: Option<(String,)> =
                sqlx::query_as("SELECT id FROM oracle_guilds WHERE id=$1")
                    .bind(guild.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db)?;
            if found.is_none() {
                return Err(err(ErrorCode::NotFound));
            }
            let affected = if let Some(expected) = expected {
                sqlx::query("UPDATE oracle_workflows SET value=$1,revision=$2 WHERE guild=$3 AND kind=$4 AND key=$5 AND revision=$6").bind(&encoded).bind(next).bind(guild.as_str()).bind(kind.as_str()).bind(key).bind(expected).execute(&mut *tx).await.map_err(db)?.rows_affected()
            } else {
                sqlx::query("INSERT INTO oracle_workflows(guild,kind,key,revision,value) VALUES($1,$2,$3,1,$4) ON CONFLICT DO NOTHING").bind(guild.as_str()).bind(kind.as_str()).bind(key).bind(&encoded).execute(&mut *tx).await.map_err(db)?.rows_affected()
            };
            if affected != 1 {
                return Err(err(ErrorCode::Conflict));
            }
            Ok(WorkflowRecord {
                key: key.to_owned(),
                revision: next as u64,
                value: value.clone(),
            })
        })
    }
    async fn workflow_list(
        &self,
        guild: &GuildId,
        kind: WorkflowKind,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<WorkflowRecord>> {
        if !(1..=100).contains(&limit) || after.is_some_and(|key| !key_valid(key)) {
            return Err(err(ErrorCode::InvalidInput));
        }
        let _barrier = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let rows = rows!(
            self,
            (String, i64, String),
            "SELECT key,revision,value FROM oracle_workflows WHERE guild=$1 AND kind=$2 AND key>$3 ORDER BY key LIMIT $4",
            guild.as_str(),
            kind.as_str(),
            after.unwrap_or(""),
            i64::from(limit)
        );
        let mut result = Vec::new();
        let mut bytes = 2;
        for row in rows {
            let record = decode(row)?;
            let size = serde_json::to_vec(&record).map_err(integrity)?.len()
                + usize::from(!result.is_empty());
            if bytes + size > MAX_PAGE {
                break;
            }
            bytes += size;
            result.push(record);
        }
        Ok(result)
    }
}
