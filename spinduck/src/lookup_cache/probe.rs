//! 哈希探测、排序和 LIMIT。并列或无法比较时回退原语句。

use super::*;

pub(super) enum ProbeOut {
    Rows(Vec<Row>),
    Fallback,
}

pub(super) fn probe(plan: &Plan, index: &Index, binds: &BindMap) -> ProbeOut {
    match (&plan.kind, index) {
        (Kind::Rows(rows), Index::Rows(map)) => probe_rows(rows, map, binds),
        (Kind::Semi(semi), Index::Semi { outer, inner }) => probe_semi(semi, outer, inner, binds),
        _ => ProbeOut::Fallback,
    }
}

fn probe_rows(plan: &RowPlan, map: &HashMap<Vec<KeyPart>, Vec<Row>>, binds: &BindMap) -> ProbeOut {
    let Some(key) = bind_key(&plan.key_binds, binds) else {
        return ProbeOut::Fallback;
    };
    if key.iter().any(|part| matches!(part, KeyPart::Null)) {
        return ProbeOut::Rows(Vec::new());
    }
    if let Some(sample) = map.keys().next() {
        if !kinds_match(sample, &key) {
            return ProbeOut::Fallback;
        }
    }
    let Some(bucket) = map.get(&key) else {
        return ProbeOut::Rows(Vec::new());
    };
    let Some(filtered) = filter_rows(bucket, &plan.filters, binds) else {
        return ProbeOut::Fallback;
    };
    let Some(ordered) = sort_rows(filtered, &plan.order) else {
        return ProbeOut::Fallback;
    };
    match take_limit(ordered, plan.limit, &plan.order) {
        Some(kept) => ProbeOut::Rows(project(&kept, &plan.proj_at)),
        None => ProbeOut::Fallback,
    }
}

fn probe_semi(
    plan: &SemiPlan,
    outer: &HashMap<Vec<KeyPart>, Vec<Row>>,
    inner: &HashMap<Vec<KeyPart>, HashSet<KeyPart>>,
    binds: &BindMap,
) -> ProbeOut {
    let Some(outer_key) = bind_key(&plan.outer_key_binds, binds) else {
        return ProbeOut::Fallback;
    };
    let Some(inner_key) = bind_key(&plan.inner_key_binds, binds) else {
        return ProbeOut::Fallback;
    };
    if outer_key
        .iter()
        .chain(inner_key.iter())
        .any(|part| matches!(part, KeyPart::Null))
    {
        return ProbeOut::Rows(Vec::new());
    }
    if outer
        .keys()
        .next()
        .is_some_and(|sample| !kinds_match(sample, &outer_key))
        || inner
            .keys()
            .next()
            .is_some_and(|sample| !kinds_match(sample, &inner_key))
    {
        return ProbeOut::Fallback;
    }
    let Some(rows) = outer.get(&outer_key) else {
        return ProbeOut::Rows(Vec::new());
    };
    let Some(members) = inner.get(&inner_key) else {
        return ProbeOut::Rows(Vec::new());
    };
    let matched = rows
        .iter()
        .filter(
            |row| match row.get(plan.outer_match_at).and_then(KeyPart::from_value) {
                Some(part) if !matches!(part, KeyPart::Null) => members.contains(&part),
                _ => false,
            },
        )
        .cloned()
        .collect::<Vec<_>>();
    ProbeOut::Rows(project(&matched, &plan.outer_proj_at))
}

/// SQL 等值会做类型转换。键的 Rust 类型对不上时不能当成 0 行，交给内核。
fn kinds_match(stored: &[KeyPart], probe: &[KeyPart]) -> bool {
    stored.len() == probe.len()
        && stored
            .iter()
            .zip(probe)
            .all(|(left, right)| std::mem::discriminant(left) == std::mem::discriminant(right))
}

fn filter_rows(rows: &[Row], filters: &[shape::Filter], binds: &BindMap) -> Option<Vec<Row>> {
    let mut wanted = Vec::new();
    for filter in filters {
        wanted.push((filter.at, candidate_set(&filter.vals, binds)?));
    }
    Some(
        rows.iter()
            .filter(|row| {
                wanted.iter().all(|(at, set)| {
                    row.get(*at)
                        .and_then(KeyPart::from_value)
                        .is_some_and(|part| !matches!(part, KeyPart::Null) && set.contains(&part))
                })
            })
            .cloned()
            .collect(),
    )
}

fn candidate_set(vals: &[ProbeVal], binds: &BindMap) -> Option<HashSet<KeyPart>> {
    let mut set = HashSet::new();
    for val in vals {
        let part = eval_probe(val, binds)?;
        if !matches!(part, KeyPart::Null) {
            set.insert(part);
        }
    }
    Some(set)
}

