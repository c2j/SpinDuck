//! 快照逐值比对（design §6.2 A5 的 `compare` 裁决面）。
//!
//! 裁决只有两条承诺：
//! 1. **判值不判文本**——每个单元格先过 [`crate::norm`]（与黄金层同一份实现，A5 收口）再比；
//!    列名不参与判定，参照侧与本地给回不同别名不是差异，列数不同才是。
//! 2. **判完不早停**——一次跑完所有行，报告是"差异清单"而不是"第一个差异"；红绿由
//!    清单是否为空决定，渲染与退出码归 CLI 壳（design §5.2 D1）。

use crate::error::EngineError;
use crate::norm::norm;
use crate::snapshot::Snapshot;
use std::cmp::Ordering;

/// 差异类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// 同一行同一列给回不同的值（归一后仍不等）
    Value,
    /// 同一行的列数不同——此时不逐列硬对齐，只报列数
    Arity,
    /// 左快照多出来的行（右快照到这一行已经没有内容）
    OnlyInA,
    /// 右快照多出来的行
    OnlyInB,
}

/// 一处差异：定位（行号 1 起 + 列指认）+ 两侧归一后的值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub kind: DiffKind,
    pub row: usize,
    pub column: String,
    pub a: String,
    pub b: String,
}

/// 位置序逐值比对：第 i 行对第 i 行，行数不等时多出来的行整行判为未匹配。
pub fn compare(a: &Snapshot, b: &Snapshot) -> Vec<Diff> {
    let common = a.rows.len().min(b.rows.len());
    let mut out = Vec::new();
    for i in 0..common {
        out.extend(compare_pair(a, &a.rows[i], &b.rows[i], i + 1));
    }
    for (i, row) in a.rows.iter().enumerate().skip(common) {
        out.push(only_in(DiffKind::OnlyInA, i, row));
    }
    for (i, row) in b.rows.iter().enumerate().skip(common) {
        out.push(only_in(DiffKind::OnlyInB, i, row));
    }
    out
}

/// 键序逐值比对（`compare --key k1,k2`）：先按键配对行，再逐列比。
///
/// 为什么要它：千万级实例上 `SELECT` 不保证行序，位置序比对会把同一份数据判成一片红。
/// 键**只定位行，不重排列**——两侧列序仍须一致，否则"第 j 列对第 j 列"没有意义。
/// 键列名在**两侧**都必须存在且同位（[`key_indices_both`]），只校验一侧等于对另一侧放弃判定。
/// 重复键：排序稳定，键相同则保持文件序，于是同键的几行按各自顺序配对。
/// 报告里的 `row` 取**左快照**的行号——`Diff` 没有第二个行号槽位，而裁决时先看本地结果。
pub fn compare_by_key(
    a: &Snapshot,
    b: &Snapshot,
    key: &[String],
) -> Result<Vec<Diff>, EngineError> {
    let idx = key_indices_both(a, b, key)?;
    let order_a = rows_sorted_by_key(a, &idx);
    let order_b = rows_sorted_by_key(b, &idx);
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < order_a.len() && j < order_b.len() {
        let (ra, rb) = (&a.rows[order_a[i]], &b.rows[order_b[j]]);
        match key_tuple(ra, &idx).cmp(&key_tuple(rb, &idx)) {
            Ordering::Equal => {
                out.extend(compare_pair(a, ra, rb, order_a[i] + 1));
                i += 1;
                j += 1;
            }
            // a 的键更小 ⇒ 这个键在 b 剩下的行里不会再出现（b 已按键升序）
            Ordering::Less => {
                out.push(only_in(DiffKind::OnlyInA, order_a[i], ra));
                i += 1;
            }
            Ordering::Greater => {
                out.push(only_in(DiffKind::OnlyInB, order_b[j], rb));
                j += 1;
            }
        }
    }
    for &ri in &order_a[i..] {
        out.push(only_in(DiffKind::OnlyInA, ri, &a.rows[ri]));
    }
    for &ri in &order_b[j..] {
        out.push(only_in(DiffKind::OnlyInB, ri, &b.rows[ri]));
    }
    Ok(out)
}

