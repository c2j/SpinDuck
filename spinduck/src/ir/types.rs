//! IR 类型定义（自 `ir/mod.rs` 拆分，AGENTS §3；行为一字不改）。

use crate::runtime::cursor::CursorAttrKind;
use crate::value::SpinValue;

/// SQL 绑定源：帧变量 / 记录字段（§3.2 消歧——记录变量限定字段可出现在
/// 下发 SQL 文本，如 `INSERT ... VALUES (r_get_purchase.fee, ...)`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BindSrc {
    /// 裸变量
    Var(VarName),
    /// 记录变量.字段（运行期从帧内 RECORD 取字段值）
    Field { rec: VarName, field: String },
}

impl BindSrc {
    /// 占位符名（`$v_<rec>__<field>` / `$v_<var>`；大小写归一小写）。
    pub fn placeholder(rec_or_var: &str, field: Option<&str>) -> String {
        match field {
            Some(f) => format!(
                "$v_{}__{}",
                rec_or_var.to_ascii_lowercase(),
                f.to_ascii_lowercase()
            ),
            None => format!("$v_{}", rec_or_var.to_ascii_lowercase()),
        }
    }
}

/// 变量名 Newtype（AGENTS §5 边界类型纪律）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct VarName(pub String);

/// 标签 Newtype。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Label(pub String);

/// 赋值目标：变量 / 记录字段（编译期按帧作用域消歧，设计 §3.2）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AssignTarget {
    Var(VarName),
    Field { rec: VarName, field: String },
}

/// 表达式 IR（AST 的可求值子集；SQL 侧表达式经改写后下沉，不出现在此）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum IrExpr {
    Const(SpinValue),
    Var(VarName),
    Field {
        rec: VarName,
        field: String,
    },
    Bin {
        op: BinOp,
        l: Box<IrExpr>,
        r: Box<IrExpr>,
    },
    /// 首个命中分支惰性求值；简单 CASE 的选择子只求值一次。
    Case {
        operand: Option<Box<IrExpr>>,
        whens: Vec<(IrExpr, IrExpr)>,
        else_expr: Option<Box<IrExpr>>,
    },
    Not(Box<IrExpr>),
    IsNull(Box<IrExpr>),
    /// 函数调用：PL 上下文一律解释器直调（M6 接例程表；M2 求值时报 ENG）
    FuncCall {
        name: String,
        args: Vec<IrExpr>,
        /// 目录未命中时使用的内建 SQL；保留执行期分派与报错时机。
        prepared: Result<String, crate::error::EngineError>,
    },
    /// SQL 内建函数（§8.2 [functions] `@` 变换标记）：整体下沉为单条
    /// `SELECT name(args)` 交给内核求值，保留中间类型（日期算术不能经
    /// SpinValue 逐函数委托——TIMESTAMP 会被文本化）
    SqlFn {
        name: String,
        args: Vec<IrExpr>,
        /// 编译期改写结果；错误延迟到该表达式实际执行时报告。
        prepared: Result<ScalarSql, crate::error::EngineError>,
    },
    /// 游标属性：cursor%NOTFOUND 等（§7.3.3）
    CursorAttr {
        cursor: VarName,
        kind: CursorAttrKind,
    },
    /// 函数内查找（§4.3 A7 LookupExpr）：trap 包裹的 SELECT INTO + WHEN OTHERS
    /// 兜底，关系化为计数门控相关子查询（0/>1 行→default，恰 1 行→值含 NULL）。
    /// `query` 为含 `$v_` 占位符的 SELECT 模板；`args` 将占位符映射到实参表达式
    /// （内联后为记录字段/常量）；`default` 为异常兜底表达式。仅集合化批写投影
    /// 渲染（render_scalar），解释器与宏注册路径一律拒绝。
    Lookup {
        query: String,
        args: Vec<(String, IrExpr)>,
        default: Box<IrExpr>,
    },
}

impl IrExpr {
    /// 构造函数调用并预计算目录未命中时的内建 SQL。
    pub fn function_call(name: String, args: Vec<IrExpr>) -> Self {
        let ph: Vec<_> = (0..args.len()).map(|i| format!("$p{i}")).collect();
        let prepared =
            crate::sql::apply_ast_transforms(&format!("SELECT {name}({})", ph.join(", ")));
        Self::FuncCall {
            name,
            args,
            prepared,
        }
    }
}

