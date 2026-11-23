//! 引擎对外入口（§12 P5）：register（注册例程）+ call（调用）。
//! 错误契约：Business 按错误码分支；未捕获异常携带 cid 由应用上报。

use crate::catalog::{compile_statement_routine_with_names, CallResult, RoutineCatalog};
use crate::error::EngineError;
use crate::exec::SqlExecutor;
use crate::ir::VarName;
use crate::rules::RuleRegistry;
use crate::runtime::{Frame, RunOutcome};
use crate::value::SpinValue;
use ogsql_parser::ast::Statement;
use ogsql_parser::token::tokenizer::Tokenizer;

mod batch_hooks;
mod loop_parallel;
mod set_oriented;
mod support;

pub use crate::exec::TraceNoopSink;
pub(crate) use support::{declares_routine, format_statements, has_package_spec};
pub use support::{param_names_to_vars, var_target, EvalCtx};

/// batch 编译产物缓存类型（`engine::batch_hooks` 的公开出口——batch 层
/// 需要具名类型来装配 `Rc<RefCell<CompiledRoutineCache>>`）。
pub use batch_hooks::CompiledRoutineCache;

/// 引擎：执行器 + 规则 + 例程目录。
pub struct Engine {
    pub ex: Box<dyn SqlExecutor>,
    pub rules: RuleRegistry,
    pub catalog: RoutineCatalog,
    /// 已装为 DuckDB 宏的表达式函数（这些名字不再参与 SQL 上下文下沉）
    macro_names: std::collections::BTreeSet<String>,
    /// 顶层调用的解释器步数预算（ENG-0032 护栏；嵌套例程仍各自取默认值）
    pub step_budget: u64,
    /// 合并 COMMIT 开关（默认关 = §3.5「COMMIT 即时生效」不变）：ON 时过程内
    /// INSERT 攒批合并、COMMIT 吞并，出口统一刷盘由调用 COMMIT 持久化。
    /// 偏差点：失败行跳过发生在刷盘期，不进入 PL 异常处理器（见 writebuf.rs）。
    pub batch_commit: bool,
    /// 默认关闭的受限静态集合化，不与 batch_commit 叠加。
    pub set_oriented: crate::ir::set_oriented::SetOrientedMode,
    set_oriented_reports: Vec<crate::ir::set_oriented::Report>,
    /// 集合化退回后的二级降级并行通道（默认全关；设计 loop-parallel §2）。
    pub loop_parallel: crate::ir::loop_parallel::LoopParallelConfig,
    loop_parallel_reports: Vec<crate::ir::loop_parallel::LoopParallelReport>,
    /// 纯标量语句记忆化缓存（`ex` 内的 MemoizingExecutor 与本句柄共享），
    /// 每次顶层调用入口清空，给缓存一个确定的内存上界。
    memo: std::rc::Rc<crate::exec::MemoCache>,
    /// 参数表点查缓存。与 memo 一样在顶层调用入口清空。
    lookup: std::rc::Rc<crate::lookup_cache::LookupState>,
    /// 与 `ex` 链中 LookupExecutor 共享 inner/state 的句柄：
    /// loop-parallel 预热（`try_prewarm_only`）与快照导出
    /// （`export_frozen`）必须走同一条底层连接。
    lookup_exec: crate::lookup_cache::LookupExecutor,
    /// IR 编译缓存（设计 V4 §5.4 二期）：`None` = 不缓存（默认）。
    /// 命中跳过 tokenize/parse/编译全链路；键含注册上下文指纹，
    /// 保证例程互调下沉与宏消歧不串条目。
    pub ir_cache: Option<crate::ir_cache::IrCache>,
    /// batch 编译产物缓存（`engine::batch_hooks`）：`None` = 逐次编译（单步 CLI 现状）。
    /// 命中时跳过 tokenize/parse/IR 编译，直接重放 `CompiledRoutine`。
    compiled_cache: Option<std::rc::Rc<std::cell::RefCell<batch_hooks::CompiledRoutineCache>>>,
    /// 编译期 codeweb 结构快照（零 DuckDB 生成）；调用期做规范化指纹漂移复核。
    schema_snapshot: Option<crate::ir::set_oriented::metadata::SchemaSnapshot>,
    set_oriented_plans: Option<crate::ir::set_oriented::CompiledPlans>,
}

