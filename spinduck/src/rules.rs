//! RuleRegistry：兼容性规则数据加载（机制层，规则值见 config/rewrite-rules.toml）。
//! 引擎只实现"规则应用"，规则值以 TOML 数据驱动（AGENTS.md §6）。
//!
//! 接线面（本类型消费的种子段落，未列出的段落为登记/探针用途，改写器不读）：
//! - `[functions]`：全表进 map，SQL 上下文改名/下沉（`sql::rewrite_tokens_with_rules`）。
//! - `[types]`：全表进 map，但 `spin_type` 目前**无生产调用点**（语料建表走参照侧
//!   schema 物化）；接入点在 DDL 翻译，见 `spin_type` 文档与 validation.md V18。
//! - `[patterns]`：`rownum_first_row`、`update_strip_alias`、`sequence_nextval`、
//!   `oracle_outer_join`、`concat_null_as_empty` 布尔开关，
//!   由 `should_wire_every_seed_pattern_key` 做漂移守护——新增布尔键必须接线。
//! - `target_duckdb` / `probe_duckdb`：`probe_versions_match`（V6 门禁）。
//! - `[dates.formats]`：载入 `date_formats`，映射表本体仍在
//!   `sql::oracle_format_to_strftime`，由
//!   `should_keep_seed_date_formats_in_sync_with_strftime_mapper` 守护两侧不漂移。
//! - `[implicit] assignment_loose`：写入 `VARCHAR` 槽的宽松转换（`value::coerce_for_slot`）。
//!   `concat_numeric_to_varchar` 仍只作登记（种子值 false）；数值拼接转串的实际口径
//!   已由 `[patterns] concat_null_as_empty` 的 `concat(…)` 承担。
//! - `[ddl]` `[rejects]` `version`：语料事实与探针期望的数据登记，当前无改写器消费。

use std::collections::BTreeMap;
use std::path::Path;

/// 规则注册表：种子 + 项目覆盖叠加。
#[derive(Debug, Clone)]
pub struct RuleRegistry {
    /// `[types]`：GaussDB 类型名 → DuckDB 类型族名
    pub types: BTreeMap<String, String>,
    /// `[functions]`：函数映射（重命名或 @变换标记）
    pub functions: BTreeMap<String, String>,
    /// `[implicit]`：隐式转换开关
    pub implicit_concat_numeric_to_varchar: bool,
    /// `[implicit] assignment_loose`：写入 `VARCHAR` 槽时数字/布尔收成字符串。
    /// 种子为 true；关闭后变体不符返回 `BIZ-0111`。
    pub implicit_assignment_loose: bool,
    /// `[patterns] rownum_first_row`：`rownum = 1` 首行谓词 → `LIMIT 1` 改写开关。
    /// DuckDB 无 rownum，不改写则语句必然内核报错，故默认 true（可经规则文件关闭）。
    pub patterns_rownum_first_row: bool,
    /// `[patterns] update_strip_alias`：`UPDATE t SET t.c = …` → 去别名限定
    /// （DuckDB 不支持 UPDATE .. SET 的限定列名）
    pub patterns_update_strip_alias: bool,
    /// `[patterns] sequence_nextval`：`seq.NEXTVAL` → `nextval('seq')`
    /// （DuckDB 无 `序列.属性` 形态，不改写会把序列名当表名解析）
    pub patterns_sequence_nextval: bool,
    /// `[patterns] oracle_outer_join`：Oracle `(+)` 外连接标记 → ANSI `LEFT JOIN … ON`
    /// （DuckDB 不识别 `(+)`，解析阶段即报 Parser Error）
    pub patterns_oracle_outer_join: bool,
    /// `[patterns] concat_null_as_empty`：A 模式文本拼接（NULL 视作空串）→ 内核 `concat(…)`
    /// （DuckDB 的 `||` 传播 NULL，与基线实测 `'A' || NULL = 'A'` 相反）
    pub patterns_concat_null_as_empty: bool,
    /// `[dates.formats]`：Oracle 整串日期格式 → strftime 格式。
    /// 映射表本体在 `sql::oracle_format_to_strftime`（逐令牌变换，覆盖语料全部格式），
    /// 本字段登记语料事实并做漂移守护（`should_keep_seed_date_formats_in_sync_with_strftime_mapper`）；
    /// 改写器接线待 rewrite.rs 拆分时一并做（见 docs/architecture.md 决策记录）。
    pub date_formats: BTreeMap<String, String>,
    /// 内核版本（探针锚定）
    pub target_duckdb: String,
    pub probe_duckdb: String,
}

