//! `loop_parallel` 配置面与可观测类型的单测（设计 §2）。

use super::*;

#[test]
fn should_default_to_off_with_documented_knobs() {
    let cfg = LoopParallelConfig::default();
    assert_eq!(cfg.mode, LoopParallelMode::Off);
    assert_eq!(cfg.threads, 0, "threads = 0 语义为自动探测");
    assert_eq!(cfg.chunk_size, 256);
    assert_eq!(cfg.min_rows, 256);
}

#[test]
fn should_roundtrip_mode_names() {
    for (mode, name) in [
        (LoopParallelMode::Off, "off"),
        (LoopParallelMode::Safe, "safe"),
    ] {
        assert_eq!(mode.as_str(), name);
        assert_eq!(LoopParallelMode::from_name(name), Some(mode));
    }
    assert_eq!(LoopParallelMode::from_name("unsafe"), None);
    assert_eq!(LoopParallelMode::from_name(""), None);
}

#[test]
fn should_keep_parallel_status_fields_for_audit() {
    let status = ParallelStatus::Parallelized {
        workers: 4,
        rows_dispatched: 100,
        rows_fallback_serial: 0,
        rollback_invalidated: false,
        prewarmed_tables: 2,
        prewarm_ms: 3,
        snapshot_hits: 99,
        snapshot_misses: 1,
        peak_snapshot_bytes: 4096,
    };
    match status {
        ParallelStatus::Parallelized {
            workers,
            rows_dispatched,
            rows_fallback_serial,
            rollback_invalidated,
            prewarmed_tables,
            prewarm_ms,
            snapshot_hits,
            snapshot_misses,
            peak_snapshot_bytes,
        } => {
            assert_eq!(workers, 4);
            assert_eq!(rows_dispatched, 100);
            assert_eq!(rows_fallback_serial, 0);
            assert!(!rollback_invalidated);
            assert_eq!(prewarmed_tables, 2);
            assert_eq!(prewarm_ms, 3);
            assert_eq!(snapshot_hits, 99);
            assert_eq!(snapshot_misses, 1);
            assert_eq!(peak_snapshot_bytes, 4096);
        }
        other => panic!("unexpected status: {other:?}"),
    }
    let skipped = ParallelStatus::Skipped {
        reason: ParallelSkipReason::InsufficientParallelWork,
    };
    assert!(matches!(
        skipped,
        ParallelStatus::Skipped {
            reason: ParallelSkipReason::InsufficientParallelWork
        }
    ));
}

#[test]
fn should_parse_loop_parallel_mode_from_project_toml() {
    // 设计 §2.1：`[engine].loop_parallel = "off" | "safe"`（布尔或规范名，非法值报错）。
    for (value, mode) in [
        ("\"off\"", LoopParallelMode::Off),
        ("\"safe\"", LoopParallelMode::Safe),
        ("true", LoopParallelMode::Safe),
        ("false", LoopParallelMode::Off),
    ] {
        let text = format!("[project]\nname = \"p\"\n[engine]\nloop_parallel = {value}\n");
        let cfg = crate::project::ProjectConfig::from_toml_str(&text, std::path::Path::new("."))
            .unwrap_or_else(|e| panic!("{value}: {}", e.message));
        assert_eq!(cfg.loop_parallel.mode, mode, "{value}");
    }
    // 缺省 = Off 且旋钮取默认值。
    let cfg = crate::project::ProjectConfig::from_toml_str(
        "[project]\nname = \"p\"\n",
        std::path::Path::new("."),
    )
    .unwrap();
    assert_eq!(cfg.loop_parallel, LoopParallelConfig::default());
}

#[test]
fn should_parse_loop_parallel_knobs_from_project_toml_table() {
    // 表形式显式覆盖 threads/chunk_size/min_rows（设计 §2：仅显式给出时覆盖默认）。
    let text = "[project]\nname = \"p\"\n\
                [engine.loop_parallel]\nmode = \"safe\"\nthreads = 4\nchunk_size = 128\nmin_rows = 512\n";
    let cfg = crate::project::ProjectConfig::from_toml_str(text, std::path::Path::new("."))
        .unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(
        cfg.loop_parallel,
        LoopParallelConfig {
            mode: LoopParallelMode::Safe,
            threads: 4,
            chunk_size: 128,
            min_rows: 512,
        }
    );
}

#[test]
fn should_reject_invalid_loop_parallel_mode_in_project_toml() {
    for value in ["\"unsafe\"", "\"on\"", "42"] {
        let text = format!("[project]\nname = \"p\"\n[engine]\nloop_parallel = {value}\n");
        let e = crate::project::ProjectConfig::from_toml_str(&text, std::path::Path::new("."))
            .unwrap_err();
        assert!(
            e.message.contains("loop_parallel"),
            "{value}: {}",
            e.message
        );
    }
}

#[test]
fn should_parse_loop_parallel_flag_in_call_args() {
    // 设计 §2.1：CLI `--loop-parallel <off|safe>`（裸旗标同 --set-oriented 先例取 safe）。
    for (flag, mode) in [
        ("--loop-parallel", LoopParallelMode::Safe),
        ("--loop-parallel=off", LoopParallelMode::Off),
        ("--loop-parallel=safe", LoopParallelMode::Safe),
    ] {
        let argv: Vec<String> = ["call", flag, "p"].iter().map(|s| s.to_string()).collect();
        let cmd =
            crate::cli::args::parse(&argv).unwrap_or_else(|e| panic!("{flag}: {}", e.message));
        let crate::cli::args::Command::Call { loop_parallel, .. } = cmd else {
            panic!("expected Call: {cmd:?}");
        };
        assert_eq!(loop_parallel, Some(mode), "{flag}");
    }
    // 不给旗标 = None（由项目配置决定）。
    let argv: Vec<String> = ["call", "p"].iter().map(|s| s.to_string()).collect();
    let crate::cli::args::Command::Call { loop_parallel, .. } =
        crate::cli::args::parse(&argv).unwrap()
    else {
        panic!("expected Call");
    };
    assert_eq!(loop_parallel, None);
}

#[test]
fn should_reject_unknown_or_repeated_loop_parallel_flags() {
    for argv in [
        vec!["call", "--loop-parallel=unsafe", "p"],
        vec!["call", "--loop-parallel", "--loop-parallel=off", "p"],
    ] {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let e = crate::cli::args::parse(&argv).unwrap_err();
        assert!(
            e.message.contains("--loop-parallel"),
            "{argv:?}: {}",
            e.message
        );
    }
}

#[test]
fn should_cover_all_skip_reasons_from_design() {
    // §4.2 完备枚举：每个变体都必须可构造且可判等（审计面按名输出）。
    let reasons = [
        ParallelSkipReason::NoLoopRegionFound,
        ParallelSkipReason::UnsupportedControlFlow,
        ParallelSkipReason::DynamicCursorSource,
        ParallelSkipReason::CycleCarriedDependence,
        ParallelSkipReason::MaterializedFnSitePresent,
        ParallelSkipReason::UncachedQueryInPrefix,
        ParallelSkipReason::NonDeterministicOrAmbiguousTop1,
        ParallelSkipReason::TransactionAnchoredRead,
        ParallelSkipReason::UnprovenPrefixTotalness,
        ParallelSkipReason::DirtyPrefixWithRollbackRisk,
        ParallelSkipReason::PrewarmFailed,
        ParallelSkipReason::InsufficientParallelWork,
    ];
    for (i, r) in reasons.iter().enumerate() {
        assert_eq!(*r, reasons[i]);
    }
}