pub struct CallResultExt {
    pub result: CallResult,
}

impl Engine {
    pub fn new(ex: Box<dyn SqlExecutor>, rules: RuleRegistry) -> Self {
        let mut catalog = RoutineCatalog::new();
        catalog.set_sql_rules(rules.clone());
        let memo = std::rc::Rc::new(crate::exec::MemoCache::default());
        let lookup = std::rc::Rc::new(crate::lookup_cache::LookupState::default());
        let inner: std::rc::Rc<dyn SqlExecutor> =
            std::rc::Rc::new(crate::exec::MemoizingExecutor::with_cache(ex, memo.clone()));
        // 与 ex 链中 LookupExecutor 共享同一 inner/state 的克隆句柄：
        // loop-parallel 预热/冻结必须落在同一条底层连接与缓存状态上。
        let lookup_exec = crate::lookup_cache::LookupExecutor::new(inner, lookup.clone());
        let ex = Box::new(lookup_exec.clone());
        Self {
            ex,
            rules,
            catalog,
            macro_names: std::collections::BTreeSet::new(),
            step_budget: crate::runtime::interp::DEFAULT_STEP_BUDGET,
            batch_commit: false,
            set_oriented: crate::ir::set_oriented::SetOrientedMode::Off,
            set_oriented_reports: Vec::new(),
            loop_parallel: crate::ir::loop_parallel::LoopParallelConfig::default(),
            loop_parallel_reports: Vec::new(),
            memo,
            lookup,
            lookup_exec,
            ir_cache: None,
            compiled_cache: None,
            schema_snapshot: None,
            set_oriented_plans: None,
        }
    }

    /// 接收静态目录并按源顺序安装宏，不重新编译例程。
    pub fn from_compiled(
        ex: Box<dyn SqlExecutor>,
        compiled: crate::catalog::CompiledCatalog,
    ) -> Result<Self, EngineError> {
        let mut engine = Self::new(ex, compiled.catalog.sql_rules().clone());
        for sql in &compiled.macro_sql {
            engine.ex.execute(sql, &crate::exec::BindMap::new())?;
        }
        engine.catalog = compiled.catalog;
        engine.macro_names = compiled.macro_names;
        Ok(engine)
    }

    /// 挂载编译期结构快照；集合化入口据此复核运行期结构漂移（设计 §4.1）。
    pub fn attach_schema_snapshot(
        &mut self,
        snapshot: crate::ir::set_oriented::metadata::SchemaSnapshot,
    ) {
        self.schema_snapshot = Some(snapshot);
        self.set_oriented_plans = None;
    }

    /// 解析 SQL 文本并注册其中的例程（CREATE PROCEDURE/FUNCTION/PACKAGE BODY）。
    pub fn register(&mut self, sql_text: &str) -> Result<usize, EngineError> {
        Ok(self.register_named(sql_text)?.len())
    }

    /// 注册并返回**本次定义/替换**的例程名（目录持久化按名单写行，
    /// 不得遍历全目录——回归守护见 should_persist_each_routine_with_its_own_source）。
    /// 注册上下文指纹：例程名 + 宏名集合（排序拼接）。fn_sites 下沉
    /// 与宏消歧依赖目录内容，必须进缓存键。
    fn cache_context(&self) -> String {
        let mut parts: Vec<String> = self.catalog.routine_names().into_iter().collect();
        parts.extend(self.macro_names.iter().cloned());
        parts.sort();
        parts.join("\u{1}")
    }

    fn register_named(&mut self, sql_text: &str) -> Result<Vec<String>, EngineError> {
        if self.compiled_cache.is_some() {
            return self.register_cached_named(sql_text);
        }
        self.register_named_uncached(sql_text)
    }

