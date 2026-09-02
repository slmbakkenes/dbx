//! One task per connection. The UI never blocks on the database.

use super::{ConnId, Driver, Grid, ReqId, mssql::MsDriver, postgres::PgDriver};
use crate::config::{Connection as ConnCfg, Engine};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone)]
pub enum DbRequest {
    Schemas,
    Tables {
        schema: String,
    },
    Columns {
        schema: String,
        table: String,
        req: ReqId,
    },
    Query {
        sql: String,
        req: ReqId,
        max_rows: usize,
    },
    /// Paged `select *` whose SQL only the driver knows how to spell.
    TableData {
        schema: String,
        table: String,
        limit: usize,
        offset: usize,
        req: ReqId,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum DbPayload {
    Connected,
    Disconnected(String),
    Schemas(Vec<String>),
    Tables {
        schema: String,
        tables: Vec<String>,
    },
    Result {
        req: ReqId,
        grid: Grid,
        /// The SQL actually sent, so the UI can show and re-run it.
        sql: Option<String>,
    },
    Error {
        req: Option<ReqId>,
        message: String,
    },
    Cancelled {
        req: ReqId,
    },
}

#[derive(Debug, Clone)]
pub struct DbEvent {
    pub conn: ConnId,
    pub payload: DbPayload,
}

pub struct ConnHandle {
    pub req_tx: UnboundedSender<DbRequest>,
    pub cancel_tx: UnboundedSender<()>,
}

impl ConnHandle {
    pub fn send(&self, req: DbRequest) {
        let _ = self.req_tx.send(req);
    }
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(());
    }
}

pub fn spawn(id: ConnId, cfg: ConnCfg, events: UnboundedSender<DbEvent>) -> ConnHandle {
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(id, cfg, req_rx, cancel_rx, events));
    ConnHandle { req_tx, cancel_tx }
}

async fn connect(cfg: &ConnCfg) -> anyhow::Result<Box<dyn Driver>> {
    Ok(match cfg.engine {
        Engine::Postgres => Box::new(PgDriver::connect(cfg).await?) as Box<dyn Driver>,
        Engine::Mssql => Box::new(MsDriver::connect(cfg).await?) as Box<dyn Driver>,
    })
}

async fn run(
    id: ConnId,
    cfg: ConnCfg,
    mut req_rx: UnboundedReceiver<DbRequest>,
    mut cancel_rx: UnboundedReceiver<()>,
    events: UnboundedSender<DbEvent>,
) {
    let mut driver: Option<Box<dyn Driver>> = None;
    let emit = |payload: DbPayload| {
        let _ = events.send(DbEvent { conn: id, payload });
    };

    while let Some(req) = req_rx.recv().await {
        if matches!(req, DbRequest::Shutdown) {
            break;
        }
        // Drain stale cancel signals so they don't kill the next statement.
        while cancel_rx.try_recv().is_ok() {}

        let req_id = match &req {
            DbRequest::Columns { req, .. }
            | DbRequest::Query { req, .. }
            | DbRequest::TableData { req, .. } => Some(*req),
            _ => None,
        };

        if driver.is_none() {
            match connect(&cfg).await {
                Ok(d) => {
                    driver = Some(d);
                    emit(DbPayload::Connected);
                }
                Err(e) => {
                    emit(DbPayload::Error {
                        req: req_id,
                        message: format!("{e:#}"),
                    });
                    continue;
                }
            }
        }
        let d = driver.as_mut().expect("connected above");

        match req {
            DbRequest::Shutdown => {}
            DbRequest::Schemas => match d.schemas().await {
                Ok(v) => emit(DbPayload::Schemas(v)),
                Err(e) => emit(DbPayload::Error {
                    req: None,
                    message: format!("{e:#}"),
                }),
            },
            DbRequest::Tables { schema } => match d.tables(&schema).await {
                Ok(tables) => emit(DbPayload::Tables { schema, tables }),
                Err(e) => emit(DbPayload::Error {
                    req: None,
                    message: format!("{e:#}"),
                }),
            },
            DbRequest::Columns { schema, table, req } => match d.columns(&schema, &table).await {
                Ok(grid) => emit(DbPayload::Result {
                    req,
                    grid,
                    sql: None,
                }),
                Err(e) => emit(DbPayload::Error {
                    req: Some(req),
                    message: format!("{e:#}"),
                }),
            },
            DbRequest::TableData {
                schema,
                table,
                limit,
                offset,
                req,
            } => {
                let sql = d.page_sql(&schema, &table, limit, offset);
                let canceller = d.canceller();
                let mut dropped = false;
                tokio::select! {
                    res = d.query(&sql, limit) => match res {
                        Ok(grid) => emit(DbPayload::Result { req, grid, sql: Some(sql) }),
                        Err(e) => emit(DbPayload::Error { req: Some(req), message: format!("{e:#}") }),
                    },
                    _ = cancel_rx.recv() => {
                        dropped = !canceller.cancel().await;
                        emit(DbPayload::Cancelled { req });
                    }
                }
                if dropped {
                    driver = None;
                    emit(DbPayload::Disconnected("reconnecting after cancel".into()));
                }
            }
            DbRequest::Query { sql, req, max_rows } => {
                if cfg.read_only && !super::is_read_only(&sql) {
                    emit(DbPayload::Error {
                        req: Some(req),
                        message: "connection is marked read_only".into(),
                    });
                    continue;
                }
                let canceller = d.canceller();
                let mut dropped = false;
                tokio::select! {
                    res = d.query(&sql, max_rows) => match res {
                        Ok(grid) => emit(DbPayload::Result { req, grid, sql: Some(sql.clone()) }),
                        Err(e) => emit(DbPayload::Error { req: Some(req), message: format!("{e:#}") }),
                    },
                    _ = cancel_rx.recv() => {
                        dropped = !canceller.cancel().await;
                        emit(DbPayload::Cancelled { req });
                    }
                }
                if dropped {
                    // Engine cannot cancel in-band; the socket is now out of
                    // sync, so force a reconnect on the next request.
                    driver = None;
                    emit(DbPayload::Disconnected(
                        "reconnecting after cancel".to_string(),
                    ));
                }
            }
        }
    }
}
