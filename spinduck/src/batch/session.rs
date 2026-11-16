//! batch 会话状态：`DbKey` 规范化、canonical 连接池、`EngineCache`、
//! 共享 `CompiledRoutineCache`（设计 §3.1/§3.2）。
//!
//! 连接纪律：同一 `DbKey` 只允许**一条** canonical `DuckExecutor`（池内），
//! 引擎各自持 `try_clone` 克隆连接——同库绝不二次 `Connection::open`
//! （文件锁）；`:memory:` 显式选择共享同一 instance，省略 `--db` 的匿名
//! 内存库不进池（每步独立 `open_in_memory`，语义与单步 CLI 一致）。

use crate::engine::{CompiledRoutineCache, Engine};
use crate::error::EngineError;
use crate::rules::RuleRegistry;
use crate::sql::DuckExecutor;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// 库标识：文件库按 `(canonical父目录, 文件名)`（文件可能尚不存在，
/// 故只 canonicalize 父目录）；`:memory:` 显式选择 = 共享内存 instance。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DbKey {
    Memory,
    File {
        dir: PathBuf,
        name: std::ffi::OsString,
    },
}

impl DbKey {
    /// `--db` 值 → DbKey。`":memory:"` → `Memory`；其余按父目录 canonicalize
    /// + 文件名（父目录不存在时退回原样——open 会报更准确的错）。
    pub fn of(db: &Path) -> Self {
        if db.as_os_str() == ":memory:" {
            return DbKey::Memory;
        }
        let name = db
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| db.as_os_str().to_os_string());
        let parent = db.parent().filter(|p| !p.as_os_str().is_empty());
        let dir = match parent {
            Some(p) => p.canonicalize().unwrap_or_else(|_| p.to_path_buf()),
            None => PathBuf::from("."),
        };
        let dir = dir.canonicalize().unwrap_or(dir);
        DbKey::File { dir, name }
    }

    /// 打开 canonical 连接（池未命中时调用一次）。
    fn open(&self) -> Result<DuckExecutor, EngineError> {
        match self {
            DbKey::Memory => DuckExecutor::open_in_memory(),
            DbKey::File { dir, name } => DuckExecutor::open(&dir.join(name).to_string_lossy()),
        }
    }
}

/// `EngineCache` 键：`(DbKey, 生效规则指纹, 排序后的源内容哈希集)`。
/// 运行期开关（`batch_commit`/`set_oriented`/`step_budget`）不在键内——
/// 每次 `call` 前由 `call_engine` 重设（设计 §3.2.1-b）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EngineKey {
    db: DbKey,
    rules: u64,
    sources: Vec<u64>,
}

impl EngineKey {
    /// `sources` 为源内容哈希集合（内部排序去重——集合语义，与注册顺序无关）。
    pub fn new(db: DbKey, rules: u64, sources: Vec<u64>) -> Self {
        let mut sources = sources;
        sources.sort_unstable();
        sources.dedup();
        Self { db, rules, sources }
    }
}

fn hash_of<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// 源文件内容哈希（非路径——同路径改内容必须击穿缓存）。
pub fn source_hash(path: &Path) -> Result<u64, EngineError> {
    let bytes = std::fs::read(path)
        .map_err(|e| EngineError::env(12, format!("read {}: {e}", path.display())))?;
    Ok(hash_of(&bytes))
}

/// 生效 `RuleRegistry` 内容指纹（Debug 形态覆盖全部规则字段）。
pub fn rules_hash(rules: &RuleRegistry) -> u64 {
    hash_of(&format!("{:?}", rules))
}

/// batch 会话：连接池 + 引擎缓存 + 编译产物缓存 + 产物登记。
pub struct BatchSession {
    /// canonical 连接池（每 DbKey 一条；引擎用 try_clone 自持克隆）。
    pool: HashMap<DbKey, DuckExecutor>,
    /// 引擎缓存（命中跳过整个 register/编译路径）。
    engines: HashMap<EngineKey, Engine>,
    /// 跨引擎共享的编译产物缓存。
    compiled: Rc<RefCell<CompiledRoutineCache>>,
    /// 已产出文件（`query --out`、`compile --dir` 计划），供汇总/覆写策略。
    pub produced_artifacts: Vec<PathBuf>,
}

impl BatchSession {
    pub fn new() -> Self {
        Self {
            pool: HashMap::new(),
            engines: HashMap::new(),
            compiled: Rc::new(RefCell::new(CompiledRoutineCache::default())),
            produced_artifacts: Vec::new(),
        }
    }

    /// 共享编译产物缓存（匿名内存引擎等不进 EngineCache 的路径也挂它）。
    pub fn compiled_cache(&self) -> Rc<RefCell<CompiledRoutineCache>> {
        self.compiled.clone()
    }

    /// 已产出文件登记（`query --out` 等；覆写语义与单步 CLI 一致）。
    pub fn record_artifact(&mut self, path: PathBuf) {
        self.produced_artifacts.push(path);
    }

    /// 取 DbKey 的 canonical 连接（首次使用时 open；`:memory:` 显式共享）。
    pub fn canonical(&mut self, key: &DbKey) -> Result<&DuckExecutor, EngineError> {
        if !self.pool.contains_key(key) {
            let ex = key.open()?;
            self.pool.insert(key.clone(), ex);
        }
        Ok(self.pool.get(key).expect("pool entry just inserted"))
    }

    /// 给引擎用的克隆连接（同 instance；绝不二次 open）。
    pub fn clone_executor(&mut self, key: &DbKey) -> Result<DuckExecutor, EngineError> {
        self.canonical(key)?.try_clone()
    }

    /// 命中路径：取引擎并做步边界复位（清报告 + 幂等重装本闭包 MACRO）。
    /// 返回 `true` = 命中（引擎在 map 内，经 `engine_mut` 取用，
    /// `register_sources=false`——catalog 已含全部例程）。
    pub fn engine_hit(&mut self, key: &EngineKey) -> Result<bool, EngineError> {
        let Some(eng) = self.engines.get_mut(key) else {
            return Ok(false);
        };
        eng.reset_for_batch_step();
        eng.refresh_sql_macros()?;
        Ok(true)
    }

    /// 命中路径取引擎（`engine_hit` 返回 true 后调用）。
    pub fn engine_mut(&mut self, key: &EngineKey) -> Option<&mut Engine> {
        self.engines.get_mut(key)
    }

    /// 未命中路径：构建新引擎（克隆连接 + 共享编译缓存），**不注册**——
    /// 注册由 `call_engine(register_sources=true)`/`deploy_apply` 完成，
    /// 成功后调用方 `insert_engine` 回填（失败不留半成品 catalog）。
    pub fn build_engine(&mut self, db: &DbKey, rules: RuleRegistry) -> Result<Engine, EngineError> {
        let ex = self.clone_executor(db)?;
        let mut eng = Engine::new(Box::new(ex), rules);
        eng.attach_compiled_cache(self.compiled.clone());
        Ok(eng)
    }

    /// 注册/执行成功后回填引擎缓存。
    pub fn insert_engine(&mut self, key: EngineKey, eng: Engine) {
        self.engines.insert(key, eng);
    }

    /// 写步（deploy/materialize/query/sync）后：该库全部缓存引擎的
    /// memo/lookup/set_oriented 失效（catalog/宏不动；内核对象由调用方重查）。
    pub fn invalidate_engines_for(&mut self, db: &DbKey) {
        for (k, eng) in self.engines.iter_mut() {
            if &k.db == db {
                eng.invalidate_caches();
            }
        }
    }
}
