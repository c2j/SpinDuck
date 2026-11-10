//! CLI 契约层（AGENTS §4.1）：逐子命令锁定**真实退出码 + stdout 文本**。
//!
//! 与 `src/cli/run.rs` 单测的分工：那边测 `run()` 纯函数的返回值与写入内容，本层
//! 起**真子进程**跑 `[[bin]] spinduck`（`required-features = ["cli"]`，与发版矩阵
//! 同一 feature 口径）。只有这里能证明的两件事：进程观察到的退出码，以及
//! "全部输出走 stdout、stderr 恒空"这条渲染纪律。
//!
//! 期望值均在 `--features cli` 构建的二进制上实测；实证登记见 docs/validation.md
//! V27.1（`call` 三形态）、V27.4（编码三入口）、V27.5（`verify`）、V28（本层落地）、
//! V29（D9① 执行面判红：`call`/`explain` 两条期望值随行为同批改）、V30（`plan` 闭包与
//! 计划落盘：计划面契约）、V31（`deploy` 契约）、
//! V32（D5 缺件门扩到 `call` 两条入口 + `call --plan`：形态 C/A/B 三条期望值同批改判）、
//! V33（D9② 真 GBK 解码：编码三条期望值再改判 + 判据实验"同源码仅编码不同 → 同结果"）、
//! V34（A2-5 表达式级被调函数进闭包：`plan` 与 `call` 各新增缺件契约，无既有期望值改判）、
//! V35（A5 本地半边：新增 `query`/`compare` 两组契约，无既有期望值改判）、
//! V36（与主干 V26 性能轮合并：`--batch-commit`/`--step-budget` 三条二进制契约折进
//! `call` 子模块，并新增"计划面也吃下执行开关"的组合契约）、
//! V37（审核应修：`compare --key` 的键列两侧都校验——参照侧改名/换位两条契约）。
//!
//! 按职责分子模块（AGENTS §3：单文件 ≤500 行），本文件只放共享夹具与接缝：
//! `usage`（版本/用法/输出通道）、`compile`、`rules_explain`、`call`（含编码两代判定与
//! 三形态）、`verify`、`plan`（运行计划落盘与缺件红）、`deploy`（按计划建落盘库、
//! 缺件即拒）、`query`（一条 SQL → 结果快照落盘）、`compare`（两份快照逐值裁决）、
//! `gauss_boundary`（`materialize`/`sync` 的门禁）。
//!
//! 零网络与实例依赖：每个用例自带临时目录，且子进程环境里所有 `SPINDUCK_*` 变量一律
//! 剥离——否则 `materialize`/`sync` 的契约会从"报错"变成"真的去连实例"。唯一的库级
//! 引用是 `encoding_rs`（GBK 夹具的字节由它生成，不硬抄字节），它已是 crate 的直接依赖。
//! 本层不写 `#![cfg(feature = "cli")]`：feature 没开时 `env!` 展开即编译红，
//! 比"收集 0 条测试"诚实（AGENTS §4.1 禁跳过即绿）。

use std::path::{Path, PathBuf};

// crate 根的 `mod` 默认在同目录查找，这里显式指向 `tests/cli/` 下的职责子模块。
#[path = "cli/call.rs"]
mod call;
#[path = "cli/compare.rs"]
mod compare;
#[path = "cli/compile.rs"]
mod compile;
#[path = "cli/deploy.rs"]
mod deploy;
#[path = "cli/gauss_boundary.rs"]
mod gauss_boundary;
#[path = "cli/plan.rs"]
mod plan;
#[path = "cli/query.rs"]
mod query;
#[path = "cli/rules_explain.rs"]
mod rules_explain;
#[path = "cli/usage.rs"]
mod usage;
#[path = "cli/verify.rs"]
mod verify;

pub(crate) const EXIT_OK: i32 = 0;
pub(crate) const EXIT_FAIL: i32 = 1;
pub(crate) const EXIT_USAGE: i32 = 2;

// ------------------------------------------------------------ 共享夹具