/// 内置种子规则（决策 2025-11-05）：`config/rewrite-rules.toml` 编译期嵌入，
/// `RuleRegistry::default()` 直接加载——引擎在零外部配置时即具备 NVL/TO_CHAR/
/// CLOB→VARCHAR 等方言能力；项目/命令行规则文件叠加在其上（overlay 语义）。
pub const BUILT_IN_RULES_TOML: &str = include_str!("../../config/rewrite-rules.toml");

impl Default for RuleRegistry {
    /// 默认 = 内置种子规则（嵌入副本与仓库 config 单元测试互验，见
    /// `should_embed_built_in_rules`）；加载失败回退空表并保留可测路径。
    fn default() -> Self {
        Self::from_toml_str(BUILT_IN_RULES_TOML).unwrap_or_else(|_| Self::empty_registry())
    }
}

impl RuleRegistry {
    /// 程序化空表（内置加载失败的回退路径；测试用构造器）。
    fn empty_registry() -> Self {
        Self {
            types: BTreeMap::new(),
            functions: BTreeMap::new(),
            implicit_concat_numeric_to_varchar: false,
            implicit_assignment_loose: true,
            patterns_rownum_first_row: true,
            patterns_update_strip_alias: true,
            patterns_sequence_nextval: true,
            patterns_oracle_outer_join: true,
            patterns_concat_null_as_empty: true,
            date_formats: BTreeMap::new(),
            target_duckdb: String::new(),
            probe_duckdb: String::new(),
        }
    }
}

impl RuleRegistry {
    /// 加载种子规则文件（config/rewrite-rules.toml）。
    /// 加载项目规则覆盖（§12.1 `[paths].rules`）：逐文件叠加在已加载的种子规则上。
    /// 只允许追加/覆盖值；文件缺失视为配置错误（路径来自 spinduck.toml）。
    pub fn with_project_rules(
        mut self,
        files: &[std::path::PathBuf],
    ) -> Result<Self, crate::error::EngineError> {
        for f in files {
            let raw = std::fs::read_to_string(f).map_err(|e| {
                crate::error::EngineError::env(32, format!("读取项目规则 {}: {e}", f.display()))
            })?;
            self.overlay(&raw)?;
        }
        Ok(self)
    }

