//! 链路校验（§12.1 `[[entry]]` 声明的消费者）：从入口存过全名出发逐对象核验。
//!
//! 核验面（`chain` = 纯静态抽取，`run` = 内核与目录比对）：
//! 入口在例程目录命中且种类一致 → 链路引用的例程已登记 →
//! 链路 SQL 引用的表/视图/序列在内核存在 → 声明该入口的用例场景可执行。
//! 报告是数据，渲染与退出码归 CLI 壳（§1）。

mod chain;
mod deploy;
mod manifest;
mod run;

pub use chain::{chain_object_refs, routine_refs, ObjectKind, ObjectRef};
pub use deploy::{
    clamp_sequence_bounds, deploy_project, deploy_routines, deploy_schema, deploy_seeds,
    drop_cache_option, sql_units, strip_defaults, verify_project,
};
pub use manifest::{manifest_text, sync_runtime_manifest, ManifestSync};
pub use run::{kernel_objects, verify_entry, ChainFacts, Check, CheckKind, EntryReport, Status};
