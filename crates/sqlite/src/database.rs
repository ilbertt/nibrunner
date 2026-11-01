use std::collections::BTreeSet;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::{
    HranaColumn, HranaDescribeColumn, HranaDescribeResult, HranaParameter, HranaStatement,
    HranaStatementResult, SQLITE_MAX_BODY_BYTES, SQLITE_MAX_COLUMNS, SQLITE_MAX_PARAMETERS, SQLITE_MAX_ROWS,
    SQLITE_MAX_STATEMENT_BYTES,
};
use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::{Batch, Connection, OpenFlags, Statement};

use crate::budget::Budget;
use crate::{values, QueryError};

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_HEAP_BYTES: i64 = 8 * 1024 * 1024;

pub struct Database {
    connection: Connection,
    deadline: Arc<Mutex<Instant>>,
}

fn allowed_pragma(name: &str, argument: Option<&str>) -> bool {
    [
        "table_info",
        "table_xinfo",
        "index_list",
        "index_info",
        "index_xinfo",
        "foreign_key_list",
        "table_list",
    ]
    .iter()
    .any(|allowed| name.eq_ignore_ascii_case(allowed))
        || (argument.is_none()
            && [
                "database_list",
                "schema_version",
                "user_version",
                "encoding",
                "query_only",
            ]
            .iter()
            .any(|allowed| name.eq_ignore_ascii_case(allowed)))
}

fn authorize(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Recursive
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. } => Authorization::Allow,
        AuthAction::Function { function_name }
            if !["load_extension", "readfile", "writefile"]
                .iter()
                .any(|denied| function_name.eq_ignore_ascii_case(denied)) =>
        {
            Authorization::Allow
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } if allowed_pragma(pragma_name, pragma_value) => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

#[allow(
    unsafe_code,
    reason = "Each guest worker owns its SQLite process and bounds its allocator before opening a database."
)]
fn limit_heap() {
    unsafe {
        rusqlite::ffi::sqlite3_hard_heap_limit64(SQLITE_HEAP_BYTES);
    }
}

fn check_file(path: &Path) -> Result<(), QueryError> {
    let cannot_open = || QueryError::new("SQLITE_CANTOPEN", "The database file could not be opened.");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| cannot_open())?;
    if !file.metadata().map_err(|_| cannot_open())?.is_file() {
        return Err(cannot_open());
    }
    let mut header = [0; 16];
    if file.read_exact(&mut header).is_err() || header != *b"SQLite format 3\0" {
        return Err(QueryError::new(
            "SQLITE_NOTADB",
            "The file is not a SQLite database.",
        ));
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match std::fs::symlink_metadata(name) {
            Ok(details) if !details.is_file() => return Err(cannot_open()),
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(cannot_open()),
            _ => {}
        }
    }
    Ok(())
}

fn columns(statement: &Statement<'_>) -> Vec<HranaColumn> {
    statement
        .columns()
        .into_iter()
        .map(|column| HranaColumn {
            name: Some(column.name().into()),
            decltype: column.decl_type().map(String::from),
        })
        .collect()
}

fn bind(statement: &mut Statement<'_>, request: &HranaStatement) -> Result<(), QueryError> {
    if request.args.len() + request.named_args.len() > SQLITE_MAX_PARAMETERS {
        return Err(QueryError::too_large());
    }
    let mut bound = BTreeSet::new();
    for (index, value) in request.args.iter().enumerate() {
        statement.raw_bind_parameter(index + 1, values::argument(value)?)?;
        bound.insert(index + 1);
    }
    for argument in &request.named_args {
        let mut index = None;
        for prefix in ["", ":", "$", "@"] {
            if let Some(found) = statement.parameter_index(&format!("{prefix}{}", argument.name))? {
                index = Some(found);
                break;
            }
        }
        let index =
            index.ok_or_else(|| QueryError::new("SQLITE_RANGE", "A named parameter does not exist."))?;
        bound.insert(index);
        statement.raw_bind_parameter(index, values::argument(&argument.value)?)?;
    }
    if bound.len() != statement.parameter_count() {
        return Err(QueryError::new(
            "SQLITE_RANGE",
            "A required parameter was not supplied.",
        ));
    }
    Ok(())
}

