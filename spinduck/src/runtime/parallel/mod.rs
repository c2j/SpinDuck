//! loop-parallel Worker 运行时（设计 §3.1/§3.2）：纯 CFG 前缀的
//! 只读并行求值。Worker 沙箱零数据库接触；主线程独占 `FETCH`、
//! trace 顺序、suffix/续跑执行、事务与写缓冲。

pub(crate) mod calls;
pub(crate) mod pool;
#[cfg(test)]
mod tests;
pub(crate) mod worker;
