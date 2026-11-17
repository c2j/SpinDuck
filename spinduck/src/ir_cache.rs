//! IR 编译缓存（二期，设计 V4 §5.4）：按 [`CompileGateStamp`] 失效的
//! 例程编译产物持久化，命中时跳过 tokenize/parse/编译全链路。
//!
//! 口径：
//! - 键 = `stamp_key + context_hash + 文本哈希`。`stamp` 覆盖全部源
//!   文件、规则注册表与 parser/kernel 版本；`context_hash` 覆盖本次
//!   注册前的例程名/宏名集合——例程互调下沉（fn_sites）与宏消歧依赖
//!   目录上下文，保证同一 stamp 下不同注册顺序不串缓存；
//! - 值 = 该文件产出的 `CompiledRoutine` 列表（非例程语句本就不产生
//!   编译产物，空结果不缓存，静默丢弃护栏路径不受影响）；
//! - 文件 = `.spinduck/ir-cache/<stamp_key>.json`（serde_json，零新增
//!   依赖）。stamp 失配即换文件名天然失效；损坏文件按 miss 处理并在
//!   下次写入时重建——缓存是加速层，绝不阻塞编译；
//! - 哈希用 FNV-1a 64：非加密用途（仅变更检测），实现固定不随
//!   Rust 版本漂移；序列化面全部走 serde 派生。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalog::CompiledRoutine;
use crate::rules::RuleRegistry;

/// ogsql-parser 依赖 pin（`spinduck/Cargo.toml` [dependencies] 次版本锁）。
/// 升级走评估 PR，版本变化必须反映到 stamp。
const PARSER_PIN: &str = "ogsql-parser=0.11";
/// duckdb crate 依赖 pin（对应 DuckDB 1.5.5；内核升级 = V6 门禁）。
const KERNEL_PIN: &str = "duckdb=1.10505.0";