    /// 无缓存的注册路径（`register_cached_named` 未命中时委托到这里）。
    fn register_named_uncached(&mut self, sql_text: &str) -> Result<Vec<String>, EngineError> {
        // IR 缓存命中：跳过解析/编译，按登记语义回放（宏安装 + 目录注册）
        let cache_key = self
            .ir_cache
            .as_ref()
            .map(|_| crate::ir_cache::IrCache::key_for(&self.cache_context(), sql_text));
        let hit = cache_key
            .as_ref()
            .and_then(|k| self.ir_cache.as_ref().and_then(|c| c.get(k)));
        if let Some(routines) = hit {
            let mut names = Vec::with_capacity(routines.len());
            for r in routines {
                if self.install_sql_macro(&r)? {
                    self.macro_names.insert(r.name.to_ascii_lowercase());
                }
                names.push(r.name.clone());
                self.catalog.register(r);
            }
            return Ok(names);
        }
        let tokens = Tokenizer::new(sql_text)
            .tokenize()
            .map_err(|e| EngineError::invariant(63, format!("tokenizer: {e}")))?;
        let stmts = ogsql_parser::parser::Parser::new(tokens).parse();
        // Gate 2A 顶层 DDL 拦截（设计 V4 §4.4）：方言/内核不支持的 DDL 在
        // 例程登记前显式拒绝，避免随非例程语句被静默放过。
        for st in &stmts {
            crate::gate::reject_unsupported_ddl(st)?;
        }
        let mut names = Vec::new();
        let mut routines_sink: Vec<crate::catalog::CompiledRoutine> = Vec::new();
        for st in &stmts {
            names.extend(self.register_statement_named(st, &mut routines_sink)?);
        }
        // 静默丢弃护栏：文本声明了例程，解析后一条都没登记（ogsql 对个别 PL
        // 构造把整条 CREATE 降级为 Empty/其它形态）。归域 ENGINE：输入被解析器
        // 吞掉是引擎侧缺口，不得伪装成"没有例程"（守护：guard_register_silent_drop）
        if names.is_empty() && declares_routine(sql_text) && !has_package_spec(&stmts) {
            let head: String = sql_text.trim().chars().take(120).collect();
            return Err(EngineError::invariant(
                64,
                format!(
                    "声明了例程却未登记任何例程（解析器降级/静默丢弃，常见原因：体内构造不支持）：{head}"
                ),
            ));
        }
        if let (Some(cache), Some(key)) = (self.ir_cache.as_mut(), cache_key) {
            cache.put(key, routines_sink);
        }
        Ok(names)
    }

    /// 纯表达式函数 → 内核宏（§8.2 路径③），使 SQL 上下文可直接调用。
    ///
    /// 只登记"无过程逻辑"的函数：单 `RETURN <表达式>`、无局部变量、纯 IN 参数、
    /// 且函数名非包限定（包函数需 schema 前缀，M7 处理）。其余保持 PL 上下文
    /// 直调；SQL 上下文调用将得到内核 Catalog Error（不静默降级为错误语义）。
    fn install_sql_macro(
        &mut self,
        r: &crate::catalog::CompiledRoutine,
    ) -> Result<bool, EngineError> {
        let Some(ddl) =
            crate::catalog::sql_macro::render(r, &self.rules, &self.catalog, &self.macro_names)
        else {
            return Ok(false);
        };
        self.ex.execute(&ddl, &crate::exec::BindMap::new())?;
        Ok(true)
    }

    /// SQL 上下文下沉可用的例程名：已注册且**未**宏化。
    fn downshift_names(&self) -> std::collections::BTreeSet<String> {
        self.catalog
            .routine_names()
            .difference(&self.macro_names)
            .cloned()
            .collect()
    }

    /// 单条 AST 语句的注册（含 PACKAGE BODY 展开）。
    /// `sink` 收集本语句产出的例程（IR 缓存写穿用；登记语义不变）。
    pub fn register_statement(&mut self, st: &Statement) -> Result<usize, EngineError> {
        let mut sink = Vec::new();
        Ok(self.register_statement_named(st, &mut sink)?.len())
    }

