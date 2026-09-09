//! Host-scoped module inventory, durable desire, documents and migration checkpoints.
use super::*;
use oracle_core::ModuleRepository;
use std::collections::BTreeSet;
const MAX_WRITES: usize = 100;
const MAX_DOC: usize = 64 * 1024;
const MAX_PAGE: usize = 512 * 1024;
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.contains('\0')
}
fn checked_writes(writes: &[DocumentWrite], empty: bool) -> Result<()> {
    if (!empty && writes.is_empty())
        || writes.len() > MAX_WRITES
        || serde_json::to_vec(writes).map_err(integrity)?.len() > MAX_PAGE
    {
        return Err(err(ErrorCode::InvalidInput));
    }
    let mut keys = BTreeSet::new();
    for write in writes {
        if !name(&write.collection)
            || !name(&write.key)
            || !keys.insert((&write.collection, &write.key))
            || write.expected_revision == Some(0)
            || (write.value.is_none() && write.expected_revision.is_none())
        {
            return Err(err(ErrorCode::InvalidInput));
        }
        if let Some(value) = &write.value
            && serde_json::to_vec(value).map_err(integrity)?.len() > MAX_DOC
        {
            return Err(err(ErrorCode::InvalidInput));
        }
    }
    Ok(())
}
type DocRow = (String, String, String, i64);
type ProgressRow = (i64, Option<i64>, Option<String>, Option<String>);
fn progress(module: &ModuleId, guild: &GuildId, row: ProgressRow) -> Result<MigrationProgress> {
    Ok(MigrationProgress {
        module: module.clone(),
        guild: guild.clone(),
        data_version: u32::try_from(row.0).map_err(integrity)?,
        target_version: row.1.map(u32::try_from).transpose().map_err(integrity)?,
        artifact_digest: row.2,
        cursor: row.3,
    })
}
fn doc((collection, key, value, revision): DocRow) -> Result<ModuleDocument> {
    Ok(ModuleDocument {
        collection,
        key,
        value: serde_json::from_str(&value).map_err(integrity)?,
        revision: u64::try_from(revision).map_err(integrity)?,
    })
}
// The same scoped CAS writes serve normal batches and migration checkpoints.
// Expand against the concrete transaction selected by write_tx!.
macro_rules! apply_writes {
    ($tx:ident, $module:expr, $guild:expr, $version:expr, $writes:expr) => {{
        let mut result = Vec::new();
        for write in $writes {
            let count = match (&write.value, write.expected_revision) {
                (Some(value), None) => sqlx::query("INSERT INTO oracle_module_documents(module,guild,collection,key,value,revision,data_version) VALUES($1,$2,$3,$4,$5,1,$6) ON CONFLICT DO NOTHING")
                    .bind($module.as_str())
                    .bind($guild.as_str())
                    .bind(&write.collection)
                    .bind(&write.key)
                    .bind(serde_json::to_string(value).map_err(integrity)?)
                    .bind(i64::from($version))
                    .execute(&mut *$tx)
                    .await
                    .map_err(db)?
                    .rows_affected(),
                (Some(value), Some(expected)) => sqlx::query("UPDATE oracle_module_documents SET value=$5,revision=revision+1,data_version=$6 WHERE module=$1 AND guild=$2 AND collection=$3 AND key=$4 AND revision=$7")
                    .bind($module.as_str())
                    .bind($guild.as_str())
                    .bind(&write.collection)
                    .bind(&write.key)
                    .bind(serde_json::to_string(value).map_err(integrity)?)
                    .bind(i64::from($version))
                    .bind(revision(expected)?)
                    .execute(&mut *$tx)
                    .await
                    .map_err(db)?
                    .rows_affected(),
                (None, Some(expected)) => sqlx::query("DELETE FROM oracle_module_documents WHERE module=$1 AND guild=$2 AND collection=$3 AND key=$4 AND revision=$5")
                    .bind($module.as_str())
                    .bind($guild.as_str())
                    .bind(&write.collection)
                    .bind(&write.key)
                    .bind(revision(expected)?)
                    .execute(&mut *$tx)
                    .await
                    .map_err(db)?
                    .rows_affected(),
                _ => return Err(err(ErrorCode::InvalidInput)),
            };
            if count != 1 {
                return Err(err(ErrorCode::Conflict));
            }
            if write.value.is_some() {
                let row: DocRow = sqlx::query_as("SELECT collection,key,value,revision FROM oracle_module_documents WHERE module=$1 AND guild=$2 AND collection=$3 AND key=$4")
                    .bind($module.as_str())
                    .bind($guild.as_str())
                    .bind(&write.collection)
                    .bind(&write.key)
                    .fetch_one(&mut *$tx)
                    .await
                    .map_err(db)?;
                result.push(doc(row)?);
            }
        }
        if serde_json::to_vec(&result).map_err(integrity)?.len() > MAX_PAGE {
            return Err(err(ErrorCode::InvalidInput));
        }
        result
    }};
}
#[async_trait]
impl ModuleRepository for Storage {
    async fn installations(&self) -> Result<Vec<InstalledModule>> {
        let _guard = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let values = rows!(
            self,
            (String, String),
            "SELECT digest,package FROM oracle_module_installations ORDER BY module,digest LIMIT 1001"
        );
        if values.len() > 1000 {
            return Err(err(ErrorCode::InvalidInput));
        }
        values
            .into_iter()
            .map(|(digest, package)| {
                Ok(InstalledModule {
                    digest,
                    package: serde_json::from_str(&package).map_err(integrity)?,
                })
            })
            .collect()
    }
    async fn install_module(&self, installed: &InstalledModule) -> Result<()> {
        if !valid_digest(&installed.digest) {
            return Err(err(ErrorCode::InvalidInput));
        }
        let package = serde_json::to_string(&installed.package).map_err(integrity)?;
        if package.len() > MAX_PAGE {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            sqlx::query("INSERT INTO oracle_module_installations(digest,module,package) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(&installed.digest).bind(installed.package.manifest.id.as_str()).bind(&package).execute(&mut *tx).await.map_err(db)?;
            let existing: (String,) =
                sqlx::query_as("SELECT package FROM oracle_module_installations WHERE digest=$1")
                    .bind(&installed.digest)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db)?;
            if existing.0 != package {
                return Err(err(ErrorCode::Conflict));
            }
            Ok(())
        })
    }
    async fn remove_installation(&self, digest: &str) -> Result<()> {
        if !valid_digest(digest) {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let busy:(i64,)=sqlx::query_as("SELECT COUNT(*) FROM oracle_module_desired d WHERE d.digest=$1 AND (d.loaded=1 OR EXISTS(SELECT 1 FROM oracle_module_activations a WHERE a.module=d.module AND a.active=1))").bind(digest).fetch_one(&mut *tx).await.map_err(db)?;
            if busy.0 > 0 {
                return Err(err(ErrorCode::Conflict));
            }
            sqlx::query("DELETE FROM oracle_module_desired WHERE digest=$1 AND loaded=0")
                .bind(digest)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            if sqlx::query("DELETE FROM oracle_module_installations WHERE digest=$1")
                .bind(digest)
                .execute(&mut *tx)
                .await
                .map_err(db)?
                .rows_affected()
                != 1
            {
                return Err(err(ErrorCode::NotFound));
            }
            Ok(())
        })
    }
    async fn desired_modules(&self) -> Result<Vec<DesiredModule>> {
        let _guard = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let values = rows!(
            self,
            (String, String, i64),
            "SELECT module,digest,loaded FROM oracle_module_desired ORDER BY module LIMIT 1001"
        );
        if values.len() > 1000 {
            return Err(err(ErrorCode::InvalidInput));
        }
        values
            .into_iter()
            .map(|(module, digest, loaded)| {
                Ok(DesiredModule {
                    module: ModuleId::new(module)?,
                    digest,
                    loaded: loaded != 0,
                })
            })
            .collect()
    }
    async fn set_module_desired(&self, desired: &DesiredModule) -> Result<()> {
        if !valid_digest(&desired.digest) {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let found: Option<(String,)> = sqlx::query_as(
                "SELECT digest FROM oracle_module_installations WHERE module=$1 AND digest=$2",
            )
            .bind(desired.module.as_str())
            .bind(&desired.digest)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
            if found.is_none() {
                return Err(err(ErrorCode::NotFound));
            }
            sqlx::query("INSERT INTO oracle_module_desired(module,digest,loaded) VALUES($1,$2,$3) ON CONFLICT(module) DO UPDATE SET digest=excluded.digest,loaded=excluded.loaded").bind(desired.module.as_str()).bind(&desired.digest).bind(i64::from(desired.loaded)).execute(&mut *tx).await.map_err(db)?;
            Ok(())
        })
    }
    async fn desired_activations(&self) -> Result<Vec<DesiredActivation>> {
        let _guard = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let values = rows!(
            self,
            (String, String, i64, String, String),
            "SELECT module,guild,active,grants,bindings FROM oracle_module_activations ORDER BY module,guild LIMIT 10001"
        );
        if values.len() > 10000 {
            return Err(err(ErrorCode::InvalidInput));
        }
        values
            .into_iter()
            .map(|(module, guild, active, grants, bindings)| {
                Ok(DesiredActivation {
                    module: ModuleId::new(module)?,
                    guild: GuildId::new(guild)?,
                    active: active != 0,
                    grants: serde_json::from_str(&grants).map_err(integrity)?,
                    bindings: serde_json::from_str(&bindings).map_err(integrity)?,
                })
            })
            .collect()
    }
    async fn set_activation_desired(&self, activation: &DesiredActivation) -> Result<()> {
        if serde_json::to_vec(activation).map_err(integrity)?.len() > 65536 {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let found: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM oracle_module_installations WHERE module=$1")
                    .bind(activation.module.as_str())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db)?;
            if found.0 == 0 {
                return Err(err(ErrorCode::NotFound));
            }
            sqlx::query("INSERT INTO oracle_module_activations(module,guild,active,grants,bindings) VALUES($1,$2,$3,$4,$5) ON CONFLICT(module,guild) DO UPDATE SET active=excluded.active,grants=excluded.grants,bindings=excluded.bindings").bind(activation.module.as_str()).bind(activation.guild.as_str()).bind(i64::from(activation.active)).bind(serde_json::to_string(&activation.grants).map_err(integrity)?).bind(serde_json::to_string(&activation.bindings).map_err(integrity)?).execute(&mut *tx).await.map_err(db)?;
            Ok(())
        })
    }
    async fn document_get(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        collection: &str,
        key: &str,
    ) -> Result<Option<ModuleDocument>> {
        if !name(collection) || !name(key) {
            return Err(err(ErrorCode::InvalidInput));
        }
        let _guard = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        rows!(self,DocRow,"SELECT collection,key,value,revision FROM oracle_module_documents WHERE module=$1 AND guild=$2 AND collection=$3 AND key=$4",module.as_str(),guild.as_str(),collection,key).into_iter().map(doc).next().transpose()
    }
    async fn document_batch(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        data_version: u32,
        writes: &[DocumentWrite],
    ) -> Result<Vec<ModuleDocument>> {
        checked_writes(writes, false)?;
        if data_version == 0 {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let current:Option<(i64,Option<i64>)>=sqlx::query_as("SELECT data_version,target_version FROM oracle_module_namespaces WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).fetch_optional(&mut *tx).await.map_err(db)?;
            if current != Some((i64::from(data_version), None)) {
                return Err(err(ErrorCode::Conflict));
            }
            Ok(apply_writes!(tx, module, guild, data_version, writes))
        })
    }
    async fn migration_status(
        &self,
        module: &ModuleId,
        guild: &GuildId,
    ) -> Result<MigrationProgress> {
        let _guard = self.inner.barrier.read().await;
        self.ensure_healthy().await?;
        let row=rows!(self,ProgressRow,"SELECT data_version,target_version,artifact_digest,cursor FROM oracle_module_namespaces WHERE module=$1 AND guild=$2",module.as_str(),guild.as_str()).pop().unwrap_or((0,None,None,None));
        progress(module, guild, row)
    }
    async fn begin_migration(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        from: u32,
        to: u32,
        digest: &str,
    ) -> Result<MigrationProgress> {
        if to <= from || !valid_digest(digest) {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            sqlx::query("INSERT INTO oracle_module_namespaces(module,guild,data_version) VALUES($1,$2,0) ON CONFLICT DO NOTHING").bind(module.as_str()).bind(guild.as_str()).execute(&mut *tx).await.map_err(db)?;
            let current:ProgressRow=sqlx::query_as("SELECT data_version,target_version,artifact_digest,cursor FROM oracle_module_namespaces WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).fetch_one(&mut *tx).await.map_err(db)?;
            if current.0 != i64::from(from) {
                return Err(err(ErrorCode::Conflict));
            }
            if let Some(target) = current.1 {
                if target != i64::from(to) || current.2.as_deref() != Some(digest) {
                    return Err(err(ErrorCode::Conflict));
                }
                return progress(module, guild, current);
            }
            if from == 0 {
                let count: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM oracle_module_documents WHERE module=$1 AND guild=$2",
                )
                .bind(module.as_str())
                .bind(guild.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
                if count.0 != 0 {
                    return Err(err(ErrorCode::Conflict));
                }
            }
            sqlx::query("UPDATE oracle_module_namespaces SET target_version=$3,artifact_digest=$4,cursor=NULL,last_collection='',last_key='' WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).bind(i64::from(to)).bind(digest).execute(&mut *tx).await.map_err(db)?;
            progress(
                module,
                guild,
                (
                    i64::from(from),
                    Some(i64::from(to)),
                    Some(digest.into()),
                    None,
                ),
            )
        })
    }
    async fn migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        limit: u32,
    ) -> Result<MigrationPage> {
        if limit == 0 || limit > 100 {
            return Err(err(ErrorCode::InvalidInput));
        }
        write_tx!(self, tx, {
            let state:Option<(i64,Option<i64>,Option<String>,Option<String>,String,String)>=sqlx::query_as("SELECT data_version,target_version,artifact_digest,cursor,last_collection,last_key FROM oracle_module_namespaces WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).fetch_optional(&mut *tx).await.map_err(db)?;
            let (from, target, digest, cursor, collection, key) =
                state.ok_or_else(|| err(ErrorCode::NotFound))?;
            if target.is_none() {
                return Err(err(ErrorCode::Conflict));
            }
            let digest = digest.ok_or_else(|| err(ErrorCode::Integrity))?;
            let values:Vec<DocRow>=sqlx::query_as("SELECT collection,key,value,revision FROM oracle_module_documents WHERE module=$1 AND guild=$2 AND data_version=$3 AND (collection,key)>($4,$5) ORDER BY collection,key LIMIT $6").bind(module.as_str()).bind(guild.as_str()).bind(from).bind(&collection).bind(&key).bind(i64::from(limit)+1).fetch_all(&mut *tx).await.map_err(db)?;
            let available = values.len();
            let mut documents = Vec::new();
            for row in values {
                if documents.len() == limit as usize {
                    break;
                }
                documents.push(doc(row)?);
                if serde_json::to_vec(&documents).map_err(integrity)?.len() > MAX_PAGE {
                    documents.pop();
                    break;
                }
            }
            let finished = documents.len() == available;
            let next_cursor = if finished {
                None
            } else {
                Some(uuid::Uuid::new_v4().to_string())
            };
            let last = documents
                .last()
                .map(|d| (d.collection.clone(), d.key.clone()))
                .unwrap_or((collection, key));
            let readset: Vec<_> = documents
                .iter()
                .map(|d| (&d.collection, &d.key, d.revision))
                .collect();
            sqlx::query("INSERT INTO oracle_module_migration_pages(module,guild,digest,expected_cursor,next_cursor,last_collection,last_key,finished,readset) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(module,guild) DO UPDATE SET digest=excluded.digest,expected_cursor=excluded.expected_cursor,next_cursor=excluded.next_cursor,last_collection=excluded.last_collection,last_key=excluded.last_key,finished=excluded.finished,readset=excluded.readset").bind(module.as_str()).bind(guild.as_str()).bind(digest).bind(cursor).bind(&next_cursor).bind(last.0).bind(last.1).bind(i64::from(finished)).bind(serde_json::to_string(&readset).map_err(integrity)?).execute(&mut *tx).await.map_err(db)?;
            Ok(MigrationPage {
                documents,
                next_cursor,
            })
        })
    }
    async fn commit_migration_page(
        &self,
        module: &ModuleId,
        guild: &GuildId,
        digest: &str,
        expected_cursor: Option<&str>,
        writes: &[DocumentWrite],
        next_cursor: Option<&str>,
        complete: bool,
    ) -> Result<MigrationProgress> {
        checked_writes(writes, true)?;
        write_tx!(self, tx, {
            let state:Option<ProgressRow>=sqlx::query_as("SELECT data_version,target_version,artifact_digest,cursor FROM oracle_module_namespaces WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).fetch_optional(&mut *tx).await.map_err(db)?;
            let state = state.ok_or_else(|| err(ErrorCode::NotFound))?;
            if state.2.as_deref() != Some(digest) || state.3.as_deref() != expected_cursor {
                return Err(err(ErrorCode::Conflict));
            }
            let target = u32::try_from(state.1.ok_or_else(|| err(ErrorCode::Conflict))?)
                .map_err(integrity)?;
            let page:Option<(String,Option<String>,Option<String>,String,String,i64,String)>=sqlx::query_as("SELECT digest,expected_cursor,next_cursor,last_collection,last_key,finished,readset FROM oracle_module_migration_pages WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).fetch_optional(&mut *tx).await.map_err(db)?;
            let (page_digest, page_from, page_next, last_collection, last_key, finished, readset) =
                page.ok_or_else(|| err(ErrorCode::Conflict))?;
            if page_digest != digest
                || page_from.as_deref() != expected_cursor
                || page_next.as_deref() != next_cursor
                || (finished != 0) != complete
            {
                return Err(err(ErrorCode::Conflict));
            }
            let readset: Vec<(String, String, u64)> =
                serde_json::from_str(&readset).map_err(integrity)?;
            for (collection, key, revision) in readset {
                if !writes.iter().any(|w| {
                    w.collection == collection
                        && w.key == key
                        && w.expected_revision == Some(revision)
                }) {
                    return Err(err(ErrorCode::Conflict));
                }
            }
            let _ = apply_writes!(tx, module, guild, target, writes);
            if complete {
                let remaining:(i64,)=sqlx::query_as("SELECT COUNT(*) FROM oracle_module_documents WHERE module=$1 AND guild=$2 AND data_version!=$3").bind(module.as_str()).bind(guild.as_str()).bind(i64::from(target)).fetch_one(&mut *tx).await.map_err(db)?;
                if remaining.0 != 0 {
                    return Err(err(ErrorCode::Conflict));
                }
                sqlx::query("UPDATE oracle_module_namespaces SET data_version=$3,target_version=NULL,artifact_digest=NULL,cursor=NULL,last_collection='',last_key='' WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).bind(i64::from(target)).execute(&mut *tx).await.map_err(db)?;
            } else {
                sqlx::query("UPDATE oracle_module_namespaces SET cursor=$3,last_collection=$4,last_key=$5 WHERE module=$1 AND guild=$2").bind(module.as_str()).bind(guild.as_str()).bind(next_cursor).bind(last_collection).bind(last_key).execute(&mut *tx).await.map_err(db)?;
            }
            sqlx::query("DELETE FROM oracle_module_migration_pages WHERE module=$1 AND guild=$2")
                .bind(module.as_str())
                .bind(guild.as_str())
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            if complete {
                progress(module, guild, (i64::from(target), None, None, None))
            } else {
                progress(
                    module,
                    guild,
                    (
                        state.0,
                        Some(i64::from(target)),
                        Some(digest.into()),
                        next_cursor.map(str::to_owned),
                    ),
                )
            }
        })
    }
}