/// FNV-1a 64：稳定、无依赖的变更检测哈希（非安全用途）。
fn fnv(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in p.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hex64(v: u64) -> String {
    format!("{v:016x}")
}

/// 一次编译会话的失效指纹。
#[derive(Debug, Clone)]
pub struct CompileGateStamp {
    /// 源文件内容摘要（`rel → fnv(text)`），覆盖 Gate 1A–2B 全部输入。
    pub sources: BTreeMap<String, u64>,
    /// 规则注册表摘要（`[types]`/`[functions]`/`[patterns]` 等全部规则值
    /// 的 Debug 规范形；`BTreeMap` 保证序稳定）。
    pub rules: u64,
    /// 解析器 pin。
    pub parser: &'static str,
    /// 内核 pin。
    pub kernel: &'static str,
}

impl CompileGateStamp {
    /// 由文件集（`rel → 文本`）与规则注册表计算 stamp。
    pub fn compute(files: &[(String, String)], rules: &RuleRegistry) -> Self {
        let sources = files
            .iter()
            .map(|(rel, text)| (rel.clone(), fnv(&[text])))
            .collect();
        Self {
            sources,
            rules: fnv(&[&format!("{rules:?}")]),
            parser: PARSER_PIN,
            kernel: KERNEL_PIN,
        }
    }

    /// stamp 的稳定键（缓存文件名分量）。
    pub fn key(&self) -> String {
        let mut parts: Vec<String> = vec![
            PARSER_PIN.to_string(),
            KERNEL_PIN.to_string(),
            format!("{:x}", self.rules),
        ];
        for (rel, digest) in &self.sources {
            parts.push(rel.clone());
            parts.push(format!("{digest:x}"));
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        hex64(fnv(&refs))
    }
}

/// 单个缓存条目：一次 `register_named` 调用产出的例程列表。
type CacheMap = BTreeMap<String, Vec<CompiledRoutine>>;

/// IR 编译缓存：内存镜像 + 落盘（每次写穿，文件数少、体量小）。
pub struct IrCache {
    path: PathBuf,
    map: CacheMap,
}

impl IrCache {
    /// 打开（或初始化）`dir/<stamp_key>.json`。损坏/不可解析按空表处理
    /// 并在下次写入时重建——缓存是加速层，绝不阻塞编译。
    pub fn open(dir: &Path, stamp: &CompileGateStamp) -> Self {
        let path = dir.join(format!("{}.json", stamp.key()));
        let map = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheMap>(&bytes).ok())
            .unwrap_or_default();
        Self { path, map }
    }

    /// 注册上下文 + 文件文本 → 缓存键。
    /// `context` 由调用方给出（例程名/宏名集合的规范摘要）。
    pub fn key_for(context: &str, text: &str) -> String {
        hex64(fnv(&[context, text]))
    }

    /// 命中返回该文件的例程列表（克隆；目录注册仍由 Engine 完成）。
    pub fn get(&self, key: &str) -> Option<Vec<CompiledRoutine>> {
        self.map.get(key).cloned()
    }

    /// 写穿：更新内存并整体落盘。空例程列表不入缓存。
    pub fn put(&mut self, key: String, routines: Vec<CompiledRoutine>) {
        if routines.is_empty() {
            return;
        }
        self.map.insert(key, routines);
        self.flush();
    }

    /// 序列化整表写盘；IO 失败静默（缓存是优化层）。
    pub fn flush(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(&self.map) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CompiledRoutine, ParamMode, RoutineKind};
    use crate::ir::{IrProg, IrStmt};
    use crate::types::SpinType;
    use crate::value::SpinValue;

    fn rules() -> RuleRegistry {
        RuleRegistry::default()
    }

    fn prog() -> IrProg {
        IrProg {
            code: vec![IrStmt::Return {
                value: Some(crate::ir::IrExpr::Const(SpinValue::Int(1))),
            }],
            var_types: BTreeMap::from([("v_x".to_string(), SpinType::Int)]),
            local_ddl: Vec::new(),
        }
    }

    fn routine(name: &str) -> CompiledRoutine {
        CompiledRoutine {
            name: name.to_string(),
            kind: RoutineKind::Function,
            params: vec![crate::catalog::RoutineParam {
                name: "p".to_string(),
                mode: ParamMode::In,
                default: None,
                ty: Some(SpinType::Int),
            }],
            prog: prog(),
        }
    }

    #[test]
    fn should_hit_after_store_and_roundtrip_prog() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = CompileGateStamp::compute(&[("a.sql".into(), "SELECT 1".into())], &rules());
        let mut cache = IrCache::open(dir.path(), &stamp);
        let key = IrCache::key_for("ctx", "CREATE FUNCTION f RETURN 1");
        cache.put(key.clone(), vec![routine("f")]);

        let cache2 = IrCache::open(dir.path(), &stamp);
        let hit = cache2.get(&key).expect("命中");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "f");
        assert_eq!(hit[0].prog.code.len(), 1);
        assert_eq!(hit[0].prog.var_types.get("v_x"), Some(&SpinType::Int));
        assert!(cache2.get("other").is_none());
    }

    #[test]
    fn should_miss_on_source_or_rules_change() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![("a.sql".to_string(), "SELECT 1".to_string())];
        let stamp = CompileGateStamp::compute(&files, &rules());
        let mut cache = IrCache::open(dir.path(), &stamp);
        cache.put(IrCache::key_for("c", "t"), vec![routine("f")]);

        // 源变化 → stamp.key 变 → 新文件 → miss
        let stamp2 =
            CompileGateStamp::compute(&[("a.sql".to_string(), "SELECT 2".to_string())], &rules());
        assert_ne!(stamp.key(), stamp2.key());
        let cache2 = IrCache::open(dir.path(), &stamp2);
        assert!(cache2.get(&IrCache::key_for("c", "t")).is_none());

        // 规则变化 → 同样 miss（种子表已含 NUMBER，改用独有键保证差异）
        let mut r2 = rules();
        r2.types.insert("SD_TEST_TYPE".into(), "VARCHAR".into());
        let stamp3 = CompileGateStamp::compute(&files, &r2);
        assert_ne!(stamp.key(), stamp3.key());
    }

    #[test]
    fn should_treat_corrupt_cache_as_miss() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = CompileGateStamp::compute(&[], &rules());
        let path = dir.path().join(format!("{}.json", stamp.key()));
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path, b"not-a-json-blob").unwrap();
        let cache = IrCache::open(dir.path(), &stamp);
        assert!(cache.get("any").is_none());
    }

    #[test]
    fn should_not_cache_empty_routine_list() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = CompileGateStamp::compute(&[], &rules());
        let mut cache = IrCache::open(dir.path(), &stamp);
        cache.put("k".into(), Vec::new());
        assert!(cache.get("k").is_none());
    }

    #[test]
    fn should_replay_identical_ir_across_engine_instances() {
        use crate::engine::Engine;
        use crate::exec::FakeExecutor;

        let dir = tempfile::tempdir().unwrap();
        let sql = "CREATE OR REPLACE FUNCTION f(p INT) RETURN p + 1";
        let files = vec![("a.sql".to_string(), sql.to_string())];
        let rules = rules();
        let stamp = CompileGateStamp::compute(&files, &rules);

        let mut e1 = Engine::new(Box::new(FakeExecutor::default()), rules.clone());
        e1.ir_cache = Some(IrCache::open(dir.path(), &stamp));
        assert_eq!(e1.register(sql).unwrap(), 1);
        let j1 = serde_json::to_string(&e1.catalog.lookup("f").unwrap().prog).unwrap();

        // 第二个 Engine 同 stamp 同上下文：命中缓存，IR 必须逐字节一致
        let mut e2 = Engine::new(Box::new(FakeExecutor::default()), rules);
        e2.ir_cache = Some(IrCache::open(dir.path(), &stamp));
        assert_eq!(e2.register(sql).unwrap(), 1);
        let j2 = serde_json::to_string(&e2.catalog.lookup("f").unwrap().prog).unwrap();
        assert_eq!(j1, j2);
    }
}
