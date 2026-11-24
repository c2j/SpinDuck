//! 转换计划绑定集装配：批写/守卫 SQL 实际引用的占位符 → BindSrc（设计 v3.1 §4.2）。

use super::{copy, resolve, route, BindSrc, CopyLoop, SkipReason};

pub(super) fn route_guards(
    copy: &CopyLoop<'_>,
    resolved: &resolve::HoistedLookups,
) -> Result<Vec<String>, SkipReason> {
    let mut guards = Vec::new();
    if let Some((v, lit)) = &copy.route {
        let Some((l, _)) = resolved.iter().find(|(l, _)| &l.var == v) else {
            return Err(SkipReason::UnsupportedControlFlow);
        };
        guards.push(route::guard(l, lit)?);
    }
    Ok(guards)
}

/// §4.2：把批写 SQL/守卫实际引用的 IN 形参占位符并入绑定集（去重）。
pub(super) fn with_param_binds(
    copy: &CopyLoop<'_>,
    generated: &copy::CopySql,
    mut binds: Vec<(String, BindSrc)>,
) -> Vec<(String, BindSrc)> {
    for (ph, src) in param_binds(copy, generated) {
        if !binds.iter().any(|(p, _)| p == &ph) {
            binds.push((ph, src));
        }
    }
    binds
}

/// §4.2：批写 SQL 与行级守卫中实际引用的 IN 形参占位符，运行期按名绑定；
/// 未被引用的形参不携带（避免对未用形参做帧解析）。
pub(super) fn param_binds(
    copy: &CopyLoop<'_>,
    generated: &copy::CopySql,
) -> Vec<(String, BindSrc)> {
    copy.binds
        .iter()
        .filter(|(ph, src)| {
            matches!(src, BindSrc::Var(v) if copy.in_params.contains(&v.0.to_ascii_lowercase()))
                && (generated.sql.contains(ph.as_str())
                    || generated
                        .expr_guards
                        .iter()
                        .any(|g| g.contains(ph.as_str())))
        })
        .cloned()
        .collect()
}

pub(super) fn query_binds(
    copy: &CopyLoop<'_>,
    resolved: &resolve::HoistedLookups,
) -> Vec<(String, BindSrc)> {
    let mut binds = query_binds_inner(copy, resolved);
    for (ph, src) in copy.aggs.iter().flat_map(|a| &a.static_binds) {
        if !binds.iter().any(|(p, _)| p == ph) {
            binds.push((ph.clone(), src.clone()));
        }
    }
    binds
}

fn query_binds_inner(
    copy: &CopyLoop<'_>,
    resolved: &resolve::HoistedLookups,
) -> Vec<(String, BindSrc)> {
    let mut binds = copy.query_binds.to_vec();
    for binds_src in copy
        .lookups
        .iter()
        .map(|l| &l.filter_binds)
        .chain(copy.tuples.iter().map(|t| &t.filter_binds))
        // 内联提升查找（__sd_lkN）的静态过滤绑定同样须随批写 SQL 解析；
        // 其占位符文本在派生表 WHERE 中，与过程级查找同口径去重。
        .chain(resolved.iter().map(|(l, _)| &l.filter_binds))
    {
        for (ph, src) in binds_src {
            // 查找结果已逐行代入投影，不能再从尚未 FETCH 的帧取值。
            if let BindSrc::Var(v) = src {
                if resolved.iter().any(|(l, _)| l.var == *v) {
                    continue;
                }
            }
            if !binds.iter().any(|(p, _)| p == ph) {
                binds.push((ph.clone(), src.clone()));
            }
        }
    }
    binds
}