    pub fn load(path: &Path) -> Result<Self, crate::error::EngineError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| crate::error::EngineError::env(10, format!("rules file: {e}")))?;
        Self::from_toml_str(&raw)
    }

    /// 解析规则 TOML（种子或项目覆盖；覆盖文件在引擎加载层与种子合并）。
    pub fn from_toml_str(raw: &str) -> Result<Self, crate::error::EngineError> {
        let v: toml::Value = toml::from_str(raw)
            .map_err(|e| crate::error::EngineError::env(11, format!("rules parse: {e}")))?;
        let mut reg = Self {
            target_duckdb: v
                .get("target_duckdb")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            probe_duckdb: v
                .get("probe_duckdb")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            ..Self::empty_registry()
        };
        if let Some(t) = v.get("types").and_then(|x| x.as_table()) {
            for (k, val) in t {
                if let Some(s) = val.as_str() {
                    reg.types.insert(k.to_ascii_uppercase(), s.to_string());
                }
            }
        }
        if let Some(f) = v.get("functions").and_then(|x| x.as_table()) {
            for (k, val) in f {
                if let Some(s) = val.as_str() {
                    reg.functions.insert(k.to_ascii_uppercase(), s.to_string());
                }
            }
        }
        if let Some(i) = v.get("implicit").and_then(|x| x.as_table()) {
            reg.implicit_concat_numeric_to_varchar = i
                .get("concat_numeric_to_varchar")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            if let Some(b) = i.get("assignment_loose").and_then(|x| x.as_bool()) {
                reg.implicit_assignment_loose = b;
            }
        }
        if let Some(p) = v.get("patterns").and_then(|x| x.as_table()) {
            if let Some(b) = p.get("rownum_first_row").and_then(|x| x.as_bool()) {
                reg.patterns_rownum_first_row = b;
            }
            if let Some(b) = p.get("update_strip_alias").and_then(|x| x.as_bool()) {
                reg.patterns_update_strip_alias = b;
            }
            if let Some(b) = p.get("sequence_nextval").and_then(|x| x.as_bool()) {
                reg.patterns_sequence_nextval = b;
            }
            if let Some(b) = p.get("oracle_outer_join").and_then(|x| x.as_bool()) {
                reg.patterns_oracle_outer_join = b;
            }
            if let Some(b) = p.get("concat_null_as_empty").and_then(|x| x.as_bool()) {
                reg.patterns_concat_null_as_empty = b;
            }
        }
        reg.date_formats = date_formats_of(&v);
        Ok(reg)
    }

    /// 项目覆盖叠加：追加新键 / 覆盖既有值；删除不可表达（缺省即不改动）。
    pub fn overlay(&mut self, raw: &str) -> Result<(), crate::error::EngineError> {
        let v: toml::Value = toml::from_str(raw)
            .map_err(|e| crate::error::EngineError::env(12, format!("overlay parse: {e}")))?;
        // 版本锚定字段同样可覆盖（rules probe 的漂移检测依赖）
        if let Some(x) = v.get("target_duckdb").and_then(|x| x.as_str()) {
            self.target_duckdb = x.to_string();
        }
        if let Some(x) = v.get("probe_duckdb").and_then(|x| x.as_str()) {
            self.probe_duckdb = x.to_string();
        }
        if let Some(t) = v.get("types").and_then(|x| x.as_table()) {
            for (k, val) in t {
                if let Some(s) = val.as_str() {
                    self.types.insert(k.to_ascii_uppercase(), s.to_string());
                }
            }
        }
        if let Some(f) = v.get("functions").and_then(|x| x.as_table()) {
            for (k, val) in f {
                if let Some(s) = val.as_str() {
                    self.functions.insert(k.to_ascii_uppercase(), s.to_string());
                }
            }
        }
        if let Some(i) = v.get("implicit").and_then(|x| x.as_table()) {
            if let Some(b) = i.get("assignment_loose").and_then(|x| x.as_bool()) {
                self.implicit_assignment_loose = b;
            }
        }
        if let Some(p) = v.get("patterns").and_then(|x| x.as_table()) {
            if let Some(b) = p.get("rownum_first_row").and_then(|x| x.as_bool()) {
                self.patterns_rownum_first_row = b;
            }
            if let Some(b) = p.get("update_strip_alias").and_then(|x| x.as_bool()) {
                self.patterns_update_strip_alias = b;
            }
            if let Some(b) = p.get("sequence_nextval").and_then(|x| x.as_bool()) {
                self.patterns_sequence_nextval = b;
            }
            if let Some(b) = p.get("oracle_outer_join").and_then(|x| x.as_bool()) {
                self.patterns_oracle_outer_join = b;
            }
            if let Some(b) = p.get("concat_null_as_empty").and_then(|x| x.as_bool()) {
                self.patterns_concat_null_as_empty = b;
            }
        }
        for (k, val) in date_formats_of(&v) {
            self.date_formats.insert(k, val);
        }
        Ok(())
    }

    /// 类型名 → SpinType。映射表没有的名字返回 None（调用方保持该槽无类型）。
    /// 形参与局部变量登记使用本方法；DDL 翻译仍见 `docs/validation.md` V18。
    pub fn spin_type(&self, gauss_name: &str) -> Option<crate::types::SpinType> {
        crate::types::SpinType::from_gauss_name(gauss_name, &self.types)
    }

    /// 探针状态（规则文件升级复测清单，V6）。
    pub fn probe_versions_match(&self) -> bool {
        !self.probe_duckdb.is_empty() && self.probe_duckdb == self.target_duckdb
    }
}

