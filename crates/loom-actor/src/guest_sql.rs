//! Admission for SQL issued by behaviors, before it reaches the actor transaction.
use anyhow::{Context, Result, bail, ensure};
use turso_parser::{
    ast::{AlterTableBody, Cmd, Name, QualifiedName, Stmt},
    lexer::Lexer,
    parser::Parser,
    token::TokenType,
};

pub(crate) fn check(sql: &str) -> Result<()> {
    let mut parser = Parser::new(sql.as_bytes());
    let command = parser.next_cmd()?.context("guest SQL requires a statement")?;
    ensure!(parser.next_cmd()?.is_none(), "guest SQL refuses multiple statements");
    let statement = match command {
        Cmd::Stmt(statement) | Cmd::Explain(statement) | Cmd::ExplainQueryPlan(statement) => statement,
    };
    match statement {
        Stmt::Select(_) => {}
        Stmt::Insert { tbl_name, .. } | Stmt::Delete { tbl_name, .. } | Stmt::DropTable { tbl_name, .. } => writable(&tbl_name)?,
        Stmt::Update(update) => writable(&update.tbl_name)?,
        Stmt::CreateTable { tbl_name, temporary, .. } => {
            ensure!(!temporary, "guest SQL refuses temporary tables");
            writable(&tbl_name)?;
        }
        Stmt::AlterTable(alter) => {
            writable(&alter.name)?;
            if let AlterTableBody::RenameTo(name) = alter.body {
                domain_name(&name)?;
            }
        }
        Stmt::CreateIndex { idx_name, tbl_name, using, .. } => {
            writable(&idx_name)?;
            domain_name(&tbl_name)?;
            ensure!(using.is_none(), "guest SQL refuses index modules");
        }
        // Triggers and views can retain hidden references across later calls.
        // Their installation belongs to the host-reviewed behavior schema.
        statement => bail!("guest SQL refuses {}", forbidden_kind(&statement)),
    }
    check_functions(sql)
}

/// Admission for a behavior's `LOOM_SCHEMA`: every object it creates must carry a domain
/// name. Without this, `CREATE TABLE IF NOT EXISTS effects(...)` is a silent no-op against
/// the runtime table of that name, and the behavior's queries then read and write the
/// runtime's table (found by verify/harness, whose `calls` and `effects` collided).
pub(crate) fn check_schema(schema: &str) -> Result<()> {
    let mut parser = Parser::new(schema.as_bytes());
    while let Some(command) = parser.next_cmd()? {
        let (Cmd::Stmt(statement) | Cmd::Explain(statement) | Cmd::ExplainQueryPlan(statement)) = command;
        match statement {
            Stmt::CreateTable { tbl_name, .. } => writable(&tbl_name)?,
            Stmt::CreateIndex { idx_name, tbl_name, .. } => {
                writable(&idx_name)?;
                domain_name(&tbl_name)?;
            }
            Stmt::CreateView { view_name, .. } | Stmt::CreateMaterializedView { view_name, .. } => writable(&view_name)?,
            Stmt::CreateTrigger { trigger_name, tbl_name, .. } => {
                writable(&trigger_name)?;
                writable(&tbl_name)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn forbidden_kind(statement: &Stmt) -> &'static str {
    match statement {
        Stmt::Attach { .. } => "ATTACH",
        Stmt::Detach { .. } => "DETACH",
        Stmt::Pragma { .. } => "PRAGMA",
        Stmt::Begin { .. } => "BEGIN",
        Stmt::Commit { .. } => "COMMIT",
        Stmt::Rollback { .. } => "ROLLBACK",
        Stmt::Savepoint { .. } => "SAVEPOINT",
        Stmt::Release { .. } => "RELEASE",
        Stmt::Vacuum { .. } => "VACUUM",
        Stmt::Analyze { .. } => "ANALYZE",
        Stmt::Reindex { .. } => "REINDEX",
        Stmt::Optimize { .. } => "OPTIMIZE",
        Stmt::CreateTrigger { .. } => "CREATE TRIGGER",
        Stmt::CreateView { .. } => "CREATE VIEW",
        Stmt::CreateMaterializedView { .. } => "CREATE MATERIALIZED VIEW",
        Stmt::CreateVirtualTable(_) => "CREATE VIRTUAL TABLE",
        Stmt::CreateType { .. } => "CREATE TYPE",
        Stmt::CreateDomain { .. } => "CREATE DOMAIN",
        Stmt::CreateSequence { .. } => "CREATE SEQUENCE",
        Stmt::DropIndex { .. } => "DROP INDEX",
        Stmt::DropTrigger { .. } => "DROP TRIGGER",
        Stmt::DropView { .. } => "DROP VIEW",
        Stmt::DropType { .. } => "DROP TYPE",
        Stmt::DropDomain { .. } => "DROP DOMAIN",
        Stmt::DropSequence { .. } => "DROP SEQUENCE",
        _ => "statement outside domain SQL",
    }
}

fn writable(name: &QualifiedName) -> Result<()> {
    ensure!(
        name.db_name.as_ref().is_none_or(|name| name.as_str().eq_ignore_ascii_case("main")),
        "guest SQL refuses database qualification"
    );
    domain_name(&name.name)
}

fn domain_name(name: &Name) -> Result<()> {
    let name = name.as_str().to_ascii_lowercase();
    ensure!(
        !crate::schema::SYSTEM_TABLES.contains(&name.as_str())
            && name != "restarts"
            && !name.starts_with("sqlite_")
            && !name.starts_with("turso_"),
        "guest SQL refuses mutation of runtime object {name}"
    );
    Ok(())
}

pub(crate) fn check_functions(sql: &str) -> Result<()> {
    let mut previous = None;
    for token in Lexer::new(sql.as_bytes()) {
        let token = token?;
        if token.token_type.is_none() {
            continue;
        }
        if token.token_type == TokenType::TK_RBRACKET {
            continue;
        }
        if token.token_type == TokenType::TK_LP
            && let Some(name) = previous.as_deref()
        {
            ensure!(
                !matches!(name, "attach" | "detach" | "load_extension" | "readfile" | "writefile" | "edit" | "eval" | "nextval" | "setval")
                    && !name.starts_with("pragma_"),
                "guest SQL refuses function {name}"
            );
        }
        previous = if matches!(token.token_type, TokenType::TK_ID | TokenType::TK_STRING) {
            Some(Name::from_string(token.to_utf8()).as_str().to_ascii_lowercase())
        } else {
            // Keywords can also be accepted as function names by SQLite.
            Some(token.to_utf8().to_ascii_lowercase())
        };
    }
    Ok(())
}
