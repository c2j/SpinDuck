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

    /// 纯表达式函数 → 内核宏（§8.2 路径③），使 SQL 上下文可直接调用。
    ///
    /// 只登记"无过程逻辑"的函数：单 `RETURN <表达式>`、无局部变量、纯 IN 参数、
    /// 且函数名非包限定（包函数需 schema 前缀，M7 处理）。其余保持 PL 上下文
    /// 直调；SQL 上下文调用将得到内核 Catalog Error（不静默降级为错误语义）。
    fn install_sql_macro(
        &mut self,
        r: &crate::catalog::CompiledRoutine,
    ) -> Result<(), EngineError> {
        use crate::catalog::{ParamMode, RoutineKind};
        if r.kind != RoutineKind::Function
            || r.name.contains('.')
            || r.params.iter().any(|p| p.mode != ParamMode::In)
        {
            return Ok(());
        }
        let Some(expr) = r.prog.single_return_expr() else {
            return Ok(());
        };
        let Some(body) = crate::sql::expr_to_sql(expr, &self.rules) else {
            return Ok(());
        };
        let params = r
            .params
            .iter()
            .map(|p| p.name.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(", ");
        let ddl = format!(
            "CREATE OR REPLACE MACRO {}({}) AS ({})",
            r.name.to_ascii_lowercase(),
            params,
            body
        );
        self.ex.execute(&ddl, &crate::exec::BindMap::new())?;
        Ok(())
    }

    /// 单条 AST 语句的注册（含 PACKAGE BODY 展开）。
    pub fn register_statement(&mut self, st: &Statement) -> Result<usize, EngineError> {
        match st {
            Statement::CreateProcedure(_) | Statement::CreateFunction(_) => {
                let routines = compile_statement_routine(st, &self.rules)?;
                let n = routines.len();
                for r in routines {
                    self.install_sql_macro(&r)?;
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

#[cfg(all(test, feature = "native"))]
mod kernel_tests {
    use super::*;
    use crate::exec::BindMap;
    use crate::sql::DuckExecutor;

    #[test]
    fn should_expose_expression_function_as_macro_in_sql_context() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        eng.register(
            "CREATE OR REPLACE FUNCTION fnc_double(p IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN p * 2; END;",
        )
        .unwrap();

        // SQL 上下文（DuckDB 内）可直接调用：宏路径③
        let mut s = eng
            .ex
            .query("SELECT fnc_double(21) AS v", &BindMap::new())
            .unwrap();
        let rows = s.fetch_many(1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], SpinValue::Int(42));
    }

    #[test]
    fn should_use_macro_inside_procedure_sql() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        // 表由 load 通道建立（register 只处理例程）
        let empty = BindMap::new();
        eng.ex.execute("CREATE TABLE dat(n INT)", &empty).unwrap();
        eng.ex.execute("CREATE TABLE dat2(n INT)", &empty).unwrap();
        eng.ex
            .execute("INSERT INTO dat VALUES (21)", &empty)
            .unwrap();
        eng.register(
            "CREATE OR REPLACE FUNCTION fnc_double(p IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN p * 2; END;",
        )
        .unwrap();
        // 过程体内的 SQL 语句直接引用该函数（改写后由内核宏求值）
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_ins IS \
             BEGIN INSERT INTO dat2 SELECT fnc_double(n) FROM dat; END;",
        )
        .unwrap();
        eng.call("prc_ins", &[]).unwrap();

        let mut s = eng.ex.query("SELECT n FROM dat2", &BindMap::new()).unwrap();
        let rows = s.fetch_many(10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], SpinValue::Int(42));
    }

    #[test]
    fn should_apply_function_rules_when_rendering_macro_body() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut rules = RuleRegistry::default();
        rules
            .functions
            .insert("NVL".to_string(), "coalesce".to_string());
        let mut eng = Engine::new(Box::new(ex), rules);
        eng.register(
            "CREATE OR REPLACE FUNCTION fnc_nz(p IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN nvl(p, 0); END;",
        )
        .unwrap();

        let mut s = eng
            .ex
            .query("SELECT fnc_nz(NULL) AS v, fnc_nz(7) AS w", &BindMap::new())
            .unwrap();
        let rows = s.fetch_many(1);
        assert_eq!(rows[0][0], SpinValue::Int(0));
        assert_eq!(rows[0][1], SpinValue::Int(7));
    }

    #[test]
    fn should_not_register_macro_for_function_with_local_logic() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        eng.register(
            "CREATE OR REPLACE FUNCTION fnc_local(p IN NUMBER) RETURN NUMBER IS \
             DECLARE v NUMBER; BEGIN v := p + 1; RETURN v; END;",
        )
        .unwrap();

        let mut s = eng
            .ex
            .query(
                "SELECT count(*) FROM duckdb_functions() WHERE function_name = 'fnc_local'",
                &BindMap::new(),
            )
            .unwrap();
        let rows = s.fetch_many(1);
        assert_eq!(rows[0][0], SpinValue::Int(0), "含局部变量不进宏命名空间");

        // PL 上下文仍可用（解释器直调）
        let fr = eng.call("fnc_local", &[SpinValue::Int(1)]).unwrap();
        assert_eq!(fr.return_value, Some(SpinValue::Int(2)));
    }

    #[test]
    fn should_keep_procedure_out_of_sql_macro_namespace() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        eng.register("CREATE OR REPLACE PROCEDURE prc_noop IS BEGIN NULL; END;")
            .unwrap();
        let r = eng.ex.query(
            "SELECT count(*) FROM duckdb_functions() WHERE function_name = 'prc_noop'",
            &BindMap::new(),
        );
        // 过程不应进入内核函数命名空间（宏只登记纯表达式函数）
        let mut r = r.unwrap();
        let rows = r.fetch_many(1);
        assert_eq!(rows[0][0], SpinValue::Int(0));
    }
}
