//! 跨日志关联键：会话标识 `sid` 与调用标识 `cid` 的构造口径。

/// 会话标识：`s-<pid>-<进程启动 epoch 毫秒>`。同一次进程运行内不变，
/// 是 engine→trace→audit→error 互查的起点（§11.2 关联性）。
pub fn session_id(pid: u32, start_epoch_ms: i64) -> String {
    format!("s-{pid}-{start_epoch_ms}")
}

/// 调用标识：`c-<同一会话内的第 N 次 CALL>`，6 位零补齐；超过 6 位原样输出，
/// 不截断也不回绕（ID 撞上等于两条调用链互相冒充）。
pub fn call_id(n: u64) -> String {
    format!("c-{n:06}")
}

#[cfg(test)]
mod tests {
    use super::{call_id, session_id};

    #[test]
    fn should_build_session_and_call_ids_from_injected_inputs() {
        // 两者都是纯函数：pid 与启动毫秒由调用方给（时钟/ID 不作弊藏在函数里）
        assert_eq!(session_id(4242, 1794364267412), "s-4242-1794364267412");
        assert_eq!(call_id(1), "c-000001");
        // 超过 6 位不截断、不回绕：ID 撞上等于两条调用链互相冒充
        assert_eq!(call_id(123456789), "c-123456789");
    }
}