/// 键列下标（单侧）：列名按大小写不敏感定位（参照侧给回 `K`、本地给回 `k` 是真实差异）。
///
/// 找不到即红，**绝不退回位置序**：把"没比成"说成"比过了"是本文最警惕的静默形态。
fn key_indices(snap: &Snapshot, key: &[String]) -> Result<Vec<usize>, EngineError> {
    key.iter()
        .map(|k| {
            snap.schema
                .iter()
                .position(|c| c.eq_ignore_ascii_case(k))
                .ok_or_else(|| {
                    EngineError::env(
                        31,
                        format!(
                            "compare: --key 列 {k} 不在快照 schema 里（有: {}）",
                            snap.schema.join(", ")
                        ),
                    )
                })
        })
        .collect()
}

/// 键列下标（两侧）：同一把刀过两遍，再要求两侧落在同一列序上。
///
/// 只校验左快照时，参照侧的键列名对不上就**静默退化成位置序**：改名 ⇒ 按位置读到错列当键，
/// 值恰好对上就产出 `diffs=0` 的假绿；换位或行更窄 ⇒ 键读到 `NULL`，判成整片未匹配洪水，
/// 报因指错方向。列序不一致在这里一并判红——键只定位行、不重排列，错位时"第 j 列对第 j 列"
/// 本来就没有意义。
fn key_indices_both(a: &Snapshot, b: &Snapshot, key: &[String]) -> Result<Vec<usize>, EngineError> {
    let idx_a = key_indices(a, key)?;
    let idx_b = key_indices(b, key)?;
    if idx_a != idx_b {
        return Err(EngineError::env(
            31,
            format!(
                "compare: --key 列在两侧快照里位置不同（本地: {} / 参照: {}）",
                a.schema.join(", "),
                b.schema.join(", ")
            ),
        ));
    }
    Ok(idx_a)
}

/// 行的键值元组（逐列归一，所以 `1.50` 与 `1.5` 是同一个键）。
fn key_tuple(row: &[Option<String>], idx: &[usize]) -> Vec<String> {
    idx.iter()
        .map(|&j| match row.get(j) {
            Some(c) => norm(c.clone()),
            None => "NULL".to_string(),
        })
        .collect()
}

/// 按键升序的行下标（`sort_by_key` 稳定 ⇒ 同键保持文件序）。
fn rows_sorted_by_key(snap: &Snapshot, idx: &[usize]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..snap.rows.len()).collect();
    order.sort_by_key(|&i| key_tuple(&snap.rows[i], idx));
    order
}

/// 一对行的差异：列数不等只报列数（避免把"少一列"渲染成一串假值差异），否则逐列比对。
///
/// `row` 是报告里的行号（1 起），由调用方给：位置序取本行序号，键序见
/// [`compare_by_key`] 的口径说明。
fn compare_pair(
    schema: &Snapshot,
    ra: &[Option<String>],
    rb: &[Option<String>],
    row: usize,
) -> Vec<Diff> {
    if ra.len() != rb.len() {
        return vec![Diff {
            kind: DiffKind::Arity,
            row,
            column: "-".into(),
            a: format!("{} 列", ra.len()),
            b: format!("{} 列", rb.len()),
        }];
    }
    (0..ra.len())
        .filter_map(|j| {
            let (na, nb) = (norm(ra[j].clone()), norm(rb[j].clone()));
            if na == nb {
                return None;
            }
            Some(Diff {
                kind: DiffKind::Value,
                row,
                column: column_name(schema, j),
                a: na,
                b: nb,
            })
        })
        .collect()
}

/// 整行未匹配：值侧给出行文本，另一侧写 `-`（"这一行在那边不存在"）。
fn only_in(kind: DiffKind, i: usize, row: &[Option<String>]) -> Diff {
    let (mine, theirs) = (render_row(row), "-".to_string());
    let (a, b) = if kind == DiffKind::OnlyInA {
        (mine, theirs)
    } else {
        (theirs, mine)
    };
    Diff {
        kind,
        row: i + 1,
        column: "-".into(),
        a,
        b,
    }
}

/// 列指认：取左快照的列名，没有列名时按位置（`col1` 起）——报告里不许出现空列名。
fn column_name(snap: &Snapshot, j: usize) -> String {
    snap.schema
        .get(j)
        .cloned()
        .unwrap_or_else(|| format!("col{}", j + 1))
}

