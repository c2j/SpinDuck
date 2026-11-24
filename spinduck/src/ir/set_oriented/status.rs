//! 集合化公开契约类型：模式/臂/拒因/状态/报告与守卫元数据（设计 v3.1 §3/§6）。

/// 集合化语义契约；所有模式均只执行已证明的转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetOrientedMode {
    /// 保持原始解释路径及步数预算语义。
    #[default]
    Off,
    /// 保持提交、值、错误域与异常回滚边界。
    Strict,
    /// 授权合并尾提交及依赖表入口单快照，不承诺崩溃时逐行部分持久化。
    BatchAtomic,
}

impl SetOrientedMode {
    /// 配置及命令行的规范模式名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Strict => "strict",
            Self::BatchAtomic => "batch-atomic",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "strict" => Some(Self::Strict),
            "batch-atomic" => Some(Self::BatchAtomic),
            _ => None,
        }
    }
}

impl From<bool> for SetOrientedMode {
    fn from(enabled: bool) -> Self {
        if enabled {
            Self::Strict
        } else {
            Self::Off
        }
    }
}

/// `call --on-skip[=mode]` / `[engine].set_oriented_on_skip`：
/// 判定行前移时机与 Skipped 是否参与退出码（设计 v3.1 §5 B2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnSkip {
    /// 判定行在区域裁决点即时输出，退出码不受 Skipped 影响。
    Report,
    /// 同 Report，且任一未列入 `--on-skip-error-if` 的 Skipped → 退出码非零。
    Error,
}

impl OnSkip {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Error => "error",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "report" => Some(Self::Report),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// 双臂路由选中的臂（设计 v3.1 §12.3(d)）：`Then` = 判别式命中臂值，
/// `Else` = 主计划（值域补集）。单臂计划恒 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    Then,
    Else,
}

impl Arm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Then => "then",
            Self::Else => "else",
        }
    }
}

/// 未获得集合化证明的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    UnsupportedControlFlow,
    ExceptionScopePresent,
    TransactionBoundaryInsideLoop,
    BatchCommitEnabled,
    LiveOutState,
    UnsupportedWrite,
    TargetReadDependency,
    UnknownObjectDependency,
    UnprovenExpression,
    UnprovenTargetConstraints,
    /// Strict 下受跳行 trap 保护的区域无法证明写入全量：内核报错经
    /// reset_session 撤销前序未提交写入，过滤坏行与原回滚效果不等价。
    UnprovenRollbackScope,
    /// INSERT 表达式无法在 4 层证明中覆盖类型/精度/NULL/异常集等价。
    UnprovenWriteCompleteness,
    /// 省略的目标列含非常量/不确定默认值（序列、时间戳等），批写施加的
    /// 默认与逐行写不可证同值（设计 §4.1 A3）。
    NonDeterministicDefault,
    /// 区域入口 sys_dummy 存在性或恰 1 行不变量被前缀/运行期破坏。
    UnprovenSysDummyInvariant,
    /// 查找/游标查询自身无法证明全定义（非基数运行时错不可排除）。
    UnprovenQueryTotalness,
    /// 有序 Top-1 排序键无 PK/UNIQUE，入口按组并列一致性检查发现
    /// 异值并列（含 NULL 与值并列），首行选择存在真实歧义。
    AmbiguousTop1Tie,
    UnsupportedCall,
    /// 多 OUT 参数被调函数的 LookupTuple 证明失败：处理器漏赋 OUT/返回值、
    /// 处理器 Raise、或成功路径拷贝表达式可抛错（设计 §3.2）。
    MultiOutCallImpure,
    /// 语句残留非空 fn_sites：运行期物化有副作用（distinct_sql/临时表/
    /// MAX_TUPLES），Converted 计划绝不原样穿透（设计 §4.2）。
    UnresolvedFnSite,
    DynamicSql,
    /// 双臂计划（§12 D7）的判别式在运行期取不到唯一非 NULL 值
    /// （0 行 / 多行 / NULL）：无法证选臂，整段退回原 IR 解释。
    RouteDiscriminatorNotTotal,
}

