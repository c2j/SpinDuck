//! 例程目录（§3.6）：已编译过程/函数的注册表与 CALL 结果契约。
//! 持久化（DuckDB 内部表）在 M7 接入；本模块提供内存目录与编译入口。

use crate::error::EngineError;
use crate::ir::{compile_routine_with_names, IrProg};
use crate::rules::RuleRegistry;
use crate::value::SpinValue;
use ogsql_parser::ast::Statement;
use std::collections::BTreeMap;

/// 参数模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamMode {
    In,
    Out,
    InOut,
}

/// 例程参数。
#[derive(Debug, Clone)]
pub struct RoutineParam {
    pub name: String,
    pub mode: ParamMode,
}

/// 已编译例程。
#[derive(Debug, Clone)]
pub struct CompiledRoutine {
    pub name: String,
    pub kind: RoutineKind,
    pub params: Vec<RoutineParam>,
    pub prog: IrProg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineKind {
    Procedure,
    Function,
}

/// CALL 结果（§12 P5 错误契约：OUT 参数按名取回，返回标量直接取）。
#[derive(Debug, Clone, Default)]
pub struct CallResult {
    pub outs: BTreeMap<String, SpinValue>,
    pub return_value: Option<SpinValue>,
}

/// 内存例程目录。
#[derive(Debug, Default, Clone)]
pub struct RoutineCatalog {
    routines: BTreeMap<String, CompiledRoutine>,
    /// SQL 方言规则（函数改名/`@` 变换标记表）：动态 SQL 运行期改写需要
    sql_functions: BTreeMap<String, String>,
    /// 完整规则注册表（patterns 开关等）：动态路径与静态语句同口径
    sql_rules: crate::rules::RuleRegistry,
}

impl RoutineCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入 SQL 方言规则函数表（Engine 构造时同步）。
    pub fn set_sql_functions(&mut self, funcs: BTreeMap<String, String>) {
        self.sql_functions = funcs;
    }

    /// 注入完整规则注册表（patterns 开关；动态路径同口径）。
    pub fn set_sql_rules(&mut self, rules: crate::rules::RuleRegistry) {
        self.sql_functions = rules.functions.clone();
        self.sql_rules = rules;
    }

    /// 完整规则注册表（动态路径 patterns/函数同口径）。
    pub fn sql_rules(&self) -> &crate::rules::RuleRegistry {
        &self.sql_rules
    }

    /// SQL 方言规则函数表（动态 SQL 改写用）。
    pub fn sql_functions(&self) -> &BTreeMap<String, String> {
        &self.sql_functions
    }

    pub fn register(&mut self, r: CompiledRoutine) {
        self.routines.insert(r.name.to_ascii_lowercase(), r);
    }

    pub fn lookup(&self, name: &str) -> Option<&CompiledRoutine> {
        self.routines.get(&name.to_ascii_lowercase())
    }

    /// 已注册例程名集合（SQL 上下文调用下沉判定用；小写）。
    pub fn routine_names(&self) -> std::collections::BTreeSet<String> {
        self.routines.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.routines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routines.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.routines.keys().cloned().collect()
    }
}

/// ogsql `RoutineParam.mode: Option<String>` → ParamMode。
pub fn map_mode_pub(mode: &Option<String>) -> Result<ParamMode, EngineError> {
    map_mode(mode)
}

/// 参数模式解析：`IN OUT`/`INOUT` 必须归 [`ParamMode::InOut`]（回归守护：
/// `contains("OUT")` 曾使 IN OUT 被归类为 OUT，丢入参值）；未知模式显式拒绝。
fn map_mode(mode: &Option<String>) -> Result<ParamMode, EngineError> {
    let raw = mode.as_deref().unwrap_or("IN");
    let compact = raw.to_ascii_uppercase().replace([' ', '_'], "");
    match compact.as_str() {
        "" | "IN" => Ok(ParamMode::In),
        "OUT" => Ok(ParamMode::Out),
        "INOUT" => Ok(ParamMode::InOut),
        _ => Err(EngineError::business(
            130,
            format!("unknown parameter mode: {raw:?}"),
        )),
    }
}

/// 从 ogsql AST 语句编译例程并登记（CREATE PROCEDURE / CREATE FUNCTION）。
/// PACKAGE spec/body 的展开在 engine 层完成。
pub fn compile_statement_routine(
    st: &Statement,
    rules: &RuleRegistry,
) -> Result<Vec<CompiledRoutine>, EngineError> {
    compile_statement_routine_with_names(st, rules, &Default::default())
}

/// 带已注册例程名集（SQL 上下文调用下沉判定用）。
pub fn compile_statement_routine_with_names(
    st: &Statement,
    rules: &RuleRegistry,
    routine_names: &std::collections::BTreeSet<String>,
) -> Result<Vec<CompiledRoutine>, EngineError> {
    let mut out = Vec::new();
    match st {
        Statement::CreateProcedure(cs) => {
            let extra: Vec<String> = cs
                .parameters
                .iter()
                .map(|p| p.name.to_ascii_lowercase())
                .collect();
            let prog = match &cs.block {
                Some(b) => compile_routine_with_names(b, &extra, rules, routine_names)?,
                None => IrProg::default(),
            };
            let params = cs
                .parameters
                .iter()
                .map(|p| {
                    Ok(RoutineParam {
                        name: p.name.clone(),
                        mode: map_mode(&p.mode)?,
                    })
                })
                .collect::<Result<Vec<_>, EngineError>>()?;
            out.push(CompiledRoutine {
                name: cs
                    .name
                    .iter()
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join("."),
                kind: RoutineKind::Procedure,
                params,
                prog,
            });
        }
        Statement::CreateFunction(cf) => {
            let extra: Vec<String> = cf
                .parameters
                .iter()
                .map(|p| p.name.to_ascii_lowercase())
                .collect();
            let prog = match &cf.block {
                Some(b) => compile_routine_with_names(b, &extra, rules, routine_names)?,
                None => IrProg::default(),
            };
            let params = cf
                .parameters
                .iter()
                .map(|p| {
                    Ok(RoutineParam {
                        name: p.name.clone(),
                        mode: map_mode(&p.mode)?,
                    })
                })
                .collect::<Result<Vec<_>, EngineError>>()?;
            out.push(CompiledRoutine {
                name: cf
                    .name
                    .iter()
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join("."),
                kind: RoutineKind::Function,
                params,
                prog,
            });
        }
        other => {
            return Err(EngineError::invariant(
                60,
                format!("compile_statement_routine: unsupported statement {other:?}"),
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_classify_in_out_as_inout() {
        // 回归守护：contains("OUT") 曾使 `IN OUT` 被归类为 Out（丢入参值）
        assert_eq!(map_mode_pub(&Some("IN".into())).unwrap(), ParamMode::In);
        assert_eq!(map_mode_pub(&None).unwrap(), ParamMode::In);
        assert_eq!(map_mode_pub(&Some("OUT".into())).unwrap(), ParamMode::Out);
        assert_eq!(
            map_mode_pub(&Some("IN OUT".into())).unwrap(),
            ParamMode::InOut
        );
        assert_eq!(
            map_mode_pub(&Some("INOUT".into())).unwrap(),
            ParamMode::InOut
        );
    }

    #[test]
    fn should_reject_unknown_parameter_mode() {
        let e = map_mode_pub(&Some("READONLY".into())).unwrap_err();
        assert_eq!(e.code.to_string(), "BIZ-0130");
    }
}