/// 行 → `[v1, v2]`（每格已归一，NULL 显式）。
fn render_row(row: &[Option<String>]) -> String {
    let cells: Vec<String> = row.iter().map(|c| norm(c.clone())).collect();
    format!("[{}]", cells.join(", "))
}

#[cfg(test)]
mod tests {
    use super::{compare, Diff, DiffKind};
    use crate::snapshot::Snapshot;

    fn snap(rows: &[&[&str]]) -> Snapshot {
        Snapshot {
            schema: vec!["a".into(), "b".into()],
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| cell(c)).collect())
                .collect(),
        }
    }

    /// `""` 是数据库 NULL 的写法（与快照 JSON 的 `null` 同值），其余按文本。
    fn cell(t: &str) -> Option<String> {
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    }

    #[test]
    fn should_report_no_diff_for_identical_rows() {
        let a = snap(&[&["1", "x"], &["2", "y"]]);
        let b = snap(&[&["1", "x"], &["2", "y"]]);
        assert_eq!(compare(&a, &b), vec![]);
    }

    #[test]
    fn should_judge_values_after_norm_not_by_text() {
        // 归一口径唯一：尾零、`+` 前缀、`.5` 补零都不算差异；这些形态在黄金层一直是绿的
        let a = snap(&[&["1.50", "+42"], &[".5", ""]]);
        let b = snap(&[&["1.5", "42"], &["0.5", "NULL"]]);
        assert_eq!(compare(&a, &b), vec![]);
    }

    #[test]
    fn should_localize_a_single_cell_difference_by_row_and_column() {
        let a = snap(&[&["1", "x"], &["2", "y"]]);
        let b = snap(&[&["1", "x"], &["2", "zz"]]);
        let d = compare(&a, &b);
        assert_eq!(
            d,
            vec![Diff {
                kind: DiffKind::Value,
                row: 2, // 1 起：报告里的行号要能直接指到快照 JSON 的第 n 行
                column: "b".into(),
                a: "y".into(),
                b: "zz".into(),
            }]
        );
    }

    #[test]
    fn should_keep_leading_zeros_as_a_difference_because_norm_does_not_parse_numbers() {
        // 口径的边界：norm 削尾零不削前导零，'00123' ≠ '123'——报告必须把它判成差异，
        // 否则"文本保真"这条承诺就成了摆设
        let a = snap(&[&["00123", "x"]]);
        let b = snap(&[&["123", "x"]]);
        assert_eq!(compare(&a, &b).len(), 1);
    }

    #[test]
    fn should_report_extra_rows_on_either_side_as_unmatched_rows() {
        let a = snap(&[&["1", "x"]]);
        let b = snap(&[&["1", "x"], &["2", "y"]]);
        assert_eq!(
            compare(&a, &b),
            vec![Diff {
                kind: DiffKind::OnlyInB,
                row: 2,
                column: "-".into(),
                a: "-".into(),
                b: "[2, y]".into(),
            }]
        );
        assert_eq!(compare(&b, &a)[0].kind, DiffKind::OnlyInA);
    }

    #[test]
    fn should_report_a_row_with_different_arity_instead_of_misaligning_columns() {
        let a = snap(&[&["1", "x"]]);
        let b = Snapshot {
            schema: vec!["a".into(), "b".into(), "c".into()],
            rows: vec![vec![Some("1".into()), Some("x".into()), Some("z".into())]],
        };
        assert_eq!(
            compare(&a, &b),
            vec![Diff {
                kind: DiffKind::Arity,
                row: 1,
                column: "-".into(),
                a: "2 列".into(),
                b: "3 列".into(),
            }]
        );
    }

    #[test]
    fn should_not_judge_column_names_only_their_count() {
        // 参照侧与本地给回不同的列别名是常态（`count(*)` vs `cnt`）；
        // 裁决的是值，别名进报告是为了指认位置，不是判据
        let a = Snapshot {
            schema: vec!["cnt".into()],
            rows: vec![vec![Some("3".into())]],
        };
        let b = Snapshot {
            schema: vec!["count".into()],
            rows: vec![vec![Some("3".into())]],
        };
        assert_eq!(compare(&a, &b), vec![]);
    }

    #[test]
    fn should_name_the_column_from_the_left_snapshot_and_fall_back_to_position() {
        let a = snap(&[&["1", "x"]]);
        let b = Snapshot {
            schema: vec![],
            rows: vec![vec![Some("1".into()), Some("y".into())]],
        };
        assert_eq!(compare(&a, &b)[0].column, "b");
        // 反过来（左表没有列名）时按位置指认，不得留下空列名
        let d = compare(&b, &a);
        assert_eq!(d[0].column, "col2");
    }

    #[test]
    fn should_report_every_difference_instead_of_stopping_at_the_first() {
        let a = snap(&[&["1", "x"], &["2", "y"], &["3", "z"]]);
        let b = snap(&[&["9", "x"], &["2", "8"], &["3", "z"], &["4", "w"]]);
        let d = compare(&a, &b);
        let kinds: Vec<(usize, String)> = d.iter().map(|x| (x.row, x.column.clone())).collect();
        assert_eq!(
            kinds,
            vec![(1, "a".into()), (2, "b".into()), (4, "-".into())]
        );
    }

    // ------------------------------------------------------------ 键序（--key）

    /// 键序比对：`compare_by_key` 的 `--key` 语义 = "行的到达顺序不算差异，键才算"。
    /// 列序仍必须一致——键只定位行，不重排列。
    use super::compare_by_key;

    fn keyed(rows: &[&[&str]]) -> Snapshot {
        Snapshot {
            schema: vec!["k".into(), "v".into()],
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| cell(c)).collect())
                .collect(),
        }
    }

    fn k(cols: &[&str]) -> Vec<String> {
        cols.iter().map(|c| c.to_string()).collect()
    }

    /// 三列快照（复合键用例）。
    fn snap3(rows: &[&[&str]]) -> Snapshot {
        Snapshot {
            schema: vec!["k".into(), "v".into(), "w".into()],
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| cell(c)).collect())
                .collect(),
        }
    }

    #[test]
    fn should_match_rows_by_key_instead_of_by_arrival_order() {
        // 千万级实例上 `SELECT` 不保证行序，位置序比对会把同一份数据判成一片红
        let a = keyed(&[&["1", "x"], &["2", "y"], &["3", "z"]]);
        let b = keyed(&[&["3", "z"], &["1", "x"], &["2", "y"]]);
        assert_eq!(compare_by_key(&a, &b, &k(&["k"])).unwrap(), vec![]);
    }

    #[test]
    fn should_report_the_non_key_columns_of_a_key_matched_row() {
        let a = keyed(&[&["1", "x"], &["2", "y"]]);
        let b = keyed(&[&["2", "y"], &["1", "zz"]]);
        assert_eq!(
            compare_by_key(&a, &b, &k(&["k"])).unwrap(),
            vec![Diff {
                kind: DiffKind::Value,
                row: 1, // 左快照的行号
                column: "v".into(),
                a: "x".into(),
                b: "zz".into(),
            }]
        );
    }

    #[test]
    fn should_report_unmatched_keys_on_either_side() {
        let a = keyed(&[&["1", "x"], &["2", "y"]]);
        let b = keyed(&[&["1", "x"], &["3", "z"]]);
        let d = compare_by_key(&a, &b, &k(&["k"])).unwrap();
        assert_eq!(
            d,
            vec![
                Diff {
                    kind: DiffKind::OnlyInA,
                    row: 2,
                    column: "-".into(),
                    a: "[2, y]".into(),
                    b: "-".into(),
                },
                Diff {
                    kind: DiffKind::OnlyInB,
                    row: 2,
                    column: "-".into(),
                    a: "-".into(),
                    b: "[3, z]".into(),
                },
            ]
        );
    }

    #[test]
    fn should_normalize_key_cells_before_matching() {
        // 键也是值：`1.50` 与 `1.5` 是同一个键，否则同一行会被判成"两边各多一行"
        let a = keyed(&[&["1.50", "x"], &["+2", "y"]]);
        let b = keyed(&[&["2", "y"], &["1.5", "x"]]);
        assert_eq!(compare_by_key(&a, &b, &k(&["k"])).unwrap(), vec![]);
    }

    #[test]
    fn should_use_every_key_column_and_compare_duplicates_in_file_order() {
        // 重复键：排序是稳定的（键相同则保持原序），于是同键的几行按各自文件里的顺序配对
        let a = keyed(&[&["1", "x"], &["1", "y"]]);
        let b = keyed(&[&["1", "y"], &["1", "x"]]);
        assert_eq!(compare_by_key(&a, &b, &k(&["k"])).unwrap().len(), 2);
    }

    #[test]
    fn should_use_every_key_column_when_matching() {
        // 复合键：单列相同、另一列不同 → 两个键互不匹配
        let a = snap3(&[&["1", "x", "p"], &["1", "y", "q"]]);
        let b = snap3(&[&["1", "y", "q"], &["1", "x", "p"]]);
        assert_eq!(compare_by_key(&a, &b, &k(&["k", "v"])).unwrap(), vec![]);
        // 只取第一列作键时同一键的两行按文件序配对，于是三列里除键外全是差异
        assert_eq!(compare_by_key(&a, &b, &k(&["k"])).unwrap().len(), 4);
    }

    #[test]
    fn should_reject_a_key_column_that_is_not_in_the_schema() {
        // 键名写错不能退化成位置序：那会把"没比"说成"比过了"
        let a = keyed(&[&["1", "x"]]);
        let e = compare_by_key(&a, &a, &k(&["nope"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0031");
        assert!(e.message.contains("nope"), "{}", e.message);
        assert!(e.message.contains("k"), "{}", e.message);
    }

    #[test]
    fn should_reject_a_key_column_renamed_only_in_the_reference_snapshot() {
        // 上面那条用 a、a 同一份快照，看不见两侧的不对称：参照侧键列改名 + 列数相同时，
        // 按位置读参照侧的第 j 列读到的是**别的列**，其值恰好对得上就产出 diffs=0 的假绿
        let a = keyed(&[&["1", "x"]]);
        let b = Snapshot {
            schema: vec!["k_id".into(), "v".into()],
            rows: vec![vec![Some("1".into()), Some("x".into())]],
        };
        let e = compare_by_key(&a, &b, &k(&["k"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0031", "{}", e.message);
        assert!(e.message.contains("k"), "诊断要点名键列: {}", e.message);
        assert!(
            e.message.contains("k_id"),
            "诊断要给出参照侧真实列名: {}",
            e.message
        );
    }

    #[test]
    fn should_reject_a_key_column_missing_in_the_narrower_reference_snapshot() {
        // 参照侧行更窄时按位置读全落 None ⇒ 键恒 NULL ⇒ 整片 OnlyInB 洪水，报因指错方向
        let a = keyed(&[&["1", "x"], &["2", "y"]]);
        let b = Snapshot {
            schema: vec!["v".into()],
            rows: vec![vec![Some("x".into())], vec![Some("y".into())]],
        };
        let e = compare_by_key(&a, &b, &k(&["k"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0031", "{}", e.message);
    }

    #[test]
    fn should_reject_a_key_column_that_sits_at_a_different_position_on_the_right() {
        // 键列名两侧都在、但参照侧换了位置：值比对仍是"第 j 列对第 j 列"，
        // 于是键读到错列——与改名同一种假绿，不能因为"名字找得到"就放过
        let a = keyed(&[&["1", "x"]]);
        let b = Snapshot {
            schema: vec!["v".into(), "k".into()],
            rows: vec![vec![Some("x".into()), Some("1".into())]],
        };
        let e = compare_by_key(&a, &b, &k(&["k"])).unwrap_err();
        assert_eq!(e.code.to_string(), "ENV-0031", "{}", e.message);
    }

    #[test]
    fn should_accept_key_column_names_case_insensitively() {
        // 参照侧给回 `K`、本地给回 `k` 是两侧大小写口径的真实差异
        let a = Snapshot {
            schema: vec!["K".into(), "V".into()],
            rows: vec![vec![Some("1".into()), Some("x".into())]],
        };
        assert!(compare_by_key(&a, &a, &k(&["k"])).unwrap().is_empty());
    }
}