impl SkipReason {
    /// 工件 `reason=` 取值（snake-kebab；设计 v3.1 D6，自 admit 工件起为长期契约）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedControlFlow => "unsupported-control-flow",
            Self::ExceptionScopePresent => "exception-scope-present",
            Self::TransactionBoundaryInsideLoop => "transaction-boundary-inside-loop",
            Self::BatchCommitEnabled => "batch-commit-enabled",
            Self::LiveOutState => "live-out-state",
            Self::UnsupportedWrite => "unsupported-write",
            Self::TargetReadDependency => "target-read-dependency",
            Self::UnknownObjectDependency => "unknown-object-dependency",
            Self::UnprovenExpression => "unproven-expression",
            Self::UnprovenTargetConstraints => "unproven-target-constraints",
            Self::UnprovenRollbackScope => "unproven-rollback-scope",
            Self::UnprovenWriteCompleteness => "unproven-write-completeness",
            Self::NonDeterministicDefault => "non-deterministic-default",
            Self::UnprovenSysDummyInvariant => "unproven-sys-dummy-invariant",
            Self::UnprovenQueryTotalness => "unproven-query-totalness",
            Self::AmbiguousTop1Tie => "ambiguous-top1-tie",
            Self::UnsupportedCall => "unsupported-call",
            Self::MultiOutCallImpure => "multi-out-call-impure",
            Self::UnresolvedFnSite => "unresolved-fn-site",
            Self::DynamicSql => "dynamic-sql",
            Self::RouteDiscriminatorNotTotal => "route-discriminator-not-total",
        }
    }

    /// `as_str` 的反查（`--on-skip-error-if` 名单口径 = 工件 reason 口径）。
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "unsupported-control-flow" => Self::UnsupportedControlFlow,
            "exception-scope-present" => Self::ExceptionScopePresent,
            "transaction-boundary-inside-loop" => Self::TransactionBoundaryInsideLoop,
            "batch-commit-enabled" => Self::BatchCommitEnabled,
            "live-out-state" => Self::LiveOutState,
            "unsupported-write" => Self::UnsupportedWrite,
            "target-read-dependency" => Self::TargetReadDependency,
            "unknown-object-dependency" => Self::UnknownObjectDependency,
            "unproven-expression" => Self::UnprovenExpression,
            "unproven-target-constraints" => Self::UnprovenTargetConstraints,
            "unproven-rollback-scope" => Self::UnprovenRollbackScope,
            "unproven-write-completeness" => Self::UnprovenWriteCompleteness,
            "non-deterministic-default" => Self::NonDeterministicDefault,
            "unproven-sys-dummy-invariant" => Self::UnprovenSysDummyInvariant,
            "unproven-query-totalness" => Self::UnprovenQueryTotalness,
            "ambiguous-top1-tie" => Self::AmbiguousTop1Tie,
            "unsupported-call" => Self::UnsupportedCall,
            "multi-out-call-impure" => Self::MultiOutCallImpure,
            "unresolved-fn-site" => Self::UnresolvedFnSite,
            "dynamic-sql" => Self::DynamicSql,
            "route-discriminator-not-total" => Self::RouteDiscriminatorNotTotal,
            _ => return None,
        })
    }
}

/// 一次调用实际选择的优化状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// `arm`：双臂计划（§12 D7）运行期判别式选中的臂；单臂计划恒 `None`。
    Converted {
        sql: String,
        arm: Option<Arm>,
    },
    Skipped {
        reason: SkipReason,
    },
}

/// 原始例程和独立优化产物的对应报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub routine: String,
    pub status: Status,
}

/// 入口守卫类别（admit 报告元数据，设计 v3.1 §4.1/§6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    /// sys_dummy 单行不变量。
    SysDummy,
    /// 运行期结构漂移复核（逐表）。
    Drift,
    /// 编译期快照一致性复核。
    Snapshot,
    /// Top-1 并列一致性。
    Top1Tie,
    /// IF 路由分支不可达证明。
    Route,
    /// 内联表达式全定义守卫。
    ExprTotalness,
}

impl GuardKind {
    /// 工件 `kind=` 值（snake-kebab，§6 契约）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SysDummy => "sys-dummy",
            Self::Drift => "drift",
            Self::Snapshot => "snapshot",
            Self::Top1Tie => "top1-tie",
            Self::Route => "route",
            Self::ExprTotalness => "expr-totalness",
        }
    }

    /// stdout `guard=` 值（§3.2 逐字规格）。
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::SysDummy => "sys-dummy",
            Self::Drift => "drift",
            Self::Snapshot => "snapshot",
            Self::Top1Tie => "top1",
            Self::Route => "route",
            Self::ExprTotalness => "expr",
        }
    }
}

/// 入口守卫求值面：Static 槽位 admit 可脱离帧全量求值；Frame 槽位
/// 依赖运行帧绑定，无帧上下文时只能 Unchecked。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardScope {
    Static,
    Frame,
}

/// L1 拒因的取证档位（admit F2）：`probe_suspect` = 该拒因可能由
/// 静态快照探测缺口导出（generate 阶段 × `Source::Static`），admit
/// 记 `conservative`；其余一律 firm。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlanRejection {
    pub reason: SkipReason,
    pub probe_suspect: bool,
}

impl PlanRejection {
    pub(crate) fn firm(reason: SkipReason) -> Self {
        Self {
            reason,
            probe_suspect: false,
        }
    }
}