impl Database {
    pub fn open(
        path: &Path,
        mut interrupted: impl FnMut() -> bool + Send + 'static,
    ) -> Result<Self, QueryError> {
        check_file(path)?;
        limit_heap();
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        )?;
        connection.busy_timeout(Duration::from_millis(250))?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, SQLITE_MAX_BODY_BYTES as i32);
        connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, SQLITE_MAX_STATEMENT_BYTES as i32);
        connection.set_limit(Limit::SQLITE_LIMIT_COLUMN, SQLITE_MAX_COLUMNS as i32);
        connection.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, SQLITE_MAX_PARAMETERS as i32);
        connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
        connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
        connection.pragma_update(None, "query_only", true)?;
        connection.pragma_update(None, "cache_size", -1024)?;
        // SELECT can spill temporary sort files; keep that work inside the bounded worker heap.
        connection.pragma_update(None, "temp_store", 2)?;
        connection.authorizer(Some(authorize));
        let deadline = Arc::new(Mutex::new(Instant::now() + QUERY_TIMEOUT));
        let progress = deadline.clone();
        connection.progress_handler(
            1000,
            Some(move || {
                interrupted()
                    || Instant::now() >= *progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            }),
        );
        connection
            .prepare("SELECT name FROM sqlite_schema LIMIT 1")?
            .query([])?
            .next()?;
        Ok(Self { connection, deadline })
    }

    pub fn begin_request(&mut self) {
        *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now() + QUERY_TIMEOUT;
    }

    fn prepare_single(&self, sql: &str) -> Result<Statement<'_>, QueryError> {
        if sql.len() > SQLITE_MAX_STATEMENT_BYTES || sql.contains('\0') {
            return Err(QueryError::too_large());
        }
        let mut batch = Batch::new(&self.connection, sql);
        let statement = batch.next()?.ok_or_else(QueryError::protocol)?;
        if batch.next()?.is_some() {
            return Err(QueryError::protocol());
        }
        Ok(statement)
    }

    pub fn execute(
        &mut self,
        sql: &str,
        request: &HranaStatement,
    ) -> Result<HranaStatementResult, QueryError> {
        let mut statement = self.prepare_single(sql)?;
        if !statement.readonly() {
            return Err(QueryError::new(
                "SQLITE_READONLY",
                "The database connection is read-only.",
            ));
        }
        bind(&mut statement, request)?;
        let cols = columns(&statement);
        let mut budget = Budget::new();
        budget.take(128)?;
        budget.charge(&cols)?;
        let mut result = Vec::new();
        let mut rows = statement.raw_query();
        while let Some(row) = rows.next()? {
            if request.want_rows == Some(false) {
                continue;
            }
            if result.len() >= SQLITE_MAX_ROWS {
                return Err(QueryError::too_large());
            }
            let mut values = Vec::new();
            for index in 0..cols.len() {
                let value = values::measured(row.get_ref(index)?)?;
                budget.charge(&value)?;
                budget.take(1)?;
                values.push(value);
            }
            budget.take(2)?;
            result.push(values);
        }
        Ok(HranaStatementResult {
            cols,
            rows: result,
            affected_row_count: 0,
            last_insert_rowid: None,
        })
    }

    pub fn describe(&self, sql: &str) -> Result<HranaDescribeResult, QueryError> {
        let statement = self.prepare_single(sql)?;
        Ok(HranaDescribeResult {
            params: (1..=statement.parameter_count())
                .map(|index| HranaParameter {
                    name: statement.parameter_name(index).map(String::from),
                })
                .collect(),
            cols: statement
                .columns()
                .into_iter()
                .map(|column| HranaDescribeColumn {
                    name: column.name().into(),
                    decltype: column.decl_type().map(String::from),
                })
                .collect(),
            is_explain: statement.is_explain() != 0,
            is_readonly: statement.readonly(),
        })
    }

    pub fn sequence(&mut self, sql: &str) -> Result<(), QueryError> {
        if sql.len() > SQLITE_MAX_STATEMENT_BYTES || sql.contains('\0') {
            return Err(QueryError::too_large());
        }
        let mut batch = Batch::new(&self.connection, sql);
        while let Some(mut statement) = batch.next()? {
            if !statement.readonly() {
                return Err(QueryError::new(
                    "SQLITE_READONLY",
                    "The database connection is read-only.",
                ));
            }
            let mut rows = statement.raw_query();
            while rows.next()?.is_some() {}
        }
        Ok(())
    }
}
