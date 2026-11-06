//! SpinType：GaussDB 类型名 → DuckDB LogicalType（映射值来自规则文件 `[types]`）。

use std::collections::BTreeMap;

/// 引擎内部类型（与 DuckDB LogicalType 一一映射，映射值来自规则数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpinType {
    Decimal { precision: u8, scale: u8 },
    Int,
    BigInt,
    Varchar,
    Date,
    Timestamp,
    Bool,
    Blob,
}

impl SpinType {
    /// 按 GaussDB 类型名解析（规则文件 `[types]` 驱动）。
    /// 无精度 NUMBER 默认 DECIMAL(38,6)（语义损失见规则文件注释）。
    pub fn from_gauss_name(name: &str, mapping: &BTreeMap<String, String>) -> Option<SpinType> {
        let base = name.trim().to_ascii_uppercase();
        // 拆参数：VARCHAR2(4000) / NUMBER(15,2)；畸形（`NUMBER(` 无右括号）
        // 返回 None → 上层显式"未知类型"，不得切片 panic（回归守护见
        // tests/guard/guard_types_name_parsing.rs）
        let (head, args) = match base.find('(') {
            Some(i) => match base.strip_suffix(')') {
                Some(rest) if rest.len() > i => (&base[..i], &rest[i + 1..]),
                _ => return None,
            },
            None => (base.as_str(), ""),
        };
        let mapped = mapping.get(head)?.as_str();
        match (mapped, args) {
            ("DECIMAL", "") => Some(SpinType::Decimal {
                precision: 38,
                scale: 6,
            }),
            ("DECIMAL", a) => {
                let mut it = a.split(',');
                let p: u8 = it.next()?.trim().parse().ok()?;
                // scale 缺省（`NUMBER(15)`）按 Oracle 语义显式取 0；
                // 坏 scale（`NUMBER(15,x)`）显式返回 None（上层报未知类型），
                // 不得静默变 0（§6 语义损失显式化）
                let s: u8 = match it.next() {
                    Some(x) => x.trim().parse().ok()?,
                    None => 0,
                };
                Some(SpinType::Decimal {
                    precision: p,
                    scale: s,
                })
            }
            ("VARCHAR", _) => Some(SpinType::Varchar),
            ("BLOB", _) => Some(SpinType::Blob),
            ("DATE", _) => Some(SpinType::Date),
            ("TIMESTAMP", _) => Some(SpinType::Timestamp),
            ("BOOLEAN", _) => Some(SpinType::Bool),
            ("INT", _) => Some(SpinType::Int),
            ("BIGINT", _) => Some(SpinType::BigInt),
            _ => None,
        }
    }

    /// DuckDB 侧类型名（用于建表/绑定）。
    pub fn duckdb_name(&self) -> &'static str {
        match self {
            SpinType::Decimal { .. } => "DECIMAL",
            SpinType::Int => "INTEGER",
            SpinType::BigInt => "BIGINT",
            SpinType::Varchar => "VARCHAR",
            SpinType::Date => "DATE",
            SpinType::Timestamp => "TIMESTAMP",
            SpinType::Bool => "BOOLEAN",
            SpinType::Blob => "BLOB",
        }
    }
}

/// 默认类型映射（与 config/rewrite-rules.toml `[types]` 同源；
/// 运行态以 RuleRegistry 加载的映射为准，此处仅供独立单元测试使用）。
pub fn default_type_mapping() -> BTreeMap<String, String> {
    [
        ("NUMBER", "DECIMAL"),
        ("NUMERIC", "DECIMAL"),
        ("DECIMAL", "DECIMAL"),
        ("VARCHAR2", "VARCHAR"),
        ("NVARCHAR2", "VARCHAR"),
        ("CHAR", "VARCHAR"),
        ("TEXT", "VARCHAR"),
        ("CLOB", "VARCHAR"),
        ("BLOB", "BLOB"),
        ("BYTEA", "BLOB"),
        ("DATE", "DATE"),
        ("TIMESTAMP", "TIMESTAMP"),
        ("BOOLEAN", "BOOLEAN"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_map_varchar2_to_varchar() {
        let m = default_type_mapping();
        assert_eq!(
            SpinType::from_gauss_name("VARCHAR2(4000)", &m),
            Some(SpinType::Varchar)
        );
    }

    #[test]
    fn should_map_number_with_precision_to_decimal() {
        let m = default_type_mapping();
        assert_eq!(
            SpinType::from_gauss_name("NUMBER(15,2)", &m),
            Some(SpinType::Decimal {
                precision: 15,
                scale: 2
            })
        );
    }

    #[test]
    fn should_map_single_arg_number_to_scale_zero() {
        // 语料形态锁定（golden/case_corpus_* 的 PL 声明里出现 NUMBER(8)）：
        // Oracle/GaussDB 语义下 NUMBER(p) 即整数，scale 显式为 0，不得默认 6
        let m = default_type_mapping();
        assert_eq!(
            SpinType::from_gauss_name("NUMBER(8)", &m),
            Some(SpinType::Decimal {
                precision: 8,
                scale: 0
            })
        );
    }

    #[test]
    fn should_map_bare_number_to_decimal38x6() {
        let m = default_type_mapping();
        assert_eq!(
            SpinType::from_gauss_name("NUMBER", &m),
            Some(SpinType::Decimal {
                precision: 38,
                scale: 6
            })
        );
    }

    #[test]
    fn should_map_char_with_semantic_loss_to_varchar() {
        // 语义损失（GaussDB CHAR 定长补空格）由规则文件注释声明，映射本身可执行
        let m = default_type_mapping();
        assert_eq!(
            SpinType::from_gauss_name("CHAR(10)", &m),
            Some(SpinType::Varchar)
        );
    }

    #[test]
    fn should_return_none_for_unknown_type_name() {
        let m = default_type_mapping();
        assert_eq!(SpinType::from_gauss_name("BINARY_DOUBLE", &m), None);
    }

    #[test]
    fn should_report_duckdb_names_for_ddl() {
        assert_eq!(SpinType::Varchar.duckdb_name(), "VARCHAR");
        assert_eq!(
            SpinType::Decimal {
                precision: 15,
                scale: 2
            }
            .duckdb_name(),
            "DECIMAL"
        );
    }
}