/// 单条赋值的过程体（`explain` 与 `call` 共用同一份源码，锁同一 IR 口径）。
pub(crate) const P_X: &str = "CREATE OR REPLACE PROCEDURE prc_x(p_i IN NUMBER, p_o OUT NUMBER) IS BEGIN p_o := p_i * 2; END;";
/// 无可观察输出的过程（`call` 的 `CALL <routine> OK` 分支）。
pub(crate) const P_NULL: &str = "CREATE OR REPLACE PROCEDURE p_null IS BEGIN NULL; END;";
/// compile 的 OK 面两个体。
pub(crate) const P_CLEAN_A: &str = "CREATE OR REPLACE PROCEDURE p_a IS BEGIN NULL; END;";
pub(crate) const P_CLEAN_B: &str = "CREATE OR REPLACE PROCEDURE p_b IS BEGIN NULL; END;";
/// FORALL 是 M2 编译器显式不支持的语句 → ENG-0022（compile 的 FAIL 面）。
pub(crate) const P_FORALL: &str =
    "CREATE OR REPLACE PROCEDURE p_b IS BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;";
/// 与 [`write_gbk_routine`] 成对的 UTF-8 写法（编码对照的另一半）。
pub(crate) const P_LITERAL_UTF8: &str =
    "CREATE OR REPLACE PROCEDURE prc_gbk(p_o OUT VARCHAR2) IS BEGIN p_o := '中文'; END;";
/// `verify` 的项目声明：入口须包限定（`src/project/entry.rs` 要求名字含 `.`）。
pub(crate) const PROJECT_X: &str =
    "[project]\nname = \"v\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_a.prc_x\"\n";
pub(crate) const PROJECT_GBK: &str =
    "[project]\nname = \"v\"\nkernel = \"1.5.5\"\n\n[[entry]]\nroutine = \"pkg_a.prc_gbk\"\n";
pub(crate) const PACKAGE_X: &str = "CREATE OR REPLACE PACKAGE pkg_a AS\n  PROCEDURE prc_x(p_o OUT NUMBER);\nEND;\n/\n\
     CREATE OR REPLACE PACKAGE BODY pkg_a AS\n  PROCEDURE prc_x(p_o OUT NUMBER) IS\n  BEGIN\n    p_o := 1;\n  END;\nEND;\n/\n";
/// 规则探针的两种版本锚定。
pub(crate) const RULES_MATCH: &str = "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.5.5\"\n";
pub(crate) const RULES_DRIFT: &str = "target_duckdb = \"1.5.5\"\nprobe_duckdb = \"1.4.4\"\n";

/// "中文"的 GBK 字节（`0xD6 0xD0 0xCE 0xC4`）——非 UTF-8，编码契约的输入形态。
pub(crate) const GBK_ZHONGWEN: [u8; 4] = [0xD6, 0xD0, 0xCE, 0xC4];

/// `call` 三形态共用的主角、实参与资产相对路径（V27.1 同一口径）。
pub(crate) const PURCHASE_ROUTINE: &str = "FNC_GET_PURCHASE_JS_DAYS";
pub(crate) const PURCHASE_DATE: &str = "'20250715'";
pub(crate) const PURCHASE_ID: &str = "'0130000001'";
pub(crate) const PURCHASE_MAIN: &str = "sql/routines/2_FNC_GET_PURCHASE_JS_DAYS.sql";
pub(crate) const PURCHASE_GETDAY: &str = "sql/routines/0_SIMPLIFIED_fnc_com_getday.sql";
pub(crate) const PURCHASE_IF_DEAL: &str = "sql/routines/0_SIMPLIFIED_fnc_com_if_deal_date.sql";
pub(crate) const PURCHASE_DDL: &str = "sql/ddl/tables.sql";
pub(crate) const PURCHASE_SEED: &str = "cases/datasets/seed.sql";

// ------------------------------------------------------------ 接缝

/// 黄金语料项目的绝对路径（只读：本层任何用例都不得改动黄金资产）。
pub(crate) fn golden_project(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate 上级即仓库根")
        .join("golden")
        .join(name)
}