fn eval_probe(val: &ProbeVal, binds: &BindMap) -> Option<KeyPart> {
    match val {
        ProbeVal::Bind(name) => KeyPart::from_value(binds.get(name)?),
        ProbeVal::Lit(v) => KeyPart::from_value(v),
        ProbeVal::SubstrConcat {
            bind,
            start,
            len,
            suffix,
        } => {
            let SpinValue::Str(text) = binds.get(bind)? else {
                return None;
            };
            let piece = text
                .chars()
                .skip((*start - 1) as usize)
                .take(*len as usize)
                .collect::<String>();
            Some(KeyPart::Str(format!("{piece}{suffix}")))
        }
    }
}

fn sort_rows(mut rows: Vec<Row>, order: &[shape::SortKey]) -> Option<Vec<Row>> {
    if order.is_empty() {
        return Some(rows);
    }
    if !sort_types_agree(&rows, order) || !sort_text_is_ascii(&rows, order) {
        return None;
    }
    rows.sort_by(|left, right| cmp_rows(left, right, order));
    Some(rows)
}

/// 非 ASCII 文本的排序受排序规则影响，不在进程内重做。
fn sort_text_is_ascii(rows: &[Row], order: &[shape::SortKey]) -> bool {
    rows.iter().all(|row| {
        order.iter().all(|key| match row.get(key.at) {
            Some(SpinValue::Str(text)) => text.is_ascii(),
            _ => true,
        })
    })
}

fn sort_types_agree(rows: &[Row], order: &[shape::SortKey]) -> bool {
    for key in order {
        let Some(proto) = rows.iter().find_map(|row| non_null(row, key.at)) else {
            continue;
        };
        for row in rows {
            if let Some(value) = non_null(row, key.at) {
                if !same_sort_type(proto, value) {
                    return false;
                }
            }
        }
    }
    true
}

fn non_null(row: &Row, at: usize) -> Option<&SpinValue> {
    match row.get(at) {
        Some(SpinValue::Null) | None => None,
        Some(value) => Some(value),
    }
}

fn same_sort_type(left: &SpinValue, right: &SpinValue) -> bool {
    match (left, right) {
        (SpinValue::Int(_), SpinValue::Int(_)) => true,
        (SpinValue::Str(_), SpinValue::Str(_)) => true,
        (SpinValue::Bool(_), SpinValue::Bool(_)) => true,
        (SpinValue::Decimal(_, _, a), SpinValue::Decimal(_, _, b)) => a == b,
        _ => false,
    }
}

fn cmp_rows(left: &Row, right: &Row, order: &[shape::SortKey]) -> std::cmp::Ordering {
    for key in order {
        let ord = cmp_at(left, right, key.at, key.desc, key.nulls_first)
            .unwrap_or(std::cmp::Ordering::Equal);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn cmp_at(
    left: &Row,
    right: &Row,
    at: usize,
    desc: bool,
    nulls_first: bool,
) -> Option<std::cmp::Ordering> {
    let a = left.get(at)?;
    let b = right.get(at)?;
    let ord = match (a, b) {
        (SpinValue::Null, SpinValue::Null) => std::cmp::Ordering::Equal,
        (SpinValue::Null, _) => {
            if nulls_first {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (_, SpinValue::Null) => {
            if nulls_first {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        _ => {
            let c = cmp_value(a, b)?;
            if desc {
                c.reverse()
            } else {
                c
            }
        }
    };
    Some(ord)
}

fn cmp_value(left: &SpinValue, right: &SpinValue) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (SpinValue::Int(a), SpinValue::Int(b)) => Some(a.cmp(b)),
        (SpinValue::Str(a), SpinValue::Str(b)) => Some(a.cmp(b)),
        (SpinValue::Bool(a), SpinValue::Bool(b)) => Some(a.cmp(b)),
        (SpinValue::Decimal(a, _, sa), SpinValue::Decimal(b, _, sb)) if sa == sb => Some(a.cmp(b)),
        _ => None,
    }
}

/// `None` 表示排序键并列，调用方回退到原语句。
fn take_limit(rows: Vec<Row>, limit: Option<usize>, order: &[shape::SortKey]) -> Option<Vec<Row>> {
    let Some(n) = limit else {
        return Some(rows);
    };
    if n == 0 {
        return Some(Vec::new());
    }
    if rows.len() <= n {
        return Some(rows);
    }
    if sort_equal(&rows[n - 1], &rows[n], order) {
        return None;
    }
    Some(rows.into_iter().take(n).collect())
}

fn sort_equal(left: &Row, right: &Row, order: &[shape::SortKey]) -> bool {
    order.iter().all(|key| {
        cmp_at(left, right, key.at, false, key.nulls_first) == Some(std::cmp::Ordering::Equal)
    })
}

fn project(rows: &[Row], proj_at: &[usize]) -> Vec<Row> {
    rows.iter()
        .map(|row| {
            proj_at
                .iter()
                .filter_map(|at| row.get(*at).cloned())
                .collect()
        })
        .collect()
}

fn bind_key(names: &[String], binds: &BindMap) -> Option<Vec<KeyPart>> {
    names
        .iter()
        .map(|name| KeyPart::from_value(binds.get(name)?))
        .collect()
}
