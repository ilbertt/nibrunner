use std::collections::BTreeMap;

use protocol::{
    HranaBatch, HranaBatchCondition, HranaBatchResult, HranaPipelineRequest, HranaPipelineResponse,
    HranaRequest, HranaResponse, HranaSql, HranaStatement, HranaStreamResult, SQLITE_MAX_BATCH_STEPS,
    SQLITE_MAX_PARAMETERS, SQLITE_MAX_PIPELINE_REQUESTS, SQLITE_MAX_STATEMENT_BYTES,
};

use crate::budget::Budget;
use crate::{Database, QueryError};

const MAX_STORED_SQL: usize = 128;
const MAX_STORED_BYTES: usize = 1_048_576;
const MAX_CONDITION_DEPTH: usize = 32;
const MAX_CONDITION_NODES: usize = 16_384;

pub struct Session {
    database: Option<Database>,
    baton: Option<String>,
    sql: BTreeMap<i32, String>,
}

fn validate_source(sql: &HranaSql) -> Result<(), QueryError> {
    match (&sql.sql, sql.sql_id) {
        (Some(text), None) if text.len() <= SQLITE_MAX_STATEMENT_BYTES && !text.contains('\0') => Ok(()),
        (None, Some(_)) => Ok(()),
        _ => Err(QueryError::protocol()),
    }
}

fn statement(request: &HranaStatement) -> Result<(), QueryError> {
    validate_source(&request.source)?;
    if request.args.len() + request.named_args.len() > SQLITE_MAX_PARAMETERS {
        return Err(QueryError::too_large());
    }
    Ok(())
}

fn condition_valid(
    condition: &HranaBatchCondition,
    index: i32,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), QueryError> {
    *nodes += 1;
    if depth > MAX_CONDITION_DEPTH || *nodes > MAX_CONDITION_NODES {
        return Err(QueryError::too_large());
    }
    match condition {
        HranaBatchCondition::Ok { step } | HranaBatchCondition::Error { step }
            if *step >= 0 && *step < index =>
        {
            Ok(())
        }
        HranaBatchCondition::Not { cond } => condition_valid(cond, index, depth + 1, nodes),
        HranaBatchCondition::And { conds } | HranaBatchCondition::Or { conds } => {
            for condition in conds {
                condition_valid(condition, index, depth + 1, nodes)?;
            }
            Ok(())
        }
        _ => Err(QueryError::protocol()),
    }
}

fn validate(request: &HranaRequest, nodes: &mut usize) -> Result<(), QueryError> {
    match request {
        HranaRequest::Execute { stmt } => statement(stmt),
        HranaRequest::Describe { source: sql } | HranaRequest::Sequence { source: sql } => {
            validate_source(sql)
        }
        HranaRequest::Batch { batch } => {
            if batch.steps.len() > SQLITE_MAX_BATCH_STEPS {
                return Err(QueryError::too_large());
            }
            for (index, step) in batch.steps.iter().enumerate() {
                statement(&step.stmt)?;
                if let Some(condition) = &step.condition {
                    condition_valid(condition, index as i32, 0, nodes)?;
                }
            }
            Ok(())
        }
        HranaRequest::StoreSql { sql, .. }
            if sql.len() > SQLITE_MAX_STATEMENT_BYTES || sql.contains('\0') =>
        {
            Err(QueryError::too_large())
        }
        _ => Ok(()),
    }
}

fn enabled(condition: &HranaBatchCondition, result: &HranaBatchResult) -> bool {
    match condition {
        HranaBatchCondition::Ok { step } => result
            .step_results
            .get(*step as usize)
            .is_some_and(Option::is_some),
        HranaBatchCondition::Error { step } => result
            .step_errors
            .get(*step as usize)
            .is_some_and(Option::is_some),
        HranaBatchCondition::Not { cond } => !enabled(cond, result),
        HranaBatchCondition::And { conds } => conds.iter().all(|cond| enabled(cond, result)),
        HranaBatchCondition::Or { conds } => conds.iter().any(|cond| enabled(cond, result)),
    }
}

