//! 运行态项目清单（§11.1 `conf/`）：研发态 `spinduck.toml` 的归一化快照。
//!
//! 决策：研发态文件仍是唯一手写入口；`compile --sync-manifest` 把"运行态所需的那部分声明"
//! （项目名/内核版本 + 可调用面 `[[entry]]` + 资产绝对路径）落到
//! `<home>/conf/spinduck.runtime.toml`。运行态目录因此自带可加载的项目声明
//! （`ProjectConfig::load(home)` 直接可用），且快照带来源标记，漂移可由
//! 下一次同步重写并显式报告（单一写入者 = `compile --sync-manifest`，禁止手改）。
//! 旧名 `conf/spinduck.toml` 不再被读取；同步成功后删除（issue #38 决策 1）。

use crate::error::EngineError;
use crate::project::{CaseKind, ProjectConfig};
use std::path::PathBuf;

/// 运行态快照文件名（`<home>/conf/` 下）。与手写真源 `spinduck.toml` 区分阶段。
pub const SNAPSHOT_FILE: &str = "spinduck.runtime.toml";

/// 一次同步的结果（供 CLI 渲染）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestSync {
    pub path: PathBuf,
    /// 与既有快照内容不同（含首次创建）
    pub changed: bool,
    /// 写新名成功后删除的旧名快照路径（`conf/spinduck.toml`）。
    pub removed_legacy: Option<PathBuf>,
}

/// 生成快照文本（唯一序列化口径：由 [`sync_runtime_manifest`] 落盘）。
pub fn manifest_text(project: &ProjectConfig) -> Result<String, EngineError> {
    let mut doc = toml::value::Table::new();
    let mut prj = toml::value::Table::new();
    prj.insert("name".into(), project.name.clone().into());
    prj.insert("kernel".into(), project.kernel.clone().into());
    doc.insert("project".into(), prj.into());

    let mut rt = toml::value::Table::new();
    rt.insert("source".into(), path_text(&project.root).into());
    rt.insert(
        "generated_by".into(),
        format!("spinduck {}", crate::version()).into(),
    );
    rt.insert(
        "note".into(),
        "运行态快照：由 `spinduck compile --sync-manifest` 生成，禁止手改（唯一真源 = 项目根 spinduck.toml）".into(),
    );
    doc.insert("runtime".into(), rt.into());

    let mut paths = toml::value::Table::new();
    paths.insert("ddl".into(), path_text(&project.ddl_dir).into());
    paths.insert("routines".into(), path_text(&project.routines_dir).into());
    paths.insert("stubs".into(), toml_list(&project.stubs_dirs));
    paths.insert("rules".into(), toml_list(&project.rules_files));
    paths.insert("probes".into(), path_text(&project.probes_dir).into());
    paths.insert("datasets".into(), path_text(&project.datasets_dir).into());
    paths.insert("baselines".into(), path_text(&project.baselines_dir).into());
    paths.insert("home".into(), path_text(&project.home_dir).into());
    doc.insert("paths".into(), paths.into());

    let mut cases = toml::value::Table::new();
    cases.insert(
        "include".into(),
        toml::Value::Array(
            project
                .case_includes
                .iter()
                .map(|p| toml::Value::String(project.root.join(p).display().to_string()))
                .collect(),
        ),
    );
    doc.insert("cases".into(), cases.into());

    doc.insert(
        "entry".into(),
        toml::Value::Array(
            project
                .entries
                .iter()
                .map(|e| {
                    let mut t = toml::value::Table::new();
                    t.insert("routine".into(), e.routine.clone().into());
                    t.insert(
                        "kind".into(),
                        match e.kind {
                            CaseKind::Function => "function",
                            CaseKind::Procedure => "procedure",
                        }
                        .into(),
                    );
                    toml::Value::Table(t)
                })
                .collect(),
        ),
    );

    toml::to_string_pretty(&toml::Value::Table(doc))
        .map_err(|e| EngineError::env(30, format!("运行态清单序列化失败: {e}")))
}

/// 写入/刷新运行态快照（单一写入者 = `compile --sync-manifest`）。
///
/// 写新名成功后删除旧名 `conf/spinduck.toml`（生成物，禁止手改）：
/// 同一目录不长期并存两份快照。
pub fn sync_runtime_manifest(project: &ProjectConfig) -> Result<ManifestSync, EngineError> {
    let text = manifest_text(project)?;
    let conf = project.home_dir.join("conf");
    let path = conf.join(SNAPSHOT_FILE);
    std::fs::create_dir_all(&conf)
        .map_err(|e| EngineError::env(30, format!("创建 {}: {e}", conf.display())))?;
    let previous = std::fs::read_to_string(&path).unwrap_or_default();
    let changed = previous != text;
    if changed {
        std::fs::write(&path, &text)
            .map_err(|e| EngineError::env(30, format!("写 {}: {e}", path.display())))?;
    }
    let legacy = conf.join("spinduck.toml");
    let removed_legacy = if legacy.is_file() {
        std::fs::remove_file(&legacy)
            .map_err(|e| EngineError::env(30, format!("删除 {}: {e}", legacy.display())))?;
        Some(legacy)
    } else {
        None
    };
    Ok(ManifestSync {
        path,
        changed,
        removed_legacy,
    })
}

