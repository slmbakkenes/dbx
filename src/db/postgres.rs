//! PostgreSQL driver.

use super::{ColumnMeta, Driver, Grid};
use crate::config::Connection as ConnCfg;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{StreamExt, pin_mut};
use std::time::Instant;
use tokio_postgres::config::SslMode;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, NoTls, Row};

pub struct PgDriver {
    client: Client,
    cancel: tokio_postgres::CancelToken,
}

pub fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

impl PgDriver {
    pub async fn connect(c: &ConnCfg) -> Result<Self> {
        let password = c.resolve_password()?;
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&c.host)
            .port(c.port_or_default())
            .dbname(&c.database)
            .user(&c.user)
            .password(password)
            .application_name("dbx");
        let mode = c.sslmode.as_deref().unwrap_or("prefer");
        cfg.ssl_mode(match mode {
            "disable" => SslMode::Disable,
            "require" => SslMode::Require,
            _ => SslMode::Prefer,
        });

        let client = if mode == "disable" {
            let (client, conn) = cfg.connect(NoTls).await.context("connecting to postgres")?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            client
        } else {
            let tls = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(c.trust_cert)
                .danger_accept_invalid_hostnames(c.trust_cert)
                .build()
                .context("building TLS connector")?;
            let (client, conn) = cfg
                .connect(postgres_native_tls::MakeTlsConnector::new(tls))
                .await
                .context("connecting to postgres")?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            client
        };

        let cancel = client.cancel_token();
        Ok(Self { client, cancel })
    }

    async fn strings(&mut self, sql: &str, params: &[&str]) -> Result<Vec<String>> {
        let owned: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        let rows = self.client.query(sql, &owned).await?;
        Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
    }
}

#[async_trait]
impl Driver for PgDriver {
    async fn databases(&mut self) -> Result<Vec<String>> {
        self.strings(
            "select datname from pg_database \
             where datallowconn and not datistemplate \
               and has_database_privilege(datname, 'CONNECT') \
             order by 1",
            &[],
        )
        .await
    }

    async fn schemas(&mut self) -> Result<Vec<String>> {
        self.strings(
            "select nspname from pg_namespace \
             where nspname <> 'information_schema' and nspname not like 'pg\\_%' \
             order by 1",
            &[],
        )
        .await
    }

    async fn tables(&mut self, schema: &str) -> Result<Vec<String>> {
        self.strings(
            "select c.relname from pg_class c \
             join pg_namespace n on n.oid = c.relnamespace \
             where n.nspname = $1 and c.relkind in ('r','v','m','p','f') \
             order by 1",
            &[schema],
        )
        .await
    }

    async fn columns(&mut self, schema: &str, table: &str) -> Result<Grid> {
        let rows = self
            .client
            .query(
                "select a.attname::text, \
                        format_type(a.atttypid, a.atttypmod), \
                        (not a.attnotnull) as nullable, \
                        pg_get_expr(d.adbin, d.adrelid) as default_expr, \
                        coalesce(k.is_pk, false) as is_pk \
                 from pg_attribute a \
                 join pg_class c on c.oid = a.attrelid \
                 join pg_namespace n on n.oid = c.relnamespace \
                 left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
                 left join (select conrelid, unnest(conkey) as attnum, true as is_pk \
                            from pg_constraint where contype = 'p') k \
                        on k.conrelid = a.attrelid and k.attnum = a.attnum \
                 where n.nspname = $1 and c.relname = $2 \
                   and a.attnum > 0 and not a.attisdropped \
                 order by a.attnum",
                &[&schema, &table],
            )
            .await?;
        let metas: Vec<(ColumnMeta, bool)> = rows
            .iter()
            .map(|r| {
                (
                    ColumnMeta {
                        name: r.get::<_, String>(0),
                        data_type: r.get::<_, String>(1),
                        nullable: r.get::<_, bool>(2),
                        default: r.get::<_, Option<String>>(3),
                    },
                    r.get::<_, bool>(4),
                )
            })
            .collect();
        Ok(super::structure_grid(&metas))
    }

