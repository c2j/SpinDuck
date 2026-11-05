//! RuleRegistry：兼容性规则数据加载（机制层，规则值见 config/rewrite-rules.toml）。
//! 引擎只实现"规则应用"，规则值以 TOML 数据驱动（AGENTS.md §6）。

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
    /// `[patterns] rownum_first_row`：`rownum = 1` 首行谓词 → `LIMIT 1` 改写开关。
    /// DuckDB 无 rownum，不改写则语句必然内核报错，故默认 true（可经规则文件关闭）。
    pub patterns_rownum_first_row: bool,
    /// 内核版本（探针锚定）
    pub target_duckdb: String,
    pub probe_duckdb: String,
}

impl Default for RuleRegistry {
    /// 程序化默认值：rownum 改写默认开启（DuckDB 无 rownum，不改写必然报错）；
    /// 其余空表/关闭，与 `RuleRegistry::default()` 旧语义兼容。
    fn default() -> Self {
        Self {
            types: BTreeMap::new(),
            functions: BTreeMap::new(),
            implicit_concat_numeric_to_varchar: false,
            patterns_rownum_first_row: true,
            target_duckdb: String::new(),
            probe_duckdb: String::new(),
        }
    }
}

impl RuleRegistry {
    /// 加载种子规则文件（config/rewrite-rules.toml）。
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
            ..Default::default()
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
        }
        if let Some(p) = v.get("patterns").and_then(|x| x.as_table()) {
            if let Some(b) = p.get("rownum_first_row").and_then(|x| x.as_bool()) {
                reg.patterns_rownum_first_row = b;
            }
        }
        Ok(reg)
    }

    /// 项目覆盖叠加：追加新键 / 覆盖既有值；删除不可表达（缺省即不改动）。
    pub fn overlay(&mut self, raw: &str) -> Result<(), crate::error::EngineError> {
        let v: toml::Value = toml::from_str(raw)
            .map_err(|e| crate::error::EngineError::env(12, format!("overlay parse: {e}")))?;
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
        if let Some(p) = v.get("patterns").and_then(|x| x.as_table()) {
            if let Some(b) = p.get("rownum_first_row").and_then(|x| x.as_bool()) {
                self.patterns_rownum_first_row = b;
            }
        }
        Ok(())
    }

    /// 类型名 → SpinType（unknown 返回 None，由调用方报 ENG 域改写缺口）。
    pub fn spin_type(&self, gauss_name: &str) -> Option<crate::types::SpinType> {
        crate::types::SpinType::from_gauss_name(gauss_name, &self.types)
    }

    /// 探针状态（规则文件升级复测清单，V6）。
    pub fn probe_versions_match(&self) -> bool {
        !self.probe_duckdb.is_empty() && self.probe_duckdb == self.target_duckdb
    }
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
}