/// 黄金项目内某资产的绝对路径文本（直接喂给 `--file`）。
pub(crate) fn purchase_file(rel: &str) -> String {
    golden_project("case_corpus_purchase")
        .join(rel)
        .to_string_lossy()
        .into_owned()
}

/// 起子进程跑 CLI，返回 `(退出码, stdout, stderr)`。
pub(crate) fn cli(args: &[&str]) -> (i32, String, String) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_spinduck"));
    cmd.args(args);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("SPINDUCK_")) {
        cmd.env_remove(key);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("无法启动被测二进制 {e}"));
    (
        out.status.code().expect("子进程被信号终止，不是正常退出码"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 写夹具文件（自动建父目录），返回其路径。
pub(crate) fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().expect("rel 至少含文件名")).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

/// 写出含 GBK 中文字面量的例程文件（V27.4 的输入形态）。
pub(crate) fn write_gbk_routine(dir: &Path, rel: &str) -> PathBuf {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(
        b"CREATE OR REPLACE PROCEDURE prc_gbk(p_o OUT VARCHAR2) IS BEGIN p_o := '",
    );
    bytes.extend_from_slice(&GBK_ZHONGWEN);
    bytes.extend_from_slice(b"'; END;\n");
    write_bytes(dir, rel, &bytes)
}

/// GBK 包夹具的声明段与结尾段：关键字、包名、例程名全 ASCII，只有字面量是 GBK 字节
/// （交付件 §4 的真实形态）。与 [`PACKAGE_X`] 只差编码，用于锁定"核验面吃下 GBK"。
pub(crate) const PACKAGE_GBK_HEAD: &str = "CREATE OR REPLACE PACKAGE pkg_a AS\n  PROCEDURE prc_gbk(p_o OUT VARCHAR2);\nEND;\n/\n\
     CREATE OR REPLACE PACKAGE BODY pkg_a AS\n  PROCEDURE prc_gbk(p_o OUT VARCHAR2) IS\n  BEGIN\n    p_o := '";
pub(crate) const PACKAGE_GBK_TAIL: &str = "';\n  END;\nEND;\n/\n";

/// 写出 GBK 包体（[`PACKAGE_GBK_HEAD`] + GBK `中文` + [`PACKAGE_GBK_TAIL`]）。
pub(crate) fn write_gbk_package(dir: &Path, rel: &str) -> PathBuf {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PACKAGE_GBK_HEAD.as_bytes());
    bytes.extend_from_slice(&GBK_ZHONGWEN);
    bytes.extend_from_slice(PACKAGE_GBK_TAIL.as_bytes());
    write_bytes(dir, rel, &bytes)
}

/// 按字节写夹具文件（自动建父目录），返回其路径——编码契约的输入必须逐字节可控。
pub(crate) fn write_bytes(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().expect("rel 至少含文件名")).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p
}

/// 编码对照判据的字面量（design §3.5 的 A0a 实验主角：交付件里 `WHERE status = '已成交'`
/// 这一类形态）。两份源码只有它的字节表示不同。
pub(crate) const FLAG_LITERAL: &str = "已成交";

/// 写出 `fnc_flag`：实参等于 `'已成交'` 给 `MATCH`，否则 `NO_MATCH`。
/// `gbk = true` 时字面量按 GBK 编码落盘，其余字节与 UTF-8 版逐字相同。
pub(crate) fn write_flag_routine(dir: &Path, rel: &str, gbk: bool) -> PathBuf {
    let head = b"CREATE OR REPLACE FUNCTION fnc_flag(p_i_flag IN VARCHAR2) RETURN VARCHAR2 IS \
                 BEGIN IF p_i_flag = '";
    let tail = b"' THEN RETURN 'MATCH'; ELSE RETURN 'NO_MATCH'; END IF; END;\n";
    let encoded = encoding_rs::GBK.encode(FLAG_LITERAL).0;
    let literal: &[u8] = if gbk {
        encoded.as_ref()
    } else {
        FLAG_LITERAL.as_bytes()
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(head);
    bytes.extend_from_slice(literal);
    bytes.extend_from_slice(tail);
    write_bytes(dir, rel, &bytes)
}
