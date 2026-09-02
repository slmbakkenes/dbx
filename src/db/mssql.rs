//! Microsoft SQL Server driver (SQL logins only).

use super::{ColumnMeta, Driver, Grid};
use crate::config::Connection as ConnCfg;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::time::Instant;
use tiberius::{AuthMethod, ColumnData, Config as TdsConfig, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type TdsClient = tiberius::Client<Compat<TcpStream>>;

pub struct MsDriver {
    client: TdsClient,
}

pub fn quote(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

impl MsDriver {
    pub async fn connect(c: &ConnCfg) -> Result<Self> {
        let password = c.resolve_password()?;
        let mut cfg = TdsConfig::new();
        cfg.host(&c.host);
        cfg.port(c.port_or_default());
        cfg.database(&c.database);
        cfg.application_name("dbx");
        cfg.authentication(AuthMethod::sql_server(&c.user, &password));
        if c.trust_cert {
            cfg.trust_cert();
        }

        let tcp = TcpStream::connect(cfg.get_addr())
            .await
            .with_context(|| format!("connecting to {}:{}", c.host, c.port_or_default()))?;
        tcp.set_nodelay(true)?;
        let client = TdsClient::connect(cfg, tcp.compat_write())
            .await
            .context("SQL Server handshake failed")?;
        Ok(Self { client })
    }

    async fn strings(&mut self, sql: &str, param: Option<&str>) -> Result<Vec<String>> {
        let stream = match param {
            Some(p) => self.client.query(sql, &[&p]).await?,
            None => self.client.query(sql, &[]).await?,
        };
        let rows = stream.into_first_result().await?;
        Ok(rows
            .iter()
            .map(|r| r.get::<&str, _>(0).unwrap_or("").to_string())
            .collect())
    }
}

#[async_trait]
impl Driver for MsDriver {
    async fn schemas(&mut self) -> Result<Vec<String>> {
        self.strings(
            "select s.name from sys.schemas s \
             where s.name not in ('sys','INFORMATION_SCHEMA','guest') \
               and s.name not like 'db\\_%' escape '\\' \
             order by s.name",
            None,
        )
        .await
    }

    async fn tables(&mut self, schema: &str) -> Result<Vec<String>> {
        self.strings(
            "select o.name from sys.objects o \
             join sys.schemas s on s.schema_id = o.schema_id \
             where s.name = @P1 and o.type in ('U','V') \
             order by o.name",
            Some(schema),
        )
        .await
    }

    async fn columns(&mut self, schema: &str, table: &str) -> Result<Grid> {
        let sql = "select c.COLUMN_NAME, \
                          c.DATA_TYPE + coalesce('(' + \
                              case when c.CHARACTER_MAXIMUM_LENGTH = -1 then 'max' \
                                   else cast(c.CHARACTER_MAXIMUM_LENGTH as varchar(12)) end \
                              + ')', '') as data_type, \
                          c.IS_NULLABLE, \
                          c.COLUMN_DEFAULT, \
                          case when pk.COLUMN_NAME is null then 0 else 1 end as is_pk \
                   from INFORMATION_SCHEMA.COLUMNS c \
                   left join ( \
                     select k.COLUMN_NAME \
                     from INFORMATION_SCHEMA.TABLE_CONSTRAINTS t \
                     join INFORMATION_SCHEMA.KEY_COLUMN_USAGE k \
                       on k.CONSTRAINT_NAME = t.CONSTRAINT_NAME \
                      and k.TABLE_SCHEMA = t.TABLE_SCHEMA \
                     where t.CONSTRAINT_TYPE = 'PRIMARY KEY' \
                       and t.TABLE_SCHEMA = @P1 and t.TABLE_NAME = @P2 \
                   ) pk on pk.COLUMN_NAME = c.COLUMN_NAME \
                   where c.TABLE_SCHEMA = @P1 and c.TABLE_NAME = @P2 \
                   order by c.ORDINAL_POSITION";
        let rows = self
            .client
            .query(sql, &[&schema, &table])
            .await?
            .into_first_result()
            .await?;
        let metas: Vec<(ColumnMeta, bool)> = rows
            .iter()
            .map(|r| {
                (
                    ColumnMeta {
                        name: r.get::<&str, _>(0).unwrap_or("").to_string(),
                        data_type: r.get::<&str, _>(1).unwrap_or("").to_string(),
                        nullable: r.get::<&str, _>(2).unwrap_or("YES") == "YES",
                        default: r.get::<&str, _>(3).map(|s| s.to_string()),
                    },
                    r.get::<i32, _>(4).unwrap_or(0) == 1,
                )
            })
            .collect();
        Ok(super::structure_grid(&metas))
    }

    async fn query(&mut self, sql: &str, max_rows: usize) -> Result<Grid> {
        let started = Instant::now();
        if !super::returns_rows(sql) {
            let res = self.client.execute(sql, &[]).await?;
            let affected: u64 = res.rows_affected().iter().sum();
            return Ok(Grid {
                columns: vec![],
                rows: vec![],
                affected: Some(affected),
                elapsed_ms: started.elapsed().as_millis() as u64,
                complete: true,
            });
        }

        let sets = self.client.simple_query(sql).await?.into_results().await?;
        // Show the last non-empty result set, which is what the user typed last.
        let rows: Vec<Row> = sets
            .into_iter()
            .rfind(|s| !s.is_empty())
            .unwrap_or_default();

        let mut columns = Vec::new();
        if let Some(first) = rows.first() {
            columns = first
                .columns()
                .iter()
                .map(|c| c.name().to_string())
                .collect();
        }
        let complete = rows.len() <= max_rows;
        let out: Vec<Vec<Option<String>>> = rows
            .into_iter()
            .take(max_rows)
            .map(|r| r.into_iter().map(ms_value).collect())
            .collect();

        Ok(Grid {
            columns,
            rows: out,
            affected: None,
            elapsed_ms: started.elapsed().as_millis() as u64,
            complete,
        })
    }

    fn page_sql(&self, schema: &str, table: &str, limit: usize, offset: usize) -> String {
        format!(
            "select * from {}.{} order by (select null) offset {} rows fetch next {} rows only",
            quote(schema),
            quote(table),
            offset,
            limit
        )
    }

    fn canceller(&self) -> super::Canceller {
        // TDS has no out-of-band cancel in tiberius; the worker drops the
        // connection and reconnects instead.
        super::Canceller::Unsupported
    }
}

fn ms_value(cell: ColumnData<'static>) -> Option<String> {
    fn s<T: std::fmt::Display>(v: Option<T>) -> Option<String> {
        v.map(|x| x.to_string())
    }
    if matches!(
        cell,
        ColumnData::DateTime(_)
            | ColumnData::SmallDateTime(_)
            | ColumnData::Time(_)
            | ColumnData::Date(_)
            | ColumnData::DateTime2(_)
            | ColumnData::DateTimeOffset(_)
    ) {
        return ms_temporal(&cell);
    }
    match cell {
        ColumnData::U8(v) => s(v),
        ColumnData::I16(v) => s(v),
        ColumnData::I32(v) => s(v),
        ColumnData::I64(v) => s(v),
        ColumnData::F32(v) => s(v),
        ColumnData::F64(v) => s(v),
        ColumnData::Bit(v) => v.map(|b| if b { "true" } else { "false" }.to_string()),
        ColumnData::String(v) => v.map(|c| c.to_string()),
        ColumnData::Guid(v) => s(v),
        ColumnData::Numeric(v) => s(v),
        ColumnData::Xml(v) => v.map(|x| x.as_ref().to_string()),
        ColumnData::Binary(v) => v.map(|b| {
            let hex: String = b.iter().take(64).map(|x| format!("{x:02x}")).collect();
            if b.len() > 64 {
                format!("0x{hex}\u{2026} ({} bytes)", b.len())
            } else {
                format!("0x{hex}")
            }
        }),
        _ => None,
    }
}

/// Temporal types decode through chrono rather than Display.
fn ms_temporal(cell: &ColumnData<'static>) -> Option<String> {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    use tiberius::FromSql;

    if let Ok(Some(v)) = NaiveDateTime::from_sql(cell) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = DateTime::<FixedOffset>::from_sql(cell) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = NaiveDate::from_sql(cell) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = NaiveTime::from_sql(cell) {
        return Some(v.to_string());
    }
    None
}
