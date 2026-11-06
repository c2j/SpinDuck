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
    /// 声明的 `DEFAULT <字面量>`（登记期编译；无默认值 = 必传）
    pub default: Option<SpinValue>,
}

/// 形参声明 → [`RoutineParam`]（模式 + DEFAULT 字面量；包体与独立例程同口径）。
pub fn map_param(p: &ogsql_parser::ast::RoutineParam) -> Result<RoutineParam, EngineError> {
    Ok(RoutineParam {
        name: p.name.clone(),
        mode: map_mode(&p.mode)?,
        default: p
            .default_value
            .as_deref()
            .map(default_literal)
            .transpose()?,
    })
}

/// 形参 `DEFAULT` 的字面量口径（语料实测只有 NULL / 数值 / 引号串）：
/// 与 PL 表达式字面量同一映射（整数→Int、小数→Decimal）；非字面量默认值
/// （表达式、函数调用、引用先前形参）显式拒绝，不静默按 NULL 处理。
fn default_literal(raw: &str) -> Result<SpinValue, EngineError> {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("null") {
        return Ok(SpinValue::Null);
    }
    if t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false") {
        return Ok(SpinValue::Bool(t[..4].eq_ignore_ascii_case("true")));
    }
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return Ok(SpinValue::Str(t[1..t.len() - 1].replace("''", "'")));
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(SpinValue::Int(i));
    }
    if let Some(d) = crate::ir::parse_decimal_literal(t) {
        return Ok(d);
    }
    Err(EngineError::business(
        203,
        format!("DEFAULT {raw} (非字面量默认值) is not supported in v1"),
    ))
}

/// 位置实参的元数校验：调用方省略的必须是**尾部且声明了 DEFAULT** 的形参。
pub fn check_arity(name: &str, params: &[RoutineParam], got: usize) -> Result<(), EngineError> {
    if got > params.len() {
        return Err(arity_err(name, params.len(), got));
    }
    match params[got..].iter().find(|p| p.default.is_none()) {
        Some(p) => Err(EngineError::business(
            151,
            format!("{name} 缺少实参 {}（形参未声明 DEFAULT）", p.name),
        )),
        None => Ok(()),
    }
}

fn arity_err(name: &str, len: usize, got: usize) -> EngineError {
    EngineError::business(151, format!("{name} expects {len} args, got {got}"))
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
    /// SQL 方言规则（函数表 + patterns 开关）：动态 SQL 运行期改写与静态语句
    /// 同口径，函数表一律由本注册表派生（单一事实源，不得并存第二份表）
    sql_rules: crate::rules::RuleRegistry,
}

impl RoutineCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入完整规则注册表（patterns 开关；动态路径同口径）。
    pub fn set_sql_rules(&mut self, rules: crate::rules::RuleRegistry) {
        self.sql_rules = rules;
    }

    /// 完整规则注册表（动态路径 patterns/函数同口径）。
    pub fn sql_rules(&self) -> &crate::rules::RuleRegistry {
        &self.sql_rules
    }

    /// SQL 方言规则函数表（动态 SQL 改写用）：`sql_rules.functions` 的视图。
    pub fn sql_functions(&self) -> &BTreeMap<String, String> {
        &self.sql_rules.functions
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
                .map(map_param)
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
                .map(map_param)
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

    #[test]
    fn should_derive_sql_functions_from_the_injected_rules() {
        // 单一事实源：函数表必须是规则注册表的一部分，不得两处各自维护
        // （回归守护：曾并存 sql_functions 字段与 sql_rules.functions，可被注入成漂移状态）
        let mut rules = crate::rules::RuleRegistry::default();
        rules
            .functions
            .insert("MY_FN".to_string(), "my_sql_fn".to_string());
        let mut cat = RoutineCatalog::new();
        cat.set_sql_rules(rules);
        assert_eq!(cat.sql_functions()["NVL"], "coalesce");
        assert_eq!(cat.sql_functions(), &cat.sql_rules().functions);
    }

    #[test]
    fn should_keep_default_catalog_functions_consistent_with_default_rules() {
        let cat = RoutineCatalog::new();
        assert_eq!(
            cat.sql_functions(),
            &cat.sql_rules().functions,
            "默认目录的函数表与规则表同源"
        );
    }
}
