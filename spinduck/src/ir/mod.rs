//! IR：PlStatement 序列 → 扁平指令序列（标签解析为 PC 跳转），M2 范围：
//! 赋值、IF、LOOP、WHILE、EXIT/CONTINUE、GOTO、RETURN、RAISE、
//! 事务标记与嵌套块。SQL 类语句自 M3 起逐步纳入，未纳入的编译期
//! 显式拒绝（ENG 域），不静默错误翻译。
//!
//! 规模拆分（AGENTS §3，单文件 ≤500 行）：本文件只做模块装配与对外表面，
//! 实现分在下级模块——`types`（IR 类型）、`entry`（编译入口与 SQL 改写管线）、
//! `control`（编译器状态、标签与跳转）、`stmt`（语句编译）、
//! `sql`（SQL 语句/赋值目标/循环标签）、`expr`（表达式编译与原子量）。

mod control;
mod entry;
mod expr;
mod sql;
mod stmt;
mod types;

pub(crate) use control::Compiler;
pub use entry::{
    compile_block, compile_block_with, compile_routine_with, compile_routine_with_names,
    compile_stmts, compile_stmts_with, rewrite_sql,
};
pub(crate) use expr::{ident0, ident1, ident_path, parse_decimal_literal};
pub(crate) use types::CompiledCursorSql;
pub use types::{
    AssignTarget, BinOp, BindSrc, CursorSource, IrExpr, IrProg, IrStmt, Label, TrapEntry, VarName,
};