impl Session {
    pub fn new(database: Database) -> Self {
        Self {
            database: Some(database),
            baton: None,
            sql: BTreeMap::new(),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.database.is_none()
    }

    fn database(&mut self) -> Result<&mut Database, QueryError> {
        self.database.as_mut().ok_or_else(QueryError::protocol)
    }

    fn resolve(&self, source: &HranaSql) -> Result<String, QueryError> {
        validate_source(source)?;
        match (&source.sql, source.sql_id) {
            (Some(text), None) => Ok(text.clone()),
            (None, Some(id)) => self.sql.get(&id).cloned().ok_or_else(QueryError::protocol),
            _ => Err(QueryError::protocol()),
        }
    }

    fn execute(&mut self, request: &HranaStatement) -> Result<protocol::HranaStatementResult, QueryError> {
        let sql = self.resolve(&request.source)?;
        self.database()?.execute(&sql, request)
    }

    fn batch(&mut self, batch: HranaBatch) -> Result<HranaBatchResult, QueryError> {
        let mut result = HranaBatchResult {
            step_results: vec![None; batch.steps.len()],
            step_errors: vec![None; batch.steps.len()],
        };
        let mut budget = Budget::new();
        for (index, step) in batch.steps.into_iter().enumerate() {
            if step
                .condition
                .as_ref()
                .is_some_and(|condition| !enabled(condition, &result))
            {
                continue;
            }
            match self.execute(&step.stmt) {
                Ok(value) => {
                    budget.charge(&value)?;
                    budget.take(32)?;
                    result.step_results[index] = Some(value);
                }
                Err(error) => {
                    let value = error.wire();
                    budget.charge(&value)?;
                    budget.take(32)?;
                    result.step_errors[index] = Some(value);
                }
            }
        }
        Ok(result)
    }

    fn request(&mut self, request: HranaRequest) -> Result<HranaResponse, QueryError> {
        self.database()?;
        match request {
            HranaRequest::Execute { stmt } => Ok(HranaResponse::Execute {
                result: self.execute(&stmt)?,
            }),
            HranaRequest::Batch { batch } => Ok(HranaResponse::Batch {
                result: self.batch(batch)?,
            }),
            HranaRequest::Describe { source } => {
                let sql = self.resolve(&source)?;
                Ok(HranaResponse::Describe {
                    result: self.database()?.describe(&sql)?,
                })
            }
            HranaRequest::Sequence { source } => {
                let sql = self.resolve(&source)?;
                self.database()?.sequence(&sql)?;
                Ok(HranaResponse::Sequence)
            }
            HranaRequest::StoreSql { sql_id, sql } => {
                let bytes: usize = self.sql.values().map(String::len).sum();
                if self.sql.contains_key(&sql_id) {
                    return Err(QueryError::protocol());
                }
                if self.sql.len() >= MAX_STORED_SQL || bytes + sql.len() > MAX_STORED_BYTES {
                    return Err(QueryError::too_large());
                }
                self.sql.insert(sql_id, sql);
                Ok(HranaResponse::StoreSql)
            }
            HranaRequest::CloseSql { sql_id } => {
                self.sql.remove(&sql_id);
                Ok(HranaResponse::CloseSql)
            }
            HranaRequest::Close => {
                self.database.take();
                self.sql.clear();
                Ok(HranaResponse::Close)
            }
        }
    }

    pub fn pipeline(&mut self, request: HranaPipelineRequest) -> Result<HranaPipelineResponse, QueryError> {
        if request.baton != self.baton || request.requests.len() > SQLITE_MAX_PIPELINE_REQUESTS {
            return Err(QueryError::protocol());
        }
        let mut nodes = 0;
        for request in &request.requests {
            validate(request, &mut nodes)?;
        }
        self.database()?.begin_request();
        self.baton = Some(uuid::Uuid::new_v4().simple().to_string());
        let mut budget = Budget::new();
        budget.take(128)?;
        let mut results = Vec::new();
        for request in request.requests {
            let result = match self.request(request) {
                Ok(response) => HranaStreamResult::Ok { response },
                Err(error) => HranaStreamResult::Error { error: error.wire() },
            };
            if budget.charge(&result).and_then(|()| budget.take(1)).is_err() {
                self.database.take();
                self.sql.clear();
                self.baton = None;
                return Err(QueryError::too_large());
            }
            results.push(result);
        }
        Ok(HranaPipelineResponse {
            baton: if self.is_closed() {
                None
            } else {
                self.baton.clone()
            },
            base_url: None,
            results,
        })
    }
}
