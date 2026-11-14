//! 链路校验（§12.1 `[[entry]]` 声明的消费者）：从入口存过全名出发逐对象核验。
//!
//! 核验面（`chain` = 纯静态抽取，`run` = 内核与目录比对）：
//! 入口在例程目录命中且种类一致 → 链路引用的例程已登记 →
//! 链路 SQL 引用的表/视图/序列在内核存在 → 声明该入口的用例场景可执行。
//! 报告是数据；`verify` 子命令已删除（issue #38），本模块只保留库级核验入口，
//! 由守护测试消费（§1）。

mod chain;
mod deploy;
mod run;

pub use chain::{chain_object_refs, routine_refs, ObjectKind, ObjectRef};
pub(crate) use chain::{expr_func_names, is_object_name, normalize, tokens, Tok};
pub use deploy::{
    clamp_sequence_bounds, deploy_project, deploy_routines, deploy_schema, deploy_seeds,
    drop_cache_option, sql_units, strip_defaults, verify_project, ENGINE_PROVIDED_TABLES,
};
// 计划面 `deploy`（`src/plan/deploy.rs`）复用的逐文件下发实现：口径只有一条，
// 允许 crate 内消费，不对外扩接缝。
pub(crate) use deploy::{
    deploy_ddl_file, deploy_view_file, ensure_engine_provided_tables, register_routine_files,
};
pub use run::{kernel_objects, verify_entry, ChainFacts, Check, CheckKind, EntryReport, Status};
