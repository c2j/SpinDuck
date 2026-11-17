//! batch 编排的引擎侧钩子（设计 docs/spinduck-batch-sdsh-design.md §3.2/§5.1）：
//! 运行期开关复位、外部写后的缓存失效、`MACRO` 幂等重装、跨计划 `CompiledRoutine` 缓存。
//!
//! 与 `set_oriented.rs` 同布局：`impl Engine` 拆分文件，可访问 engine 私有字段。
//! 单步 CLI 路径不经过这里——`compiled_cache` 缺省 `None`，行为逐字节不变。

use super::Engine;
use crate::catalog::CompiledRoutine;
use crate::error::EngineError;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// 编译产物缓存键：(源文本哈希, 生效规则指纹, 编译上下文指纹)。
///
/// 上下文指纹覆盖编译期真正读取的两套名字集合：`routine_names()`（PACKAGE BODY
/// 编译用）与 `macro_names`（决定 `downshift_names()` = routine_names - macro_names，
/// PROCEDURE/FUNCTION 编译用）。只哈希 downshift 会漏掉"同 downshift、不同
/// routine_names"的碰撞（宏化与否不同但名字集合相同的两个前缀）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompiledKey {
    source: u64,
    rules: u64,
    context: u64,
}

impl CompiledKey {
    fn of(eng: &Engine, sql_text: &str) -> Self {
        let mut source = DefaultHasher::new();
        sql_text.hash(&mut source);
        let mut rules = DefaultHasher::new();
        format!("{:?}", eng.rules).hash(&mut rules);
        let mut context = DefaultHasher::new();
        eng.catalog.routine_names().hash(&mut context);
        eng.macro_names.hash(&mut context);
        Self {
            source: source.finish(),
            rules: rules.finish(),
            context: context.finish(),
        }
    }
}

/// 跨计划/跨 `--file` 组合复用的编译产物缓存（batch `EngineCache` 未命中但
/// 源集合部分重叠时仍省 IR 编译）。写入只发生在 `register_named_uncached`
/// 成功返回之后——静默丢弃护栏（ENG-0064）拒绝的文本永不进缓存。
#[derive(Debug, Default)]
pub struct CompiledRoutineCache {
    map: HashMap<CompiledKey, Vec<CompiledRoutine>>,
    hits: std::cell::Cell<usize>,
    misses: std::cell::Cell<usize>,
}

impl CompiledRoutineCache {
    /// 测试与观测用：命中/未命中计数（缓存有效性的唯一可断言面）。
    pub fn stats(&self) -> (usize, usize) {
        (self.hits.get(), self.misses.get())
    }
}

impl Engine {
    /// 装配 batch 共享的编译产物缓存（`Rc<RefCell>` 与 memo/lookup 同风格：
    /// 引擎单线程使用，缓存跨引擎实例共享）。
    pub fn attach_compiled_cache(&mut self, cache: Rc<RefCell<CompiledRoutineCache>>) {
        self.compiled_cache = Some(cache);
    }

    /// 带缓存的注册路径：`register_named` 在 `compiled_cache` 装配后走这里。
    /// `pub(super)`：定义在子模块、由父模块 `register_named` 回调。
    /// 命中 = 跳过 tokenize/parse/IR 编译，按序重放 `install_sql_macro` +
    /// `catalog.register`（与 `register_statement_named` 的副作用逐一对齐）。
    pub(super) fn register_cached_named(
        &mut self,
        sql_text: &str,
    ) -> Result<Vec<String>, EngineError> {
        let key = CompiledKey::of(self, sql_text);
        let cached = self
            .compiled_cache
            .as_ref()
            .and_then(|c| c.borrow().map.get(&key).cloned());
        let Some(routines) = cached else {
            if let Some(c) = &self.compiled_cache {
                let b = c.borrow_mut();
                b.misses.set(b.misses.get() + 1);
            }
            let names = self.register_named_uncached(sql_text)?;
            // 只缓存成功注册的结果：routines 从 catalog 按名取回（OR REPLACE
            // 重定义名也取到最新编译产物），names 与 routines 一一对应。
            let routines: Vec<CompiledRoutine> = names
                .iter()
                .filter_map(|n| self.catalog.lookup(n).cloned())
                .collect();
            if let Some(c) = &self.compiled_cache {
                c.borrow_mut().map.insert(key, routines);
            }
            return Ok(names);
        };
        if let Some(c) = &self.compiled_cache {
            let b = c.borrow_mut();
            b.hits.set(b.hits.get() + 1);
        }
        let mut names = Vec::with_capacity(routines.len());
        for r in &routines {
            if self.install_sql_macro(r)? {
                self.macro_names.insert(r.name.to_ascii_lowercase());
            }
            names.push(r.name.clone());
            self.catalog.register(r.clone());
        }
        Ok(names)
    }

    /// batch 步边界复位：清 `set_oriented_reports`（memo/lookup 由
    /// `call_with_sink` 入口清空；本方法兜底"未发起 call 就复用引擎"的路径）。
    pub fn reset_for_batch_step(&mut self) {
        self.set_oriented_reports.clear();
    }

