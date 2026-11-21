//! 混合常量兜底 trap 的查找解析（region::mixed 结构面的关系化裁决）：
//! 保护区深度 0 SELECT INTO 按处理器兜底常量逐条 parse_total，产出
//! pc → LookupQuery 映射供 Body 走查在对应 pc 消费；写集依赖检查与
//! fallback_group_at 同口径。

use std::collections::BTreeMap;

use super::super::region::{self, CursorRegion};
use super::super::SkipReason;
use super::{parse_total, LookupQuery};
use crate::catalog::RoutineCatalog;
use crate::ir::{IrProg, IrStmt};

/// mixed_fallback_group_at 的产物：待定查找表与走查边界。
pub(crate) struct MixedGroup {
    /// SqlQuery pc → 解析完成的查找（含兜底常量）。
    pub lookups: BTreeMap<usize, LookupQuery>,
    /// 保护区深度 0 PopTrap pc（走查上界，含）。
    pub pop_pc: usize,
    /// 续跑 pc（处理器末 Jump 与 PopTrap 后 Jump 的共同目标）。
    pub resume_pc: usize,
}

/// 识别 pc 处 PushTrap 的混合常量兜底形态并解析全部深度 0 查找；
/// 形态不符 = Ok(None)；全定义证明失败 = Err(UnprovenQueryTotalness)。
pub(crate) fn mixed_fallback_group_at(
    prog: &IrProg,
    pc: usize,
    region: &CursorRegion,
    catalog: &RoutineCatalog,
) -> Result<Option<MixedGroup>, SkipReason> {
    let IrStmt::PushTrap { entries } = &prog.code[pc] else {
        return Ok(None);
    };
    let [entry] = entries.as_slice() else {
        return Ok(None);
    };
    let Some(shape) = region::mixed_fallback_group(prog, pc, entry) else {
        return Ok(None);
    };
    let writes = region::write_set(prog, region, catalog)?;
    let mut lookups = BTreeMap::new();
    for q in &shape.queries {
        let IrStmt::SqlQuery { sql, binds, .. } = &prog.code[q.pc] else {
            return Ok(None);
        };
        let query = parse_total(
            sql,
            q.var.clone(),
            super::parse::fallback_sql(&q.default)?,
            binds,
            &region.record,
        )?;
        if writes.contains(&query.table.0)
            || query.in_filters.iter().any(|f| writes.contains(&f.table.0))
        {
            return Err(SkipReason::TargetReadDependency);
        }
        lookups.insert(q.pc, query);
    }
    Ok(Some(MixedGroup {
        lookups,
        pop_pc: shape.pop_pc,
        resume_pc: shape.resume_pc,
    }))
}