    async fn query(&mut self, sql: &str, max_rows: usize) -> Result<Grid> {
        let started = Instant::now();
        if !super::returns_rows(sql) {
            let affected = self.client.execute(sql, &[]).await?;
            return Ok(Grid {
                columns: vec![],
                rows: vec![],
                affected: Some(affected),
                elapsed_ms: started.elapsed().as_millis() as u64,
                complete: true,
            });
        }

        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![];
        let stream = self.client.query_raw(sql, params).await?;
        pin_mut!(stream);

        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut complete = true;
        while let Some(item) = stream.next().await {
            let row: Row = item?;
            if columns.is_empty() {
                columns = row.columns().iter().map(|c| c.name().to_string()).collect();
            }
            if rows.len() >= max_rows {
                complete = false;
                break;
            }
            rows.push(
                (0..row.columns().len())
                    .map(|i| pg_value(&row, i))
                    .collect(),
            );
        }

        Ok(Grid {
            columns,
            rows,
            affected: None,
            elapsed_ms: started.elapsed().as_millis() as u64,
            complete,
        })
    }

    fn page_sql(&self, schema: &str, table: &str, limit: usize, offset: usize) -> String {
        format!(
            "select * from {}.{} limit {} offset {}",
            quote(schema),
            quote(table),
            limit,
            offset
        )
    }

    fn canceller(&self) -> super::Canceller {
        super::Canceller::Postgres(self.cancel.clone())
    }
}

fn join_arr<T: std::fmt::Display>(v: Vec<Option<T>>) -> String {
    let inner: Vec<String> = v
        .into_iter()
        .map(|x| x.map(|y| y.to_string()).unwrap_or_else(|| "NULL".into()))
        .collect();
    format!("{{{}}}", inner.join(","))
}

/// Renders one cell as text. `None` is SQL NULL.
fn pg_value(row: &Row, idx: usize) -> Option<String> {
    let ty = row.columns()[idx].type_().clone();

    macro_rules! g {
        ($t:ty) => {
            return match row.try_get::<_, Option<$t>>(idx) {
                Ok(v) => v.map(|x| x.to_string()),
                Err(e) => Some(format!("<err: {e}>")),
            }
        };
    }
    macro_rules! ga {
        ($t:ty) => {
            return match row.try_get::<_, Option<Vec<Option<$t>>>>(idx) {
                Ok(v) => v.map(join_arr),
                Err(e) => Some(format!("<err: {e}>")),
            }
        };
    }

    if ty == Type::BOOL {
        g!(bool);
    } else if ty == Type::INT2 {
        g!(i16);
    } else if ty == Type::INT4 {
        g!(i32);
    } else if ty == Type::INT8 {
        g!(i64);
    } else if ty == Type::OID {
        g!(u32);
    } else if ty == Type::FLOAT4 {
        g!(f32);
    } else if ty == Type::FLOAT8 {
        g!(f64);
    } else if ty == Type::NUMERIC {
        g!(rust_decimal::Decimal);
    } else if ty == Type::CHAR {
        g!(i8);
    } else if ty == Type::TEXT
        || ty == Type::VARCHAR
        || ty == Type::BPCHAR
        || ty == Type::NAME
        || ty == Type::UNKNOWN
    {
        g!(String);
    } else if ty == Type::UUID {
        g!(uuid::Uuid);
    } else if ty == Type::JSON || ty == Type::JSONB {
        g!(serde_json::Value);
    } else if ty == Type::TIMESTAMP {
        g!(chrono::NaiveDateTime);
    } else if ty == Type::TIMESTAMPTZ {
        g!(chrono::DateTime<chrono::Utc>);
    } else if ty == Type::DATE {
        g!(chrono::NaiveDate);
    } else if ty == Type::TIME {
        g!(chrono::NaiveTime);
    } else if ty == Type::BYTEA {
        return match row.try_get::<_, Option<Vec<u8>>>(idx) {
            Ok(v) => v.map(|b| {
                let hex: String = b.iter().take(64).map(|x| format!("{x:02x}")).collect();
                if b.len() > 64 {
                    format!("\\x{hex}\u{2026} ({} bytes)", b.len())
                } else {
                    format!("\\x{hex}")
                }
            }),
            Err(e) => Some(format!("<err: {e}>")),
        };
    } else if ty == Type::TEXT_ARRAY || ty == Type::VARCHAR_ARRAY {
        ga!(String);
    } else if ty == Type::INT2_ARRAY {
        ga!(i16);
    } else if ty == Type::INT4_ARRAY {
        ga!(i32);
    } else if ty == Type::INT8_ARRAY {
        ga!(i64);
    } else if ty == Type::BOOL_ARRAY {
        ga!(bool);
    } else if ty == Type::UUID_ARRAY {
        ga!(uuid::Uuid);
    }

    match row.try_get::<_, Option<String>>(idx) {
        Ok(v) => v,
        Err(_) => Some(format!("<{}>", ty.name())),
    }
}