    fn register_statement_named(
        &mut self,
        st: &Statement,
        sink: &mut Vec<crate::catalog::CompiledRoutine>,
    ) -> Result<Vec<String>, EngineError> {
        match st {
            Statement::CreateProcedure(_) | Statement::CreateFunction(_) => {
                let routines =
                    compile_statement_routine_with_names(st, &self.rules, &self.downshift_names())?;
                let mut names = Vec::with_capacity(routines.len());
                for r in routines {
                    if self.install_sql_macro(&r)? {
                        self.macro_names.insert(r.name.to_ascii_lowercase());
                    }
                    names.push(r.name.clone());
                    sink.push(r.clone());
                    self.catalog.register(r);
                }
                Ok(names)
            }
            // PACKAGE BODY：items 内的 Procedure/Function 逐个编译，例程名带包前缀
            Statement::CreatePackageBody(pb) => {
                let pkg = pb
                    .name
                    .iter()
                    .map(|i| i.value.clone())
                    .collect::<Vec<_>>()
                    .join(".");
                let mut names: Vec<String> = Vec::new();
                for item in &pb.items {
                    if let Some(routine) = crate::catalog::source::compile_package_item(
                        &pkg,
                        item,
                        &self.rules,
                        &self.catalog.routine_names(),
                    )? {
                        names.push(routine.name.clone());
                        sink.push(routine.clone());
                        self.catalog.register(routine);
                    }
                }
                Ok(names)
            }
            _ => Ok(Vec::new()), // 非例程语句（建表等）由 load 通道处理
        }
    }

    /// 例程目录表（内核内持久化，§3.6 / §12 P6）。
    ///
    /// 只存**可重放的源 DDL 文本**：IR 是编译产物（含跳转地址），跨版本不可信；
    /// 源文本 + 当前规则重新编译才是可靠恢复路径。
    const CATALOG_DDL: &'static str = "CREATE TABLE IF NOT EXISTS spinduck_routines (\
         name VARCHAR PRIMARY KEY, kind VARCHAR NOT NULL, source VARCHAR NOT NULL, \
         registered_at TIMESTAMP DEFAULT current_timestamp)";

    /// 注册例程并把源 DDL 落到内核目录表（重启后 [`reload_catalog`](Self::reload_catalog) 恢复）。
    pub fn register_persistent(&mut self, sql_text: &str) -> Result<usize, EngineError> {
        let names = self.register_named(sql_text)?;
        if names.is_empty() {
            return Ok(0); // 非例程语句（建表等）不写目录
        }
        self.ensure_catalog_table()?;
        let source = format_statements(sql_text)?.join("\n");
        let n = names.len();
        for name in names {
            let kind = match self.catalog.lookup(&name).map(|r| r.kind) {
                Some(crate::catalog::RoutineKind::Procedure) => "PROCEDURE",
                Some(crate::catalog::RoutineKind::Function) => "FUNCTION",
                None => continue,
            };
            let mut binds = crate::exec::BindMap::new();
            binds.insert("$name".to_string(), SpinValue::Str(name));
            binds.insert("$kind".to_string(), SpinValue::Str(kind.to_string()));
            binds.insert("$src".to_string(), SpinValue::Str(source.clone()));
            self.ex.execute(
                "INSERT INTO spinduck_routines(name, kind, source) VALUES ($name, $kind, $src) \
                 ON CONFLICT (name) DO UPDATE SET kind = excluded.kind, source = excluded.source",
                &binds,
            )?;
        }
        Ok(n)
    }

