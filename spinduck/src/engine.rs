//! 引擎对外入口（§12 P5）：register（注册例程）+ call（调用）。
//! 错误契约：Business 按错误码分支；未捕获异常携带 cid 由应用上报。

use crate::catalog::{compile_statement_routine, CallResult, RoutineCatalog};
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use crate::ir::{compile_routine_with, AssignTarget, IrProg, VarName};
use crate::rules::RuleRegistry;
use crate::runtime::{Frame, RunOutcome};
use crate::value::SpinValue;
use ogsql_parser::ast::Statement;
use ogsql_parser::token::tokenizer::Tokenizer;

/// 引擎：执行器 + 规则 + 例程目录。
pub struct Engine {
    pub ex: Box<dyn SqlExecutor>,
    pub rules: RuleRegistry,
    pub catalog: RoutineCatalog,
}

pub struct CallResultExt {
    pub result: CallResult,
}

impl Engine {
    pub fn new(ex: Box<dyn SqlExecutor>, rules: RuleRegistry) -> Self {
        Self {
            ex,
            rules,
            catalog: RoutineCatalog::new(),
        }
    }

    /// 解析 SQL 文本并注册其中的例程（CREATE PROCEDURE/FUNCTION/PACKAGE BODY）。
    pub fn register(&mut self, sql_text: &str) -> Result<usize, EngineError> {
        let tokens = Tokenizer::new(sql_text)
            .tokenize()
            .map_err(|e| EngineError::invariant(63, format!("tokenizer: {e}")))?;
        let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
        let mut n = 0;
        for st in &stmts {
            n += self.register_statement(st)?;
        }
        Ok(n)
    }

    /// 单条 AST 语句的注册（含 PACKAGE BODY 展开）。
    pub fn register_statement(&mut self, st: &Statement) -> Result<usize, EngineError> {
        match st {
            Statement::CreateProcedure(_) | Statement::CreateFunction(_) => {
                let routines = compile_statement_routine(st, &self.rules)?;
                let n = routines.len();
                for r in routines {
                    self.catalog.register(r);
                }
                Ok(n)
            }
            // PACKAGE BODY：items 内的 Procedure/Function 逐个编译，例程名带包前缀
            Statement::CreatePackageBody(pb) => {
                let pkg = pb
                    .name
                    .iter()
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join(".");
                let mut n = 0;
                for item in &pb.items {
                    // items 的变体名与 spec 对称：Procedure/Function
                    if let ogsql_parser::ast::PackageItem::Procedure(p) = item {
                        let full = format!(
                            "{}.{}",
                            pkg.to_ascii_lowercase(),
                            p.name.join(".").to_ascii_lowercase()
                        );
                        let prog = match &p.block {
                            Some(b) => {
                                let extra: Vec<String> = p
                                    .parameters
                                    .iter()
                                    .map(|pp| pp.name.to_ascii_lowercase())
                                    .collect();
                                compile_routine_with(b, &extra, &self.rules)?
                            }
                            None => IrProg::default(),
                        };
                        let params = p
                            .parameters
                            .iter()
                            .map(|pp| crate::catalog::RoutineParam {
                                name: pp.name.clone(),
                                mode: crate::catalog::map_mode_pub(&pp.mode),
                            })
                            .collect();
                        self.catalog.register(crate::catalog::CompiledRoutine {
                            name: full,
                            kind: crate::catalog::RoutineKind::Procedure,
                            params,
                            prog,
                        });
                        n += 1;
                    }
                    if let ogsql_parser::ast::PackageItem::Function(f) = item {
                        let full = format!(
                            "{}.{}",
                            pkg.to_ascii_lowercase(),
                            f.name.join(".").to_ascii_lowercase()
                        );
                        let prog = match &f.block {
                            Some(b) => {
                                let extra: Vec<String> = f
                                    .parameters
                                    .iter()
                                    .map(|pp| pp.name.to_ascii_lowercase())
                                    .collect();
                                compile_routine_with(b, &extra, &self.rules)?
                            }
                            None => IrProg::default(),
                        };
                        let params = f
                            .parameters
                            .iter()
                            .map(|pp| crate::catalog::RoutineParam {
                                name: pp.name.clone(),
                                mode: crate::catalog::map_mode_pub(&pp.mode),
                            })
                            .collect();
                        self.catalog.register(crate::catalog::CompiledRoutine {
                            name: full,
                            kind: crate::catalog::RoutineKind::Function,
                            params,
                            prog,
                        });
                        n += 1;
                    }
                }
                Ok(n)
            }
            _ => Ok(0), // 非例程语句（建表等）由 load 通道处理
        }
    }

    /// 调用例程（§12 P5）：IN 按值、OUT/INOUT 按位取回、函数返回标量。
    pub fn call(&mut self, name: &str, args: &[SpinValue]) -> Result<CallResult, EngineError> {
        let routine = self
            .catalog
            .lookup(name)
            .ok_or_else(|| EngineError::business(150, format!("routine {name} not registered")))?
            .clone();
        if args.len() != routine.params.len() {
            return Err(EngineError::business(
                151,
                format!(
                    "routine {} expects {} args, got {}",
                    name,
                    routine.params.len(),
                    args.len()
                ),
            ));
        }
        let mut frame = Frame::new();
        for (p, v) in routine.params.iter().zip(args) {
            frame.set(&VarName(p.name.to_ascii_lowercase()), v.clone());
        }
        let mut sink = crate::exec::TraceNoopSink;
        let out = crate::runtime::run_with_catalog(
            &routine.prog,
            &mut frame,
            self.ex.as_ref(),
            &mut sink,
            &self.catalog,
            0,
        )?;
        let mut result = CallResult::default();
        for p in &routine.params {
            if p.mode != crate::catalog::ParamMode::In {
                if let Some(v) = frame.get(&VarName(p.name.to_ascii_lowercase())) {
                    result.outs.insert(p.name.clone(), v.clone());
                }
            }
        }
        if let RunOutcome::Returned(v) = out {
            result.return_value = v;
        }
        Ok(result)
    }
}

/// TraceSink 的空实现（引擎默认；CLI/监控注入真实 sink）。
pub struct TraceNoopSink;
impl crate::exec::TraceSink for TraceNoopSink {
    fn on_event(&mut self, _event: crate::exec::TraceEvent) {}
}

/// 表达式求值环境（PL 上下文函数直调需要 catalog/ex）。
pub struct EvalCtx<'a> {
    pub frame: &'a Frame,
    pub ex: &'a dyn SqlExecutor,
    pub cat: &'a RoutineCatalog,
    pub depth: usize,
}

/// AssignTarget 的便捷构造（CallRoutine OUT 写回）。
pub fn var_target(name: &str) -> AssignTarget {
    AssignTarget::Var(VarName(name.to_ascii_lowercase()))
}

/// 供 ir 测试与 engine 使用：SQL 文本 → 参数名 → 帧变量集（M6 扩展点落地）。
pub fn param_names_to_vars(params: &[String]) -> Vec<VarName> {
    params
        .iter()
        .map(|p| VarName(p.to_ascii_lowercase()))
        .collect()
}
