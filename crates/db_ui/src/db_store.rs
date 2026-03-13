//! Database connection store: config persistence and schema caching.
//!
//! Connection configs (host, port, user, database name) are written to
//! `paths::config_dir()/databases.json`. Passwords are stored in the
//! platform keychain via gpui's `cx.write_credentials` / `cx.read_credentials`.

use anyhow::{Context as _, Result};
use gpui::{AppContext, AsyncApp, Context, Entity, EventEmitter, FutureExt};
use gpui_tokio::Tokio;
use serde::{Deserialize, Serialize};
use sqlx::{Column, Executor, QueryBuilder, Row, TypeInfo};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbKind {
    MySql,
    MariaDb,
    Postgres,
}

impl DbKind {
    pub fn label(self) -> &'static str {
        match self {
            DbKind::MySql => "MySQL",
            DbKind::MariaDb => "MariaDB",
            DbKind::Postgres => "PostgreSQL",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DbKind::MySql | DbKind::MariaDb => 3306,
            DbKind::Postgres => 5432,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub database: Option<String>,
}

impl ConnectionConfig {
    pub fn keychain_key(&self) -> String {
        format!("zed-db-{}", self.id)
    }
}

/// Schema for a single database within a connection.
#[derive(Debug, Clone, Default)]
pub struct DatabaseSchema {
    pub name: String,
    pub tables: Vec<String>,
    pub views: Vec<String>,
    pub functions: Vec<String>,
}

/// Full schema for a connection: multiple databases, each with tables/views/functions.
#[derive(Debug, Clone, Default)]
pub struct DbSchema {
    pub databases: Vec<DatabaseSchema>,
}

impl DbSchema {
    /// Returns schema for a single-database connection (legacy / backward compat).
    pub fn single_database(&self) -> Option<&DatabaseSchema> {
        if self.databases.len() == 1 {
            self.databases.first()
        } else {
            None
        }
    }

