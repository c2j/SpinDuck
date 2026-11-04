//! 引擎错误类型：故障域（§11.2.2 归域判定链）在产生点标注。

use std::fmt;

/// 错误码 Newtype（AGENTS.md §4：`ENG-####` / `KERNEL-####` / `BIZ-####` / `ENV-####`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ErrorCode(pub String);

impl ErrorCode {
    pub fn engine(n: u16) -> Self {
        ErrorCode(format!("ENG-{n:04}"))
    }
    pub fn kernel(n: u16) -> Self {
        ErrorCode(format!("KERNEL-{n:04}"))
    }
    pub fn biz(n: u16) -> Self {
        ErrorCode(format!("BIZ-{n:04}"))
    }
    pub fn env(n: u16) -> Self {
        ErrorCode(format!("ENV-{n:04}"))
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 故障域（产生点标注，禁止上层改判）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultDomain {
    Engine,
    Kernel,
    Business,
    Env,
}

/// 引擎错误：域 + 错误码 + 消息。
#[derive(Debug, Clone)]
pub struct EngineError {
    pub domain: FaultDomain,
    pub code: ErrorCode,
    pub message: String,
}

impl EngineError {
    pub fn new(domain: FaultDomain, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            domain,
            code,
            message: message.into(),
        }
    }
    /// 引擎内部断言失败（唯一可确定为 ENGINE 域的信号，§11.2.2）。
    pub fn invariant(code: u16, message: impl Into<String>) -> Self {
        Self::new(FaultDomain::Engine, ErrorCode::engine(code), message)
    }
    /// SQL 执行错误（内核侧返回）。
    pub fn kernel(code: u16, message: impl Into<String>) -> Self {
        Self::new(FaultDomain::Kernel, ErrorCode::kernel(code), message)
    }
    /// 业务域错误（RAISE / 过程内预期错误）。
    pub fn business(code: u16, message: impl Into<String>) -> Self {
        Self::new(FaultDomain::Business, ErrorCode::biz(code), message)
    }
    /// 环境域错误。
    pub fn env(code: u16, message: impl Into<String>) -> Self {
        Self::new(FaultDomain::Env, ErrorCode::env(code), message)
    }
    /// GaussDB PL 命名异常：NO_DATA_FOUND（SELECT INTO 0 行）。
    pub fn no_data_found() -> Self {
        Self::business(100, "NO_DATA_FOUND")
    }
    /// GaussDB PL 命名异常：TOO_MANY_ROWS（SELECT INTO ≥2 行）。
    pub fn too_many_rows() -> Self {
        Self::business(101, "TOO_MANY_ROWS")
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {}: {}",
            self.code,
            domain_name(self.domain),
            self.message
        )
    }
}

impl std::error::Error for EngineError {}

/// 异常条件匹配（AGENTS §4：错误码表为规则数据；v1 静态表锚定探针）。
/// cond 归一化小写：`others` 通配；命名条件映射错误码；SQLSTATE 串等值匹配。
pub fn cond_matches(cond: &str, e: &EngineError) -> bool {
    let c = cond.trim().to_ascii_lowercase();
    match c.as_str() {
        "others" => true,
        "no_data_found" => e.code.0 == "BIZ-0100",
        "too_many_rows" => e.code.0 == "BIZ-0101",
        sqlstate
            if sqlstate.len() == 5
                && sqlstate
                    .chars()
                    .all(|ch| ch.is_ascii_digit() || ch.is_ascii_uppercase()) =>
        {
            e.message.contains(sqlstate) || e.code.0.ends_with(sqlstate)
        }
        _ => false,
    }
}

fn domain_name(d: FaultDomain) -> &'static str {
    match d {
        FaultDomain::Engine => "ENGINE",
        FaultDomain::Kernel => "KERNEL",
        FaultDomain::Business => "BUSINESS",
        FaultDomain::Env => "ENV",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_tag_fault_domain_at_construction_point() {
        let e = EngineError::business(100, "NO_DATA_FOUND");
        assert_eq!(e.domain, FaultDomain::Business);
        assert_eq!(e.code.to_string(), "BIZ-0100");
    }

    #[test]
    fn should_format_error_with_code_and_domain() {
        let e = EngineError::invariant(7, "IR state illegal");
        assert_eq!(e.to_string(), "[ENG-0007] ENGINE: IR state illegal");
    }

    #[test]
    fn should_raise_named_exceptions_for_select_into_rowcount() {
        assert_eq!(EngineError::no_data_found().code.to_string(), "BIZ-0100");
        assert_eq!(EngineError::too_many_rows().code.to_string(), "BIZ-0101");
    }
}

#[cfg(test)]
mod cond_tests {
    use super::*;

    #[test]
    fn should_match_named_conditions_by_error_code() {
        let e = EngineError::no_data_found();
        assert!(cond_matches("no_data_found", &e));
        assert!(cond_matches("NO_DATA_FOUND", &e));
        assert!(!cond_matches("too_many_rows", &e));
    }

    #[test]
    fn should_match_others_wildcard() {
        assert!(cond_matches("others", &EngineError::kernel(1, "x")));
        assert!(cond_matches("OTHERS", &EngineError::no_data_found()));
    }
}