/// 编译期生成的标量 SQL 与按名绑定源。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScalarSql {
    pub sql: String,
    pub binds: Vec<(String, BindSrc)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

/// 写缓冲策略（V3-6，设计 §3.6）：编译期静态 pass 为每条 `SqlExec` 标注。
/// 默认 `Immediate`（安全失败兜底）——只有纯 `INSERT ... VALUES` 且通过
/// CFG 安全扫描的指令才升级 `Buffered` 进入 `WriteBuffer`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum BufferPolicy {
    /// 即时执行：`take_insert` 先按表 `flush_if_touches` 再返回 false。
    #[default]
    Immediate,
    /// 允许攒批：仅编译期扫描确认安全的纯 `INSERT ... VALUES`。
    Buffered,
}

/// 指令（PC 为编译后索引；标签在代码生成期解析）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum IrStmt {
    Nop,
    Assign {
        target: AssignTarget,
        expr: IrExpr,
    },
    Jump(usize),
    JumpIfNot {
        cond: IrExpr,
        to: usize,
    },
    JumpIf {
        cond: IrExpr,
        to: usize,
    },
    /// `RAISE [msg]`；`message: None` 是裸 `RAISE;`——重抛处理器当前正在处理的错误。
    Raise {
        message: Option<IrExpr>,
    },
    Return {
        value: Option<IrExpr>,
    },
    /// 事务边界标记（§3.5 延迟事务，顶层统一执行）
    TxCommit,
    /// 条件提交：仅当最近一次直发写入影响行数 > 0 才提交（集合化出口专用；
    /// 空游标零行写入不得切换事务快照，设计 §4.5）。编译器不产生此指令。
    TxCommitIfWritten,
    TxRollback,
    /// SELECT [INTO ...]（行数语义：0→NO_DATA_FOUND，≥2→TOO_MANY_ROWS）
    SqlQuery {
        sql: String,
        binds: Vec<(String, BindSrc)>,
        into: Vec<AssignTarget>,
        /// SQL 上下文例程调用下沉计划（§8.3；空 = 无）
        fn_sites: Vec<crate::sql::FnSite>,
    },
    /// 哑表行数检查与折叠赋值共占一步，保持原 SELECT INTO 的预算边界。
    FoldedQuery {
        sql: String,
        target: AssignTarget,
        expr: IrExpr,
    },
    /// DML/DDL（Truncate/Insert/Update/Delete/...）
    SqlExec {
        sql: String,
        binds: Vec<(String, BindSrc)>,
        /// SQL 上下文例程调用下沉计划（§8.3；空 = 无）
        fn_sites: Vec<crate::sql::FnSite>,
        /// 写缓冲策略（V3-6）：编译期 pass 统一标注，旧缓存缺省 Immediate。
        #[serde(default)]
        buffer_policy: BufferPolicy,
    },
    /// EXECUTE IMMEDIATE（动态文本）：运行期求值文本后与动态游标走**同一条**
    /// 解析管线（ogsql 解析 → 规则改名 → 方言变换 → 函数下沉）——统一收口
    /// （决策记录 2025-11-05：撤销 ENG-0062 整体拒绝；解析失败显式报错，
    /// ogsql-parser 不支持的形态向其上游提 issue）
    SqlExecDynamic {
        text_expr: Box<IrExpr>,
        /// INTO 目标（与静态 SELECT INTO 同语义：0 行 NO_DATA_FOUND，>1 行 TOO_MANY）
        into_targets: Vec<AssignTarget>,
    },
    /// PERFORM <expr>：求值并丢弃
    Perform {
        expr: IrExpr,
    },
    /// OPEN 游标（§7.1：静态已改写 / 动态运行期文本）
    OpenCursor {
        cursor: VarName,
        source: CursorSource,
    },
    /// FETCH INTO（位置对位写槽；流尾不覆盖目标）
    Fetch {
        cursor: VarName,
        targets: Vec<AssignTarget>,
        /// true = 隐式游标（FOR rec IN ...）：单目标整体按 schema 建 RECORD
        record: bool,
    },
    CloseCursor {
        cursor: VarName,
    },
    /// 安装异常处理器表（§3.5；handlers 体在 handlers 区，start/end 已标记）
    PushTrap {
        entries: Vec<TrapEntry>,
    },
    /// 撤销当前处理器（正常路径离开 try 体）
    PopTrap,
    /// CALL 例程（§3.4.1）：OUT/INOUT 按 out_targets 位置写回调用方
    CallRoutine {
        name: String,
        args: Vec<IrExpr>,
        /// 与 params 序对应：Some = 写回目标，None = 丢弃
        out_targets: Vec<Option<AssignTarget>>,
    },
}