    /// 从内核目录表恢复全部例程（源文本重新编译；返回恢复的例程数）。
    pub fn reload_catalog(&mut self) -> Result<usize, EngineError> {
        let has_table = self
            .ex
            .query(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_name = 'spinduck_routines'",
                &crate::exec::BindMap::new(),
            )
            .map(|mut s| {
                s.fetch_many(1)
                    .first()
                    .and_then(|r| r.first().cloned())
                    .map(|v| v.as_str_loose() == "1")
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !has_table {
            return Ok(0);
        }
        let mut s = self.ex.query(
            "SELECT DISTINCT source FROM spinduck_routines ORDER BY source",
            &crate::exec::BindMap::new(),
        )?;
        let mut total = 0;
        loop {
            let rows = s.fetch_many(64);
            if rows.is_empty() {
                break;
            }
            for row in rows {
                if let Some(src) = row.first().map(|v| v.as_str_loose()) {
                    total += self.register(&src)?;
                }
            }
        }
        Ok(total)
    }

    fn ensure_catalog_table(&mut self) -> Result<(), EngineError> {
        self.ex
            .execute(Self::CATALOG_DDL, &crate::exec::BindMap::new())?;
        Ok(())
    }

    /// 调用例程（§12 P5）：IN 按值、OUT/INOUT 按位取回、函数返回标量。
    /// 默认静默（`TraceNoopSink`）；要观测埋点走 [`Self::call_with_sink`]。
    pub fn call(&mut self, name: &str, args: &[SpinValue]) -> Result<CallResult, EngineError> {
        let mut sink = crate::exec::TraceNoopSink;
        self.call_with_sink(name, args, &mut sink)
    }

    /// 调用例程并把埋点交给调用方的 sink（AGENTS §1 接缝：CLI 持汇，内核只发事件）。
    pub fn call_with_sink(
        &mut self,
        name: &str,
        args: &[SpinValue],
        sink: &mut dyn crate::exec::TraceSink,
    ) -> Result<CallResult, EngineError> {
        self.memo.clear();
        self.lookup.clear();
        self.set_oriented_reports.clear();
        self.loop_parallel_reports.clear();
        let routine = self
            .catalog
            .lookup(name)
            .ok_or_else(|| EngineError::business(150, format!("routine {name} not registered")))?
            .clone();
        crate::catalog::check_arity(name, &routine.params, args.len())?;
        let mut frame = Frame::new();
        frame.install_params(
            &routine.params,
            args,
            self.catalog.sql_rules().implicit_assignment_loose,
        )?;
        // 统一事务边界（§3.5）：最外层调用在显式事务内执行——过程内
        // TxCommit/TxRollback 即时提交/回滚当前事务并重开；正常完成统一
        // COMMIT；未处理异常统一 ROLLBACK（DuckDB ACID 提供原子性）
        self.ex
            .execute("BEGIN TRANSACTION", &crate::exec::BindMap::new())?;
        let out = match self.run_selected(&routine, &mut frame, sink) {
            Ok(out) => out,
            Err(mut e) => {
                // 回滚失败不得静默：并入原错误消息。归域在产生点已定（§5），
                // 主因保持原域与原错误码，回滚失败只作为次因文本追加。
                if let Err(rb) = self.ex.execute("ROLLBACK", &crate::exec::BindMap::new()) {
                    e.message = format!("{}（且 ROLLBACK 失败: {}）", e.message, rb.message);
                }
                return Err(e);
            }
        };
        self.ex.execute("COMMIT", &crate::exec::BindMap::new())?;
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
    fn should_run_handler_after_kernel_error_in_same_session() {
        // 回归守护（语料 156 处 WHEN OTHERS 记日志形态）：过程被包在最外层显式
        // 事务内，语句在内核侧失败会使整个事务作废；处理器体若不先复位会话，
        // 其中的 INSERT/SELECT 全部连锁报 "Current transaction is aborted"。
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        eng.ex
            .execute("CREATE TABLE t_log(m VARCHAR)", &BindMap::new())
            .unwrap();
        eng.register(
            "CREATE OR REPLACE PROCEDURE p_log(o OUT VARCHAR2) IS \
             v DECIMAL(18,2); \
             BEGIN \
               v := TO_NUMBER('abc'); \
               o := 'no'; \
             EXCEPTION WHEN OTHERS THEN \
               INSERT INTO t_log VALUES ('logged'); \
               o := 'caught'; \
             END;",
        )
        .unwrap();
        let r = eng
            .call("p_log", &[SpinValue::Null])
            .unwrap_or_else(|e| panic!("调用失败: {}", e.message));
        assert_eq!(r.outs["o"].as_str_loose(), "caught");
        let mut s = eng
            .ex
            .query("SELECT COUNT(*) FROM t_log", &BindMap::new())
            .unwrap();
        assert_eq!(s.fetch_many(1)[0][0].as_str_loose(), "1");
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

#[cfg(all(test, feature = "native"))]
mod persist_tests {
    use super::*;
    use crate::exec::BindMap;
    use crate::sql::DuckExecutor;

    const DDL: &str = "CREATE OR REPLACE PROCEDURE prc_persist(p_i IN NUMBER, p_o OUT NUMBER) IS \
                       BEGIN p_o := p_i * 5; END;";

    #[test]
    fn should_persist_routines_and_reload_in_new_engine() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("w.duckdb");
        let path = db.to_string_lossy().into_owned();

        {
            let ex = DuckExecutor::open(&path).unwrap();
            let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
            let n = eng.register_persistent(DDL).unwrap();
            assert_eq!(n, 1);
            assert_eq!(eng.catalog.len(), 1);
        } // 连接关闭，释放文件锁

        let ex = DuckExecutor::open(&path).unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        assert!(eng.catalog.is_empty(), "新实例目录应为空");
        let n = eng.reload_catalog().unwrap();
        assert_eq!(n, 1, "从内核表恢复 1 个例程");
        let r = eng
            .call("prc_persist", &[SpinValue::Int(3), SpinValue::Null])
            .unwrap();
        assert_eq!(r.outs.get("p_o"), Some(&SpinValue::Int(15)));
    }

    #[test]
    fn should_upsert_routine_source_without_duplicating_rows() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        eng.register_persistent(DDL).unwrap();
        eng.register_persistent(DDL).unwrap();
        let mut s = eng
            .ex
            .query("SELECT count(*) FROM spinduck_routines", &BindMap::new())
            .unwrap();
        assert_eq!(s.fetch_many(1)[0][0], SpinValue::Int(1));
    }

    #[test]
    fn should_persist_each_routine_with_its_own_source() {
        // 回归守护：register_persistent 曾用本次 SQL 文本覆盖全部已存例程的 source，
        // reload 后两个过程行为相同（持久化能力失效）
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("w.duckdb");
        let path = db.to_string_lossy().into_owned();
        {
            let ex = DuckExecutor::open(&path).unwrap();
            let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
            eng.register_persistent(
                "CREATE OR REPLACE PROCEDURE prc_a(p_o OUT NUMBER) IS BEGIN p_o := 1; END;",
            )
            .unwrap();
            eng.register_persistent(
                "CREATE OR REPLACE PROCEDURE prc_b(p_o OUT NUMBER) IS BEGIN p_o := 2; END;",
            )
            .unwrap();
        }
        let ex = DuckExecutor::open(&path).unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        assert_eq!(eng.reload_catalog().unwrap(), 2);
        assert_eq!(
            eng.call("prc_a", &[SpinValue::Null])
                .unwrap()
                .outs
                .get("p_o"),
            Some(&SpinValue::Int(1))
        );
        assert_eq!(
            eng.call("prc_b", &[SpinValue::Null])
                .unwrap()
                .outs
                .get("p_o"),
            Some(&SpinValue::Int(2))
        );
    }

    #[test]
    fn should_treat_in_out_params_as_in_and_out() {
        // IN OUT：入参值参与运算且写回调用方（归类曾被 contains("OUT") 破坏）
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        eng.register(
            "CREATE OR REPLACE PROCEDURE prc_io(p_x IN OUT NUMBER) IS BEGIN p_x := p_x + 1; END;",
        )
        .unwrap();
        assert_eq!(
            eng.catalog.lookup("prc_io").unwrap().params[0].mode,
            crate::catalog::ParamMode::InOut
        );
        let r = eng.call("prc_io", &[SpinValue::Int(41)]).unwrap();
        assert_eq!(r.outs.get("p_x"), Some(&SpinValue::Int(42)));
    }

    #[test]
    fn should_report_zero_reload_when_catalog_table_absent() {
        let ex = DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), RuleRegistry::default());
        assert_eq!(eng.reload_catalog().unwrap(), 0);
    }
}