    /// 外部写（deploy/materialize/query，含 `--continue-on-error` 下的失败写）
    /// 后的缓存失效：memo + lookup + set_oriented 报告全部清空。
    /// 引擎 catalog/宏不动——例程定义不受数据写影响；内核对象新鲜度由
    /// batch 层在下次 `call` 前重查 `kernel_objects` 保证。
    pub fn invalidate_caches(&mut self) {
        self.memo.clear();
        self.lookup.clear();
        self.set_oriented_reports.clear();
    }

    /// 本引擎安装过的宏名（小写；`MACRO` 是 DB 编目对象，跨连接可见）。
    pub fn macro_names(&self) -> &BTreeSet<String> {
        &self.macro_names
    }

    /// 幂等重装本闭包全部 `MACRO`（EngineCache 命中兜底：宏是数据库编目对象，
    /// 可能被批外连接 DROP 或未随引擎走；`CREATE OR REPLACE` 重复执行无副作用）。
    /// 只重放 `macro_names` 命中的例程——catalog 里未宏化的例程不受影响。
    pub fn refresh_sql_macros(&mut self) -> Result<(), EngineError> {
        let routines: Vec<CompiledRoutine> = self
            .macro_names
            .iter()
            .filter_map(|n| self.catalog.lookup(n).cloned())
            .collect();
        for r in &routines {
            self.install_sql_macro(r)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::BindMap;

    fn engine_with_cache() -> (Engine, Rc<RefCell<CompiledRoutineCache>>) {
        let cache = Rc::new(RefCell::new(CompiledRoutineCache::default()));
        let mut eng = Engine::new(
            Box::new(crate::cli::NoopExecutor),
            crate::rules::RuleRegistry::default(),
        );
        eng.attach_compiled_cache(cache.clone());
        (eng, cache)
    }

    const FN_SQL: &str = "CREATE OR REPLACE FUNCTION fnc_double(x IN NUMBER) \
                          RETURN NUMBER IS BEGIN RETURN x * 2; END;";

    #[test]
    fn should_reuse_compiled_routine_across_engines_with_shared_cache() {
        let (mut e1, cache) = engine_with_cache();
        assert_eq!(e1.register(FN_SQL).unwrap(), 1);
        assert_eq!(cache.borrow().stats(), (0, 1));
        // 第二个引擎同规则同源：命中缓存，catalog 仍得例程且可调用
        let mut e2 = Engine::new(
            Box::new(crate::cli::NoopExecutor),
            crate::rules::RuleRegistry::default(),
        );
        e2.attach_compiled_cache(cache.clone());
        assert_eq!(e2.register(FN_SQL).unwrap(), 1);
        assert_eq!(cache.borrow().stats(), (1, 1));
        assert!(e2.catalog.lookup("fnc_double").is_some());
        assert!(e2.macro_names().contains("fnc_double"));
    }

    #[test]
    fn should_not_cache_failed_registration() {
        let (mut eng, cache) = engine_with_cache();
        // 注册失败（词法/编译错）的文本不得进缓存：同一坏文本重试仍是 miss，
        // 之后好文本正常 miss→存→hit。
        let bad = "CREATE OR REPLACE FUNCTION fnc_x(";
        assert!(eng.register(bad).is_err());
        assert!(eng.register(bad).is_err());
        assert_eq!(cache.borrow().stats(), (0, 2), "两次失败各计一次 miss");
        assert_eq!(eng.register(FN_SQL).unwrap(), 1);
        // 同引擎重复注册同一源：catalog 上下文已变（fnc_double 入册），键不同仍 miss；
        // 跨引擎同前缀才命中——这正是"按编译上下文键控"的语义。
        let mut e2 = Engine::new(
            Box::new(crate::cli::NoopExecutor),
            crate::rules::RuleRegistry::default(),
        );
        e2.attach_compiled_cache(cache.clone());
        assert_eq!(e2.register(FN_SQL).unwrap(), 1);
        assert_eq!(cache.borrow().stats(), (1, 3));
    }

    #[test]
    fn should_invalidate_caches_and_reset_reports_for_batch_step() {
        let (mut eng, _cache) = engine_with_cache();
        eng.batch_commit = true;
        eng.step_budget = 7;
        eng.reset_for_batch_step();
        eng.invalidate_caches();
        // 复位只清报告与会话缓存；运行期开关由 call_engine 每步重设，不在此复位
        assert!(eng.set_oriented_reports().is_empty());
    }

    #[cfg(feature = "native")]
    #[test]
    fn should_reinstall_dropped_macro_on_refresh() {
        let ex = crate::sql::DuckExecutor::open_in_memory().unwrap();
        let mut eng = Engine::new(Box::new(ex), crate::rules::RuleRegistry::default());
        eng.register(FN_SQL).unwrap();
        assert!(eng.macro_names().contains("fnc_double"));
        // 外部删除宏（模拟批外连接/跨进程干预）
        eng.ex
            .execute("DROP MACRO fnc_double", &BindMap::new())
            .unwrap();
        eng.refresh_sql_macros().unwrap();
        // 重装后 SQL 上下文可再调用宏
        let mut s = eng
            .ex
            .query("SELECT fnc_double(21)", &BindMap::new())
            .unwrap();
        assert_eq!(s.fetch_many(1)[0][0].as_str_loose(), "42");
    }
}