/// `[dates.formats]`：Oracle 整串日期格式 → strftime 格式（键统一大写）。
fn date_formats_of(v: &toml::Value) -> BTreeMap<String, String> {
    v.get("dates")
        .and_then(|d| d.get("formats"))
        .and_then(|x| x.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(k, val)| {
                    val.as_str()
                        .map(|s| (k.to_ascii_uppercase(), s.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = r#"
target_duckdb = "1.5.5"
probe_duckdb = "1.5.5"
[types]
NUMBER = "DECIMAL"
VARCHAR2 = "VARCHAR"
CLOB = "VARCHAR"
[functions]
NVL = "coalesce"
DECODE = "@case"
[implicit]
concat_numeric_to_varchar = false
"#;

    #[test]
    fn should_load_and_overlay_rownum_pattern_flag() {
        let on = RuleRegistry::from_toml_str("[patterns]\nrownum_first_row = true").unwrap();
        assert!(on.patterns_rownum_first_row);
        let off = RuleRegistry::from_toml_str("[patterns]\nrownum_first_row = false").unwrap();
        assert!(!off.patterns_rownum_first_row, "规则文件可显式关闭");
        // 程序化默认：开启（DuckDB 无 rownum，不改写必然报错）
        assert!(RuleRegistry::default().patterns_rownum_first_row);
        let mut d = RuleRegistry::default();
        d.overlay("[patterns]\nrownum_first_row = false").unwrap();
        assert!(!d.patterns_rownum_first_row, "overlay 可覆盖");
    }

    #[test]
    fn should_load_assignment_loose_and_allow_overlay_to_close_it() {
        assert!(RuleRegistry::default().implicit_assignment_loose);
        let mut rules = RuleRegistry::default();
        rules
            .overlay("[implicit]\nassignment_loose = false")
            .unwrap();
        assert!(!rules.implicit_assignment_loose);
    }

    #[test]
    fn should_load_seed_rules_from_toml() {
        let r = RuleRegistry::from_toml_str(SEED).unwrap();
        assert_eq!(r.target_duckdb, "1.5.5");
        assert_eq!(r.types["VARCHAR2"], "VARCHAR");
        assert_eq!(r.functions["NVL"], "coalesce");
        assert!(!r.implicit_concat_numeric_to_varchar);
    }

    #[test]
    fn should_resolve_spin_type_via_registry() {
        let r = RuleRegistry::from_toml_str(SEED).unwrap();
        assert_eq!(
            r.spin_type("NUMBER(15,2)"),
            Some(crate::types::SpinType::Decimal {
                precision: 15,
                scale: 2
            })
        );
    }

    #[test]
    fn should_overlay_project_rules_by_addition_and_override() {
        let mut r = RuleRegistry::from_toml_str(SEED).unwrap();
        r.overlay(
            r#"
[types]
NUMBER = "DECIMAL"
BINARY_DOUBLE = "DOUBLE"
"#,
        )
        .unwrap();
        assert_eq!(r.types["NUMBER"], "DECIMAL"); // 覆盖同值
        assert_eq!(r.types["BINARY_DOUBLE"], "DOUBLE"); // 追加新类型
        assert_eq!(r.types["VARCHAR2"], "VARCHAR"); // 种子未动（缺省即不改动）
    }

    #[test]
    fn should_overlay_override_seed_value() {
        let mut r = RuleRegistry::from_toml_str(SEED).unwrap();
        r.overlay("[types]\nCLOB = \"TEXT\"\n").unwrap();
        assert_eq!(r.types["CLOB"], "TEXT");
        assert_eq!(r.types["NUMBER"], "DECIMAL"); // 其他种子不受影响
    }

    #[test]
    fn should_check_probe_version_alignment() {
        let r = RuleRegistry::from_toml_str(SEED).unwrap();
        assert!(r.probe_versions_match());
        let stale =
            RuleRegistry::from_toml_str("probe_duckdb = \"1.4.4\"\ntarget_duckdb = \"1.5.5\"")
                .unwrap();
        assert!(!stale.probe_versions_match());
    }

    #[test]
    fn should_fall_back_to_default_mapping_when_no_file() {
        let m = crate::types::default_type_mapping();
        assert_eq!(m["VARCHAR2"], "VARCHAR");
    }

    #[test]
    fn should_parse_and_overlay_update_strip_alias() {
        // 回归守护：该开关被静态/动态改写路径消费，曾只在结构体里存在而不可配置
        let off = RuleRegistry::from_toml_str("[patterns]\nupdate_strip_alias = false").unwrap();
        assert!(!off.patterns_update_strip_alias);
        let mut d = RuleRegistry::default();
        d.overlay("[patterns]\nupdate_strip_alias = false").unwrap();
        assert!(!d.patterns_update_strip_alias, "overlay 可关闭");
        assert!(
            RuleRegistry::default().patterns_update_strip_alias,
            "种子 config/rewrite-rules.toml 声明 true"
        );
    }

    #[test]
    fn should_embed_built_in_rules() {
        // default() 必须是种子规则本体，而不是解析失败后的空表回退
        let r = RuleRegistry::default();
        assert_eq!(r.types["VARCHAR2"], "VARCHAR");
        assert_eq!(r.types["CLOB"], "VARCHAR");
        assert_eq!(r.types["NUMBER"], "DECIMAL");
        assert_eq!(r.functions["NVL"], "coalesce");
        assert_eq!(r.target_duckdb, "1.5.5");
        assert!(r.probe_versions_match(), "种子内 probe/target 版本必须一致");
    }

    #[test]
    fn should_wire_every_seed_pattern_key() {
        // 漂移守护：种子 [patterns] 下每个布尔键都必须落到规则字段
        let v: toml::Value = toml::from_str(BUILT_IN_RULES_TOML).expect("内置种子规则必须可解析");
        let p = v
            .get("patterns")
            .and_then(|x| x.as_table())
            .expect("[patterns]");
        let reg = RuleRegistry::default();
        let mut wired = 0usize;
        for (k, val) in p {
            let Some(seed_val) = val.as_bool() else {
                continue;
            };
            let field = match k.as_str() {
                "rownum_first_row" => reg.patterns_rownum_first_row,
                "update_strip_alias" => reg.patterns_update_strip_alias,
                "sequence_nextval" => reg.patterns_sequence_nextval,
                "oracle_outer_join" => reg.patterns_oracle_outer_join,
                "concat_null_as_empty" => reg.patterns_concat_null_as_empty,
                other => panic!("未接线的 [patterns] 布尔键: {other}"),
            };
            assert_eq!(field, seed_val, "[patterns] {k} 与规则字段漂移");
            wired += 1;
        }
        assert_eq!(wired, 5, "种子 [patterns] 布尔键数量变化：必须同步接线");
    }

    #[test]
    fn should_parse_seed_date_formats() {
        let r = RuleRegistry::default();
        assert_eq!(r.date_formats["YYYYMMDD"], "%Y%m%d");
        assert_eq!(r.date_formats["YYYY-MM-DD"], "%Y-%m-%d");
        assert_eq!(r.date_formats["YYYYMMDDHH24MISS"], "%Y%m%d%H%M%S");
    }

    #[test]
    fn should_keep_seed_date_formats_in_sync_with_strftime_mapper() {
        // 漂移守护：种子 [dates.formats] 的每个整串映射都必须等于代码内
        // `oracle_format_to_strftime` 的输出（映射表本体在代码，规则数据是语料事实清单）
        let r = RuleRegistry::default();
        for (oracle, expected) in &r.date_formats {
            assert_eq!(
                &crate::sql::oracle_format_to_strftime(oracle),
                expected,
                "[dates.formats] {oracle} 与 strftime 映射器漂移"
            );
        }
    }

    #[test]
    fn should_overlay_seed_date_formats() {
        let mut r = RuleRegistry::default();
        r.overlay("[dates.formats]\n\"DD-MON-YYYY\" = \"%d-%b-%Y\"\n")
            .expect("overlay");
        assert_eq!(r.date_formats["DD-MON-YYYY"], "%d-%b-%Y");
        assert_eq!(r.date_formats["YYYYMMDD"], "%Y%m%d", "种子未动");
    }
}
