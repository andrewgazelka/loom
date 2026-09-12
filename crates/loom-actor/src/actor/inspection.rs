use super::query;
use crate::Rows;
use anyhow::{Context, Result, ensure};
use turso::{Connection, IntoParams};

pub(crate) async fn inspect_query(conn: &Connection, sql: &str, params: impl IntoParams) -> Result<Rows> {
    inspect_statement(sql)?;
    query(conn, sql, params).await
}

pub(crate) fn inspect_statement(sql: &str) -> Result<()> {
    crate::guest_sql::check_functions(sql)?;
    use turso_parser::{
        ast::{Cmd, Stmt},
        parser::Parser,
    };
    let mut parser = Parser::new(sql.as_bytes());
    let command = parser.next_cmd()?.context("empty SQL statement")?;
    let statement = match command {
        Cmd::Stmt(statement) | Cmd::Explain(statement) | Cmd::ExplainQueryPlan(statement) => statement,
    };
    let kind = match statement {
        Stmt::Select(_) => None,
        Stmt::Insert { .. } => Some("insert"),
        Stmt::Update(_) => Some("update"),
        Stmt::Delete { .. } => Some("delete"),
        Stmt::Pragma { .. } => Some("pragma"),
        Stmt::Attach { .. } => Some("attach"),
        Stmt::Detach { .. } => Some("detach"),
        Stmt::Begin { .. } => Some("begin"),
        Stmt::Commit { .. } => Some("commit"),
        Stmt::Rollback { .. } => Some("rollback"),
        Stmt::Savepoint { .. } => Some("savepoint"),
        Stmt::Release { .. } => Some("release"),
        Stmt::Vacuum { .. } => Some("vacuum"),
        Stmt::Analyze { .. } => Some("analyze"),
        Stmt::Reindex { .. } => Some("reindex"),
        Stmt::Optimize { .. } => Some("optimize"),
        Stmt::AlterTable(_) => Some("alter table"),
        Stmt::CreateIndex { .. } => Some("create index"),
        Stmt::CreateTable { .. } => Some("create table"),
        Stmt::CreateTrigger { .. } => Some("create trigger"),
        Stmt::CreateView { .. } => Some("create view"),
        Stmt::CreateMaterializedView { .. } => Some("create materialized view"),
        Stmt::CreateVirtualTable(_) => Some("create virtual table"),
        Stmt::CreateType { .. } => Some("create type"),
        Stmt::CreateDomain { .. } => Some("create domain"),
        Stmt::CreateSequence { .. } => Some("create sequence"),
        Stmt::DropIndex { .. } => Some("drop index"),
        Stmt::DropTable { .. } => Some("drop table"),
        Stmt::DropTrigger { .. } => Some("drop trigger"),
        Stmt::DropView { .. } => Some("drop view"),
        Stmt::DropType { .. } => Some("drop type"),
        Stmt::DropDomain { .. } => Some("drop domain"),
        Stmt::DropSequence { .. } => Some("drop sequence"),
    };
    if let Some(kind) = kind {
        anyhow::bail!("read-only inspection refuses {kind} statement");
    }
    ensure!(parser.next_cmd()?.is_none(), "read-only inspection refuses multiple statements");
    Ok(())
}