fn path_text(p: &std::path::Path) -> String {
    p.display().to_string()
}

fn toml_list(items: &[std::path::PathBuf]) -> toml::Value {
    toml::Value::Array(
        items
            .iter()
            .map(|p| toml::Value::String(p.display().to_string()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectConfig;

    const DEV: &str = r#"
[project]
name = "exam"
kernel = "1.5.5"

[paths]
ddl = "sql/ddl"
routines = "sql/routines"
stubs = ["sql/stubs"]
rules = ["rules/spinduck-rules.toml"]
home = ".spinduck"

[cases]
include = ["cases/*.case.toml"]

[[entry]]
routine = "pkg_split_trade_step2.prc_pre_deal_shjycf"

[[entry]]
routine = "PKG_QQ_SPLIT.PRC_GENERAL_DEAL_INST_SH_BUY"
kind = "function"
"#;

    #[test]
    fn should_snapshot_entries_and_asset_paths_into_home_conf() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("spinduck.toml"), DEV).unwrap();
        let project = ProjectConfig::load(root).unwrap();
        let sync = sync_runtime_manifest(&project).unwrap();

        assert_eq!(sync.path, root.join(".spinduck/conf/spinduck.runtime.toml"));
        assert!(sync.changed, "首次写入应报告变化");
        let snap = std::fs::read_to_string(&sync.path).unwrap();
        assert!(snap.contains("generated_by"), "快照需标注生成者：{snap}");
        assert!(
            snap.contains("spinduck compile --sync-manifest"),
            "快照 note 须指向新的生成入口：{snap}"
        );
        assert!(
            snap.contains(&format!("source = \"{}\"", root.display())),
            "快照需带来源根（漂移可判读）：{snap}"
        );

        // 运行态目录可独立加载，且入口面一致
        let runtime = ProjectConfig::load(&root.join(".spinduck")).unwrap();
        assert_eq!(runtime.name, "exam");
        assert_eq!(runtime.kernel, "1.5.5");
        assert_eq!(
            runtime
                .entries
                .iter()
                .map(|e| (e.routine.clone(), e.kind))
                .collect::<Vec<_>>(),
            vec![
                (
                    "pkg_split_trade_step2.prc_pre_deal_shjycf".to_string(),
                    CaseKind::Procedure
                ),
                (
                    "pkg_qq_split.prc_general_deal_inst_sh_buy".to_string(),
                    CaseKind::Function
                ),
            ],
            "运行态快照必须完整承载可调用面"
        );
        assert_eq!(
            runtime.ddl_dir,
            root.join("sql/ddl"),
            "资产路径按绝对口径固化"
        );
        assert_eq!(
            runtime.case_includes,
            vec![root.join("cases/*.case.toml").display().to_string()],
            "用例通配同样绝对化：运行态目录不再依赖相对基准"
        );
        assert_eq!(
            runtime.home_dir, project.home_dir,
            "快照的 home 与研发态指向同一运行态目录（重复同步幂等）"
        );

        let again = sync_runtime_manifest(&project).unwrap();
        assert!(!again.changed, "同内容重复同步不得报告变化");
    }

    #[test]
    fn should_report_change_when_dev_entries_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("spinduck.toml"), DEV).unwrap();
        let first = sync_runtime_manifest(&ProjectConfig::load(root).unwrap()).unwrap();
        assert!(first.changed);

        let drifted = DEV.replace(
            "[[entry]]\nroutine = \"pkg_split_trade_step2.prc_pre_deal_shjycf\"\n",
            "[[entry]]\nroutine = \"pkg_split_trade_step3_sh.prc_trd_hz_byfund\"\n",
        );
        std::fs::write(root.join("spinduck.toml"), drifted).unwrap();
        let second = sync_runtime_manifest(&ProjectConfig::load(root).unwrap()).unwrap();
        assert!(second.changed, "入口漂移必须重写并报告");
        let runtime = ProjectConfig::load(&root.join(".spinduck")).unwrap();
        assert_eq!(
            runtime.entries[0].routine,
            "pkg_split_trade_step3_sh.prc_trd_hz_byfund"
        );
    }

    #[test]
    fn should_remove_legacy_snapshot_after_writing_new_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("spinduck.toml"), DEV).unwrap();
        let legacy = root.join(".spinduck/conf/spinduck.toml");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "# 旧名快照（生成物）\n").unwrap();

        let sync = sync_runtime_manifest(&ProjectConfig::load(root).unwrap()).unwrap();
        assert_eq!(sync.removed_legacy, Some(legacy.clone()));
        assert!(!legacy.exists(), "写新名成功后旧名快照必须删除");
        assert!(sync.path.is_file());

        let again = sync_runtime_manifest(&ProjectConfig::load(root).unwrap()).unwrap();
        assert_eq!(again.removed_legacy, None, "第二次同步已无旧名可删");
    }
}
