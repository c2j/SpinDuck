//! 零执行器候选计划编译；运行期只消费产物并复核结构与数据守卫。

use super::metadata::SchemaSnapshot;
use super::{entry::ConversionPlan, schema, OptimizeCtx, SetOrientedMode, SkipReason};
use crate::catalog::RoutineCatalog;
use crate::error::EngineError;
use std::collections::BTreeMap;

/// 静态结构供给，不包含 SQL 执行能力。
pub trait SchemaProvider {
    /// 已按部署规则规范化的 codeweb 结构快照。
    fn schema_snapshot(&self) -> &SchemaSnapshot;
}

impl SchemaProvider for SchemaSnapshot {
    fn schema_snapshot(&self) -> &SchemaSnapshot {
        self
    }
}

/// 原 IR 的独立集合化候选；生成不读取任何运行库。
#[derive(Clone)]
pub struct CandidatePlan {
    pub(crate) conversion: ConversionPlan,
    fingerprint: String,
}

impl CandidatePlan {
    /// 完整批写 SQL；所有表达式与查找已关系化。
    pub fn sql(&self) -> &str {
        &self.conversion.sql
    }

    /// 生成时的静态结构指纹。
    pub fn schema_fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// 同一目录、规则、模式和结构快照的完整候选集。
pub struct CompiledPlans {
    plans: BTreeMap<String, Result<CandidatePlan, SkipReason>>,
    catalog_revision: std::sync::Arc<()>,
    rules: crate::rules::RuleRegistry,
    mode: SetOrientedMode,
    pub(crate) snapshot: SchemaSnapshot,
}

impl CompiledPlans {
    /// 按例程名获取静态候选或拒绝原因。
    pub fn get(&self, name: &str) -> Option<&Result<CandidatePlan, SkipReason>> {
        self.plans.get(&name.to_ascii_lowercase())
    }

    pub(crate) fn is_current(&self, ctx: &OptimizeCtx<'_>) -> bool {
        !ctx.batch_commit
            && self.mode == ctx.mode
            && std::sync::Arc::ptr_eq(&self.catalog_revision, ctx.catalog.revision())
            && self.rules == *ctx.rules
            && ctx.snapshot == Some(&self.snapshot)
    }
}

/// 为目录内每个例程编译候选或保守拒绝原因；不实例化执行器。
pub fn compile_set_oriented_plans(
    catalog: &RoutineCatalog,
    provider: &dyn SchemaProvider,
    mode: SetOrientedMode,
) -> Result<CompiledPlans, EngineError> {
    let snapshot = provider.schema_snapshot();
    let ctx = OptimizeCtx {
        mode,
        batch_commit: false,
        rules: catalog.sql_rules(),
        snapshot: Some(snapshot),
        catalog,
    };
    let source = schema::Source::Static(snapshot);
    let mut plans = BTreeMap::new();
    for name in catalog.names() {
        if let Some(routine) = catalog.lookup(&name) {
            let candidate = super::plan_from(routine, &source, &ctx)?
                .map_err(|e| e.reason)
                .map(|conversion| CandidatePlan {
                    conversion,
                    fingerprint: snapshot.fingerprint.clone(),
                });
            plans.insert(name, candidate);
        }
    }
    Ok(CompiledPlans {
        plans,
        catalog_revision: catalog.revision().clone(),
        rules: catalog.sql_rules().clone(),
        mode,
        snapshot: snapshot.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_invalidate_on_catalog_rules_mode_batch_or_snapshot_changes() {
        let compiled = crate::catalog::compile_sources(
            &["CREATE PROCEDURE noop IS BEGIN NULL; END;"],
            crate::rules::RuleRegistry::default(),
        )
        .unwrap();
        let catalog = compiled.catalog;
        let snapshot = SchemaSnapshot {
            tables: Vec::new(),
            diagnostics: Vec::new(),
            fingerprint: "same".into(),
        };
        let plans =
            compile_set_oriented_plans(&catalog, &snapshot, SetOrientedMode::Strict).unwrap();
        let ctx = OptimizeCtx {
            mode: SetOrientedMode::Strict,
            batch_commit: false,
            rules: catalog.sql_rules(),
            snapshot: Some(&snapshot),
            catalog: &catalog,
        };
        assert!(plans.is_current(&ctx));
        for mode in [SetOrientedMode::Off, SetOrientedMode::BatchAtomic] {
            assert!(!plans.is_current(&OptimizeCtx { mode, ..ctx }));
        }
        assert!(!plans.is_current(&OptimizeCtx {
            batch_commit: true,
            ..ctx
        }));
        let mut rules = catalog.sql_rules().clone();
        rules.implicit_assignment_loose = !rules.implicit_assignment_loose;
        assert!(!plans.is_current(&OptimizeCtx {
            rules: &rules,
            ..ctx
        }));
        let mut registered = catalog.clone();
        registered.register(catalog.lookup("noop").unwrap().clone());
        assert!(!plans.is_current(&OptimizeCtx {
            catalog: &registered,
            ..ctx
        }));
        let mut revised_rules = catalog.clone();
        revised_rules.set_sql_rules(rules);
        assert!(!plans.is_current(&OptimizeCtx {
            catalog: &revised_rules,
            ..ctx
        }));
        let mut changed = snapshot.clone();
        changed.diagnostics.push("incomplete DDL".into());
        assert_eq!(changed.fingerprint, snapshot.fingerprint);
        assert!(!plans.is_current(&OptimizeCtx {
            snapshot: Some(&changed),
            ..ctx
        }));
        assert!(!plans.is_current(&OptimizeCtx {
            snapshot: None,
            ..ctx
        }));
        assert!(plans.is_current(&ctx));
    }
}
