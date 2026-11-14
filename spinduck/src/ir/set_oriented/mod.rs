//! 受限静态游标搬运集合化；原始 IR 不变，每次调用在同一事务内重新证明。

mod schema;
mod shape;

use super::{AssignTarget, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, VarName};
use crate::catalog::CompiledRoutine;
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use crate::runtime::cursor::CursorAttrKind;

/// 未获得集合化证明的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    UnsupportedControlFlow,
    ExceptionScopePresent,
    TransactionBoundaryInsideLoop,
    BatchCommitEnabled,
    LiveOutState,
    UnsupportedWrite,
    TargetReadDependency,
    UnknownObjectDependency,
    UnprovenExpression,
    UnprovenTargetConstraints,
    UnsupportedCall,
    DynamicSql,
}

/// 一次调用实际选择的优化状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Converted { sql: String },
    Skipped { reason: SkipReason },
}

/// 原始例程和独立优化产物的对应报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub routine: String,
    pub status: Status,
}

struct CopyLoop<'a> {
    query: &'a str,
    insert: &'a str,
    binds: &'a [(String, BindSrc)],
    record: &'a VarName,
}

pub(crate) fn optimize(
    routine: &CompiledRoutine,
    ex: &dyn SqlExecutor,
    batch: bool,
) -> Result<(IrProg, Report), EngineError> {
    let selected = if batch {
        Err(SkipReason::BatchCommitEnabled)
    } else {
        match analyze(&routine.prog) {
            Ok(region)
                if routine
                    .params
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(&region.record.0)) =>
            {
                Err(SkipReason::LiveOutState)
            }
            Ok(region) => generate(&region, ex)?,
            Err(reason) => Err(reason),
        }
    };
    let (prog, status) = match selected {
        Ok(sql) => (
            IrProg {
                code: vec![IrStmt::SqlExec {
                    sql: sql.clone(),
                    binds: Vec::new(),
                    fn_sites: Vec::new(),
                }],
                var_types: routine.prog.var_types.clone(),
            },
            Status::Converted { sql },
        ),
        Err(reason) => (routine.prog.clone(), Status::Skipped { reason }),
    };
    Ok((
        prog,
        Report {
            routine: routine.name.clone(),
            status,
        },
    ))
}

fn reject_effects(code: &[IrStmt]) -> Result<(), SkipReason> {
    for stmt in code {
        match stmt {
            IrStmt::TxCommit | IrStmt::TxRollback => {
                return Err(SkipReason::TransactionBoundaryInsideLoop)
            }
            IrStmt::PushTrap { .. } | IrStmt::PopTrap | IrStmt::Raise { .. } => {
                return Err(SkipReason::ExceptionScopePresent)
            }
            IrStmt::CallRoutine { .. } => return Err(SkipReason::UnsupportedCall),
            IrStmt::SqlExecDynamic { .. }
            | IrStmt::OpenCursor {
                source: CursorSource::Dynamic { .. },
                ..
            } => return Err(SkipReason::DynamicSql),
            // 余下指令只在完整的闭合区域模式内获得资格。
            _ => {}
        }
    }
    Ok(())
}

fn analyze(prog: &IrProg) -> Result<CopyLoop<'_>, SkipReason> {
    reject_effects(&prog.code)?;
    let [IrStmt::OpenCursor {
        cursor,
        source:
            CursorSource::Static {
                sql: query,
                binds: query_binds,
                fn_sites: query_sites,
            },
    }, IrStmt::Fetch {
        cursor: fetched,
        targets,
        record: true,
    }, IrStmt::JumpIf {
        cond:
            IrExpr::CursorAttr {
                cursor: tested,
                kind: CursorAttrKind::NotFound,
            },
        to,
    }, IrStmt::SqlExec {
        sql: insert,
        binds,
        fn_sites,
    }, IrStmt::Jump(1), IrStmt::CloseCursor { cursor: closed }] = prog.code.as_slice()
    else {
        return Err(SkipReason::UnsupportedControlFlow);
    };
    let [AssignTarget::Var(record)] = targets.as_slice() else {
        return Err(SkipReason::LiveOutState);
    };
    if cursor != fetched || cursor != tested || cursor != closed || *to != 6 {
        return Err(SkipReason::UnsupportedControlFlow);
    }
    if !query_sites.is_empty() || !fn_sites.is_empty() {
        return Err(SkipReason::UnsupportedCall);
    }
    if !query_binds.is_empty() || prog.var_types.contains_key(&record.0) {
        return Err(SkipReason::UnprovenExpression);
    }
    Ok(CopyLoop {
        query,
        insert,
        binds,
        record,
    })
}

fn generate(
    region: &CopyLoop<'_>,
    ex: &dyn SqlExecutor,
) -> Result<Result<String, SkipReason>, EngineError> {
    let shape = shape::select(region.query)
        .and_then(|select| shape::insert(region.insert).map(|insert| (select, insert)));
    let (select, insert) = match shape {
        Ok(shape) => shape,
        Err(reason) => return Ok(Err(reason)),
    };
    if select.table == insert.table {
        return Ok(Err(SkipReason::TargetReadDependency));
    }
    let Some(source) = schema::load(ex, &select.table)? else {
        return Ok(Err(SkipReason::UnknownObjectDependency));
    };
    let Some(target) = schema::load(ex, &insert.table)? else {
        return Ok(Err(SkipReason::UnprovenTargetConstraints));
    };
    Ok(copy_sql(region, &select, &insert, &source, &target))
}

fn copy_sql(
    region: &CopyLoop<'_>,
    select: &shape::Select,
    insert: &shape::Insert,
    source: &schema::Table,
    target: &schema::Table,
) -> Result<String, SkipReason> {
    if insert.columns.len() != target.columns.len()
        || insert.parameters.len() != insert.columns.len()
        || region.binds.len() != insert.parameters.len()
    {
        return Err(SkipReason::UnprovenTargetConstraints);
    }
    let mut projections = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (name, parameter) in insert.columns.iter().zip(&insert.parameters) {
        if !seen.insert(&name.0) {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
        let Some((_, BindSrc::Field { rec, field })) =
            region.binds.iter().find(|(p, _)| p == parameter)
        else {
            return Err(SkipReason::UnprovenExpression);
        };
        let field = shape::Name(field.clone());
        if rec != region.record || select.columns.iter().filter(|c| **c == field).count() != 1 {
            return Err(SkipReason::UnprovenExpression);
        }
        let from = source.columns.iter().find(|c| c.name == field);
        let to = target.columns.iter().find(|c| c.name == *name);
        let (Some(from), Some(to)) = (from, to) else {
            return Err(SkipReason::UnknownObjectDependency);
        };
        if from.kind != to.kind || (from.nullable && !to.nullable) {
            return Err(SkipReason::UnprovenTargetConstraints);
        }
        projections.push(format!("sd_cur.{}", field.sql()));
    }
    if select
        .columns
        .iter()
        .any(|n| !source.columns.iter().any(|c| c.name == *n))
    {
        return Err(SkipReason::UnknownObjectDependency);
    }
    Ok(format!(
        "WITH sd_cur AS (SELECT {} FROM {}) INSERT INTO {} ({}) SELECT {} FROM sd_cur",
        render(&select.columns),
        source.qualified,
        target.qualified,
        render(&insert.columns),
        projections.join(", ")
    ))
}

fn render(names: &[shape::Name]) -> String {
    names
        .iter()
        .map(shape::Name::sql)
        .collect::<Vec<_>>()
        .join(", ")
}