    /// Iterate over all databases.
    pub fn iter_databases(&self) -> impl Iterator<Item = &DatabaseSchema> {
        self.databases.iter()
    }
}

#[derive(Debug, Clone)]
pub struct QueryColumn {
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone)]
pub struct QueryKeyValue {
    pub column_name: String,
    pub type_name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryRow {
    pub values: Vec<Option<String>>,
    pub primary_key: Vec<QueryKeyValue>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryExecution {
    pub columns: Vec<QueryColumn>,
    pub rows: Vec<QueryRow>,
    pub summary: String,
    pub elapsed: Duration,
    pub affected_rows: u64,
    pub table_name: Option<String>,
    pub primary_key_columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum CellUpdateScope {
    PrimaryKey(Vec<QueryKeyValue>),
    AllRows,
}

#[derive(Debug, Clone)]
pub struct CellUpdate {
    pub table_name: String,
    pub column_name: String,
    pub column_type: String,
    pub new_value: Option<String>,
    pub scope: CellUpdateScope,
}

#[derive(Debug, Clone)]
pub enum ConnectionState {
    Disconnected,
    Loading,
    Connected(DbSchema),
    Error(String),
}

impl ConnectionState {
    pub fn is_loading(&self) -> bool {
        matches!(self, ConnectionState::Loading)
    }

    pub fn schema(&self) -> Option<&DbSchema> {
        if let ConnectionState::Connected(schema) = self {
            Some(schema)
        } else {
            None
        }
    }

    pub fn error(&self) -> Option<&str> {
        if let ConnectionState::Error(error) = self {
            Some(error)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct DbConnection {
    pub config: ConnectionConfig,
    pub state: ConnectionState,
}

pub enum DbStoreEvent {
    ConnectionsChanged,
}

pub struct DbStore {
    connections: Vec<DbConnection>,
    config_path: PathBuf,
}

impl EventEmitter<DbStoreEvent> for DbStore {}

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

impl DbStore {
    pub fn new(cx: &mut impl AppContext) -> Entity<Self> {
        let config_path = paths::config_dir().join("databases.json");
        cx.new(|_cx| DbStore {
            connections: Vec::new(),
            config_path,
        })
    }

    pub fn connections(&self) -> &[DbConnection] {
        &self.connections
    }

    pub fn connection_by_id(&self, id: Uuid) -> Option<&DbConnection> {
        self.connections
            .iter()
            .find(|connection| connection.config.id == id)
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        let path = self.config_path.clone();
        match Self::read_configs(&path) {
            Ok(configs) => {
                for config in configs {
                    if self
                        .connections
                        .iter()
                        .any(|connection| connection.config.id == config.id)
                    {
                        continue;
                    }
                    self.connections.push(DbConnection {
                        config,
                        state: ConnectionState::Disconnected,
                    });
                }
            }
            Err(error) => {
                log::warn!("Failed to load database configs: {}", error);
            }
        }
        cx.emit(DbStoreEvent::ConnectionsChanged);
        cx.notify();
    }

    pub async fn test_connection(
        config: ConnectionConfig,
        password: String,
        cx: &mut AsyncApp,
    ) -> Result<DbSchema> {
        Tokio::spawn_result(cx, async move { fetch_schema(&config, &password).await })
            .with_timeout(CONNECTION_TIMEOUT, cx.background_executor())
            .await
            .map_err(|_| anyhow::anyhow!("Connection timed out after 10 seconds"))?
    }

    pub async fn execute_query(
        config: ConnectionConfig,
        database: String,
        password: String,
        sql: String,
        target_table: Option<String>,
        cx: &mut AsyncApp,
    ) -> Result<QueryExecution> {
        Tokio::spawn_result(cx, async move {
            execute_query(&config, &database, &password, &sql, target_table.as_deref()).await
        })
        .with_timeout(QUERY_TIMEOUT, cx.background_executor())
        .await
        .map_err(|_| anyhow::anyhow!("Query timed out after 30 seconds"))?
    }

    pub async fn update_cell(
        config: ConnectionConfig,
        database: String,
        password: String,
        update: CellUpdate,
        cx: &mut AsyncApp,
    ) -> Result<u64> {
        Tokio::spawn_result(cx, async move {
            execute_cell_update(&config, &database, &password, &update).await
        })
        .with_timeout(QUERY_TIMEOUT, cx.background_executor())
        .await
        .map_err(|_| anyhow::anyhow!("Update timed out after 30 seconds"))?
    }

    fn read_configs(path: &PathBuf) -> Result<Vec<ConnectionConfig>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&contents).context("parsing databases.json")
    }

    fn save_configs(&self) -> Result<()> {
        let configs: Vec<&ConnectionConfig> = self
            .connections
            .iter()
            .map(|connection| &connection.config)
            .collect();
        let json = serde_json::to_string_pretty(&configs)?;
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.config_path, json)?;
        Ok(())
    }

    fn insert_connection_record(
        &mut self,
        config: ConnectionConfig,
        state: ConnectionState,
        cx: &mut Context<Self>,
    ) {
        self.connections.push(DbConnection { config, state });
        if let Err(error) = self.save_configs() {
            log::error!("Failed to save database configs: {}", error);
        }
        cx.emit(DbStoreEvent::ConnectionsChanged);
        cx.notify();
    }

    async fn store_credentials(
        config: &ConnectionConfig,
        password: &str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let key = config.keychain_key();
        let username = config.username.clone();
        cx.update(move |cx| cx.write_credentials(&key, &username, password.as_bytes()))
            .await
            .map_err(|error| {
                anyhow::anyhow!("Failed to write credentials to the system keychain: {error}")
            })
    }

    pub fn add_connection(
        &mut self,
        config: ConnectionConfig,
        password: String,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let id = config.id;
        let entity = cx.weak_entity();
        cx.spawn(async move |_this, cx| {
            DbStore::store_credentials(&config, &password, cx).await?;

            entity.update(cx, move |store, cx| {
                store.insert_connection_record(config, ConnectionState::Disconnected, cx);
                store.refresh_connection(id, cx);
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub async fn add_connection_with_schema(
        db_store: Entity<Self>,
        config: ConnectionConfig,
        password: String,
        schema: DbSchema,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        Self::store_credentials(&config, &password, cx).await?;

        db_store.update(cx, |store, cx| {
            store.insert_connection_record(config, ConnectionState::Connected(schema), cx);
        });

        Ok(())
    }

    pub fn remove_connection(
        &mut self,
        id: Uuid,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(position) = self
            .connections
            .iter()
            .position(|connection| connection.config.id == id)
        {
            let key = self.connections[position].config.keychain_key();
            let _ = cx.delete_credentials(&key);
            self.connections.remove(position);
            if let Err(error) = self.save_configs() {
                log::error!("Failed to save database configs after remove: {}", error);
            }
            cx.emit(DbStoreEvent::ConnectionsChanged);
            cx.notify();
        }
    }

    pub fn refresh_connection(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(connection) = self
            .connections
            .iter_mut()
            .find(|connection| connection.config.id == id)
        else {
            return;
        };
        let config = connection.config.clone();
        connection.state = ConnectionState::Loading;
        cx.emit(DbStoreEvent::ConnectionsChanged);
        cx.notify();

        let entity = cx.weak_entity();
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            let key = config.keychain_key();
            let password = cx
                .update(|cx| cx.read_credentials(&key))
                .await
                .ok()
                .flatten()
                .map(|(_, password): (String, Vec<u8>)| {
                    String::from_utf8_lossy(&password).into_owned()
                })
                .unwrap_or_default();

            let result = DbStore::test_connection(config.clone(), password, cx).await;
            let _ = entity.update(cx, move |store: &mut DbStore, cx| {
                if let Some(connection) = store
                    .connections
                    .iter_mut()
                    .find(|connection| connection.config.id == id)
                {
                    connection.state = match result {
                        Ok(schema) => ConnectionState::Connected(schema),
                        Err(error) => ConnectionState::Error(error.to_string()),
                    };
                }
                cx.emit(DbStoreEvent::ConnectionsChanged);
                cx.notify();
            });
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn connect_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<Uuid> = self
            .connections
            .iter()
            .filter(|connection| matches!(connection.state, ConnectionState::Disconnected))
            .map(|connection| connection.config.id)
            .collect();
        for id in ids {
            self.refresh_connection(id, cx);
        }
    }
}

async fn fetch_schema(config: &ConnectionConfig, password: &str) -> Result<DbSchema> {
    match config.kind {
        DbKind::MySql | DbKind::MariaDb => fetch_mysql_schema(config, password).await,
        DbKind::Postgres => fetch_postgres_schema(config, password).await,
    }
}

async fn execute_query(
    config: &ConnectionConfig,
    database: &str,
    password: &str,
    sql: &str,
    target_table: Option<&str>,
) -> Result<QueryExecution> {
    match config.kind {
        DbKind::MySql | DbKind::MariaDb => {
            execute_mysql_query(config, database, password, sql, target_table).await
        }
        DbKind::Postgres => {
            execute_postgres_query(config, database, password, sql, target_table).await
        }
    }
}

async fn execute_cell_update(
    config: &ConnectionConfig,
    database: &str,
    password: &str,
    update: &CellUpdate,
) -> Result<u64> {
    match config.kind {
        DbKind::MySql | DbKind::MariaDb => {
            execute_mysql_cell_update(config, database, password, update).await
        }
        DbKind::Postgres => {
            execute_postgres_cell_update(config, database, password, update).await
        }
    }
}

async fn fetch_mysql_schema(config: &ConnectionConfig, password: &str) -> Result<DbSchema> {
    // Connect to mysql (system db) to list databases
    let url = format!(
        "mysql://{}:{}@{}:{}/mysql",
        config.username, password, config.host, config.port
    );

    let pool = sqlx::mysql::MySqlPool::connect(&url)
        .await
        .with_context(|| format!("connecting to MySQL at {}:{}", config.host, config.port))?;

    let database_names: Vec<String> = sqlx::query("SHOW DATABASES")
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|row| row.get::<String, _>(0))
        .collect();

    pool.close().await;

    let mut databases = Vec::with_capacity(database_names.len());
    for db_name in database_names {
        let schema = fetch_mysql_database_schema(config, password, &db_name).await?;
        databases.push(schema);
    }

    Ok(DbSchema { databases })
}

async fn fetch_mysql_database_schema(
    config: &ConnectionConfig,
    password: &str,
    database: &str,
) -> Result<DatabaseSchema> {
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        config.username, password, config.host, config.port, database
    );

    let pool = sqlx::mysql::MySqlPool::connect(&url)
        .await
        .with_context(|| format!("connecting to MySQL database {} at {}:{}", database, config.host, config.port))?;

    let tables: Vec<String> = sqlx::query(
        "SELECT TABLE_NAME FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' \
         ORDER BY TABLE_NAME",
    )
    .bind(database)
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect();

    let views: Vec<String> = sqlx::query(
        "SELECT TABLE_NAME FROM information_schema.VIEWS \
         WHERE TABLE_SCHEMA = ? \
         ORDER BY TABLE_NAME",
    )
    .bind(database)
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect();

    let functions: Vec<String> = sqlx::query(
        "SELECT ROUTINE_NAME FROM information_schema.ROUTINES \
         WHERE ROUTINE_SCHEMA = ? \
         ORDER BY ROUTINE_NAME",
    )
    .bind(database)
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect();

    pool.close().await;

    Ok(DatabaseSchema {
        name: database.to_string(),
        tables,
        views,
        functions,
    })
}

async fn execute_mysql_query(
    config: &ConnectionConfig,
    database: &str,
    password: &str,
    sql: &str,
    target_table: Option<&str>,
) -> Result<QueryExecution> {
    let sql = normalize_sql(sql)?;
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        config.username, password, config.host, config.port, database
    );

    let pool = sqlx::mysql::MySqlPool::connect(&url)
        .await
        .with_context(|| format!("connecting to MySQL at {}:{}", config.host, config.port))?;

    let started_at = Instant::now();
    let describe = pool.describe(sql).await?;
    let primary_key_columns = if let Some(table_name) = target_table {
        fetch_mysql_primary_key_columns(&pool, table_name).await?
    } else {
        Vec::new()
    };
    let columns = describe
        .columns()
        .iter()
        .map(|column| QueryColumn {
            name: column.name().to_string(),
            type_name: column.type_info().name().to_string(),
        })
        .collect::<Vec<_>>();

    let execution = if should_fetch_rows(sql, columns.len()) {
        let wrapped_query = wrap_mysql_text_query(sql, &columns);
        let rows = sqlx::query(&wrapped_query).fetch_all(&pool).await?;
        let raw_rows = rows
            .into_iter()
            .map(|row| {
                (0..columns.len())
                    .map(|index| row.try_get::<Option<String>, _>(index).map_err(Into::into))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let row_count = raw_rows.len() as u64;
        let rows = build_query_rows(&columns, raw_rows, &primary_key_columns);

        QueryExecution {
            columns,
            rows,
            summary: format!("{} row{} returned", row_count, pluralize(row_count)),
            elapsed: started_at.elapsed(),
            affected_rows: row_count,
            table_name: target_table.map(ToOwned::to_owned),
            primary_key_columns,
        }
    } else {
        let result = sqlx::query(sql).execute(&pool).await?;
        let affected_rows = result.rows_affected();

        QueryExecution {
            columns: Vec::new(),
            rows: Vec::new(),
            summary: format!("{} row{} affected", affected_rows, pluralize(affected_rows)),
            elapsed: started_at.elapsed(),
            affected_rows,
            table_name: target_table.map(ToOwned::to_owned),
            primary_key_columns,
        }
    };

    pool.close().await;
    Ok(execution)
}

async fn fetch_postgres_schema(config: &ConnectionConfig, password: &str) -> Result<DbSchema> {
    let url = format!(
        "postgres://{}:{}@{}:{}/postgres",
        config.username, password, config.host, config.port
    );

    let pool = sqlx::postgres::PgPool::connect(&url)
        .await
        .with_context(|| format!("connecting to Postgres at {}:{}", config.host, config.port))?;

    let database_names: Vec<String> = sqlx::query(
        "SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname",
    )
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect();

    pool.close().await;

    let mut databases = Vec::with_capacity(database_names.len());
    for db_name in database_names {
        let schema = fetch_postgres_database_schema(config, password, &db_name).await?;
        databases.push(schema);
    }

    Ok(DbSchema { databases })
}

async fn fetch_postgres_database_schema(
    config: &ConnectionConfig,
    password: &str,
    database: &str,
) -> Result<DatabaseSchema> {
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        config.username, password, config.host, config.port, database
    );

    let pool = sqlx::postgres::PgPool::connect(&url)
        .await
        .with_context(|| format!("connecting to Postgres database {} at {}:{}", database, config.host, config.port))?;

    let tables: Vec<String> = sqlx::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
         ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect();

    let views: Vec<String> = sqlx::query(
        "SELECT table_name FROM information_schema.views \
         WHERE table_schema = 'public' \
         ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect();

    let functions: Vec<String> = sqlx::query(
        "SELECT routine_name FROM information_schema.routines \
         WHERE routine_schema = 'public' AND routine_type = 'FUNCTION' \
         ORDER BY routine_name",
    )
    .fetch_all(&pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect();

    pool.close().await;

    Ok(DatabaseSchema {
        name: database.to_string(),
        tables,
        views,
        functions,
    })
}

async fn execute_postgres_query(
    config: &ConnectionConfig,
    database: &str,
    password: &str,
    sql: &str,
    target_table: Option<&str>,
) -> Result<QueryExecution> {
    let sql = normalize_sql(sql)?;
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        config.username, password, config.host, config.port, database
    );

    let pool = sqlx::postgres::PgPool::connect(&url)
        .await
        .with_context(|| format!("connecting to Postgres at {}:{}", config.host, config.port))?;

    let started_at = Instant::now();
    let describe = pool.describe(sql).await?;
    let primary_key_columns = if let Some(table_name) = target_table {
        fetch_postgres_primary_key_columns(&pool, table_name).await?
    } else {
        Vec::new()
    };
    let columns = describe
        .columns()
        .iter()
        .map(|column| QueryColumn {
            name: column.name().to_string(),
            type_name: column.type_info().name().to_string(),
        })
        .collect::<Vec<_>>();

    let execution = if should_fetch_rows(sql, columns.len()) {
        let wrapped_query = wrap_postgres_text_query(sql, &columns);
        let rows = sqlx::query(&wrapped_query).fetch_all(&pool).await?;
        let raw_rows = rows
            .into_iter()
            .map(|row| {
                (0..columns.len())
                    .map(|index| row.try_get::<Option<String>, _>(index).map_err(Into::into))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let row_count = raw_rows.len() as u64;
        let rows = build_query_rows(&columns, raw_rows, &primary_key_columns);

        QueryExecution {
            columns,
            rows,
            summary: format!("{} row{} returned", row_count, pluralize(row_count)),
            elapsed: started_at.elapsed(),
            affected_rows: row_count,
            table_name: target_table.map(ToOwned::to_owned),
            primary_key_columns,
        }
    } else {
        let result = sqlx::query(sql).execute(&pool).await?;
        let affected_rows = result.rows_affected();

        QueryExecution {
            columns: Vec::new(),
            rows: Vec::new(),
            summary: format!("{} row{} affected", affected_rows, pluralize(affected_rows)),
            elapsed: started_at.elapsed(),
            affected_rows,
            table_name: target_table.map(ToOwned::to_owned),
            primary_key_columns,
        }
    };

    pool.close().await;
    Ok(execution)
}

async fn execute_mysql_cell_update(
    config: &ConnectionConfig,
    database: &str,
    password: &str,
    update: &CellUpdate,
) -> Result<u64> {
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        config.username, password, config.host, config.port, database
    );

    let pool = sqlx::mysql::MySqlPool::connect(&url)
        .await
        .with_context(|| format!("connecting to MySQL at {}:{}", config.host, config.port))?;

    let mut builder = QueryBuilder::<sqlx::MySql>::new("UPDATE ");
    builder
        .push(quote_mysql_identifier(&update.table_name))
        .push(" SET ")
        .push(quote_mysql_identifier(&update.column_name))
        .push(" = ");
    push_mysql_value(&mut builder, update.new_value.as_deref());
    push_mysql_update_scope(&mut builder, &update.scope);

    let result = builder.build().execute(&pool).await?;
    pool.close().await;
    Ok(result.rows_affected())
}

async fn execute_postgres_cell_update(
    config: &ConnectionConfig,
    database: &str,
    password: &str,
    update: &CellUpdate,
) -> Result<u64> {
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        config.username, password, config.host, config.port, database
    );

    let pool = sqlx::postgres::PgPool::connect(&url)
        .await
        .with_context(|| format!("connecting to Postgres at {}:{}", config.host, config.port))?;

    let mut builder = QueryBuilder::<sqlx::Postgres>::new("UPDATE ");
    builder
        .push(quote_postgres_identifier(&update.table_name))
        .push(" SET ")
        .push(quote_postgres_identifier(&update.column_name))
        .push(" = ");
    push_postgres_typed_value(
        &mut builder,
        update.new_value.as_deref(),
        Some(&update.column_type),
    );
    push_postgres_update_scope(&mut builder, &update.scope);

    let result = builder.build().execute(&pool).await?;
    pool.close().await;
    Ok(result.rows_affected())
}

async fn fetch_mysql_primary_key_columns(
    pool: &sqlx::mysql::MySqlPool,
    table_name: &str,
) -> Result<Vec<String>> {
    Ok(sqlx::query(
        "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND CONSTRAINT_NAME = 'PRIMARY' \
         ORDER BY ORDINAL_POSITION",
    )
    .bind(table_name)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect())
}

async fn fetch_postgres_primary_key_columns(
    pool: &sqlx::postgres::PgPool,
    table_name: &str,
) -> Result<Vec<String>> {
    Ok(sqlx::query(
        "SELECT kcu.column_name \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON tc.constraint_name = kcu.constraint_name \
          AND tc.table_schema = kcu.table_schema \
         WHERE tc.constraint_type = 'PRIMARY KEY' \
           AND tc.table_schema = 'public' \
           AND tc.table_name = $1 \
         ORDER BY kcu.ordinal_position",
    )
    .bind(table_name)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| row.get::<String, _>(0))
    .collect())
}

fn normalize_sql(sql: &str) -> Result<&str> {
    let mut trimmed = sql.trim();
    while let Some(without_semicolon) = trimmed.strip_suffix(';') {
        trimmed = without_semicolon.trim_end();
    }

    if trimmed.is_empty() {
        anyhow::bail!("Enter a SQL query before running it.");
    }

    Ok(trimmed)
}

fn should_fetch_rows(sql: &str, column_count: usize) -> bool {
    if column_count == 0 {
        return false;
    }

    !matches!(
        leading_keyword(sql).as_deref(),
        Some("alter" | "create" | "delete" | "drop" | "insert" | "merge" | "truncate" | "update")
    )
}

fn leading_keyword(sql: &str) -> Option<String> {
    sql.split_whitespace().next().map(|keyword| {
        keyword
            .trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
            .to_ascii_lowercase()
    })
}

fn wrap_mysql_text_query(sql: &str, columns: &[QueryColumn]) -> String {
    let projection = columns
        .iter()
        .map(|column| {
            let name = quote_mysql_identifier(&column.name);
            format!("CAST(q.{name} AS CHAR) AS {name}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!("SELECT {projection} FROM ({sql}) AS q")
}

fn wrap_postgres_text_query(sql: &str, columns: &[QueryColumn]) -> String {
    let projection = columns
        .iter()
        .map(|column| {
            let name = quote_postgres_identifier(&column.name);
            format!("q.{name}::text AS {name}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!("SELECT {projection} FROM ({sql}) AS q")
}

fn build_query_rows(
    columns: &[QueryColumn],
    raw_rows: Vec<Vec<Option<String>>>,
    primary_key_columns: &[String],
) -> Vec<QueryRow> {
    let primary_key_indices = primary_key_columns
        .iter()
        .map(|primary_key| {
            columns
                .iter()
                .position(|column| column.name == *primary_key)
        })
        .collect::<Option<Vec<_>>>();

    raw_rows
        .into_iter()
        .map(|values| {
            let primary_key = primary_key_indices
                .as_ref()
                .map(|indices| {
                    indices
                        .iter()
                        .map(|&index| QueryKeyValue {
                            column_name: columns[index].name.clone(),
                            type_name: columns[index].type_name.clone(),
                            value: values.get(index).cloned().unwrap_or(None),
                        })
                        .collect()
                })
                .unwrap_or_default();

            QueryRow {
                values,
                primary_key,
            }
        })
        .collect()
}

fn push_mysql_update_scope(builder: &mut QueryBuilder<'_, sqlx::MySql>, scope: &CellUpdateScope) {
    let CellUpdateScope::PrimaryKey(primary_key) = scope else {
        return;
    };

    if primary_key.is_empty() {
        return;
    }

    builder.push(" WHERE ");
    let mut separated = builder.separated(" AND ");
    for key in primary_key {
        separated.push(quote_mysql_identifier(&key.column_name));
        match key.value.as_deref() {
            Some(value) => {
                separated.push(" = ");
                separated.push_bind(value.to_string());
            }
            None => {
                separated.push(" IS NULL");
            }
        }
    }
}

fn push_postgres_update_scope(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    scope: &CellUpdateScope,
) {
    let CellUpdateScope::PrimaryKey(primary_key) = scope else {
        return;
    };

    if primary_key.is_empty() {
        return;
    }

    builder.push(" WHERE ");
    let mut separated = builder.separated(" AND ");
    for key in primary_key {
        separated.push(quote_postgres_identifier(&key.column_name));
        match key.value.as_deref() {
            Some(value) => {
                separated.push(" = ");
                push_postgres_typed_value_in_separated(
                    &mut separated,
                    Some(value),
                    Some(&key.type_name),
                );
            }
            None => {
                separated.push(" IS NULL");
            }
        }
    }
}

fn push_mysql_value(builder: &mut QueryBuilder<'_, sqlx::MySql>, value: Option<&str>) {
    match value {
        Some(value) => {
            builder.push_bind(value.to_string());
        }
        None => {
            builder.push("NULL");
        }
    }
}

fn push_postgres_typed_value_in_separated<Sep>(
    builder: &mut sqlx::query_builder::Separated<'_, '_, sqlx::Postgres, Sep>,
    value: Option<&str>,
    type_name: Option<&str>,
) where
    Sep: std::fmt::Display,
{
    let sanitized_type_name = type_name.and_then(sanitize_db_type_name);
    match (value, sanitized_type_name) {
        (Some(value), Some(type_name)) => {
            builder.push_bind(value.to_string());
            builder.push("::");
            builder.push(type_name);
        }
        (Some(value), None) => {
            builder.push_bind(value.to_string());
        }
        (None, Some(type_name)) => {
            builder.push("NULL::");
            builder.push(type_name);
        }
        (None, None) => {
            builder.push("NULL");
        }
    }
}

fn push_postgres_typed_value(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    value: Option<&str>,
    type_name: Option<&str>,
) {
    let sanitized_type_name = type_name.and_then(sanitize_db_type_name);
    match (value, sanitized_type_name) {
        (Some(value), Some(type_name)) => {
            builder.push_bind(value.to_string());
            builder.push("::");
            builder.push(type_name);
        }
        (Some(value), None) => {
            builder.push_bind(value.to_string());
        }
        (None, Some(type_name)) => {
            builder.push("NULL::");
            builder.push(type_name);
        }
        (None, None) => {
            builder.push("NULL");
        }
    }
}

fn sanitize_db_type_name(type_name: &str) -> Option<&str> {
    type_name
        .chars()
        .all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '_' | ' ' | '(' | ')' | ',' | '[' | ']' | '.')
        })
        .then_some(type_name)
}

fn quote_mysql_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

fn quote_postgres_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn pluralize(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}