/// 单个处理器：条件列表 + 主体指令区间 [body_start, body_end)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrapEntry {
    pub conds: Vec<String>,
    pub body_start: usize,
    pub body_end: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum CursorSource {
    /// 静态游标：声明段 parsed_query 经改写管线后的文本与绑定
    Static {
        sql: String,
        binds: Vec<(String, BindSrc)>,
        /// SQL 上下文例程调用下沉计划（§8.3；空 = 无）
        fn_sites: Vec<crate::sql::FnSite>,
    },
    /// 动态 OPEN FOR <变量>（运行期求值文本，M8 前不做人内变量替换）
    Dynamic { query_expr: IrExpr },
}

/// 静态游标声明编译产物：(SQL 文本, 绑定表, SQL 上下文调用下沉计划)。
pub(crate) type CompiledCursorSql = (String, Vec<(String, BindSrc)>, Vec<crate::sql::FnSite>);

/// 例程体内局部 DDL（`CREATE [TEMP] TABLE` / `CREATE TABLE AS`）
/// 编译产物的标记：对应 `code[index]` 的 `SqlExec` 在 Gate 2B
/// stub 库中**执行**（CTAS 追加 `LIMIT 0`），使后续引用获得真实
/// 列检查而非一律延迟。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalDdlStmt {
    /// `code` 中对应 `SqlExec` 的下标。
    pub index: usize,
    /// `true` = CTAS（执行时追加 `LIMIT 0`）；`false` = 普通 CREATE TABLE。
    pub ctas: bool,
    /// 引号形态限定名（`"s"."t"`），Gate 2B 例程收尾 `DROP TABLE IF EXISTS`
    /// 清理用；旧缓存条目缺省为空串（跳过清理，不影响正确性）。
    #[serde(default)]
    pub name: String,
    /// `CREATE TEMP TABLE`：stub 中建 temp 影子表，DROP 只解析影子不动全局。
    #[serde(default)]
    pub temporary: bool,
    /// `IF NOT EXISTS`：命中已有对象时 CREATE 为空操作，不得记入
    /// 可 DROP 名单（防止收尾误删全局 stub 表）。
    #[serde(default)]
    pub if_not_exists: bool,
}

/// 编译产物：线性指令序列。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct IrProg {
    pub code: Vec<IrStmt>,
    /// 已映射的标量声明（小写变量名 → 类型）。`%ROWTYPE` 与映射表外的类型名不在此。
    pub var_types: std::collections::BTreeMap<String, crate::types::SpinType>,
    /// 例程体内局部 DDL 语句标记（按 `code` 下标关联；Gate 2B stub 执行）。
    #[serde(default)]
    pub local_ddl: Vec<LocalDdlStmt>,
}

impl IrProg {
    /// 纯表达式函数体识别（§8.2 路径③）：仅允许 `Nop` + 唯一 `RETURN <expr>`。
    ///
    /// 该形态不含局部变量/分支/副作用，表达式可直接下沉为内核宏；
    /// 任何其它指令都返回 `None`（调用方据此拒绝宏登记，不做近似翻译）。
    pub fn single_return_expr(&self) -> Option<&IrExpr> {
        let mut ret = None;
        for st in &self.code {
            match st {
                IrStmt::Nop => {}
                IrStmt::Return { value: Some(e) } if ret.is_none() => ret = Some(e),
                _ => return None,
            }
        }
        ret
    }
}
