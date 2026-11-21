# 集合化 V3 验收

规格：`docs/spinduck-set-oriented-v3-design.md`（`feat/set-oriented-v3`）。

owner: feat-set-oriented-v3

## 终态

`prc_trd_hz_byfund` 的区域 0 可以在 `BatchAtomic` 下集合化执行，后缀仍按原 IR 解释。M1 先保住 `--batch-commit` 下 `QFII` / `c_fxj` 的写缓冲语义。M2 通过 §5.1 的 10 个单元测试，并在 100k 输入上与 openGauss 四张目标表逐行全等。合并、版本号和 tag 不在本目标内。

## 行

同一个 owner 拥有全部 6 行。快速 checker 是 `scripts/goals/set-oriented-v3.sh`。它不生成剖面，也不跑 100k。

| 行 | 完成标准 | checker 怎么判定 |
|---|---|---|
| 1 | 设计文档已在 `HEAD` | `git cat-file -e HEAD:docs/spinduck-set-oriented-v3-design.md` |
| 2 | M0 基线剖面已落盘 | `docs/goals/artifacts/set-oriented-v3-m0-profile.json` 通过下面的 schema，且 `git_head` 是当前 `HEAD` 的祖先。剖面是基线，之后的代码改动不使它失效 |
| 3 | M1：测试 8 通过 | `should_enforce_write_buffer_immediate_policy_for_qfii_cross_table_and_fxj_close` 已定义且 `cargo test --features cli <name> -- --exact` 为 1 passed |
| 4 | M2 单元：其余 9 个测试通过 | 下表 9 个名字同样已定义且各自 exact 通过 |
| 5 | 100k 黄金对账通过 | `docs/goals/artifacts/set-oriented-v3-golden.json` 通过 schema。`git_head == HEAD`，或它是祖先且此后没有改过集合化实现路径。实现路径有改动则这一行重新变成缺口 |
| 6 | 该 `HEAD` 上 ci 的五个 job 全绿 | 恰好一个 open PR，`headRefOid` 等于本地 `HEAD`，job `format`、`clippy`、`test`、`audit`、`golden` 的 conclusion 都是 `SUCCESS` |

第 4 行的 9 个测试：

- `should_drive_multiple_regions_with_run_span_and_preserve_suffix_absolute_gotos`
- `should_reject_outer_and_inner_when_static_cursors_are_nested`
- `should_keep_prc_trd_deal_purchase_single_region_conversion_unchanged`
- `should_inline_multi_out_lookup_tuple_and_reject_unregistered_or_partial_out`
- `should_lower_jyfy_aggregate_with_exact_four_case_semantics`
- `should_fallback_when_v_par_bond_has_zero_overlapping_or_query_error`
- `should_reject_sys_dummy_block_when_not_total_and_verify_decimal_mul_sub`
- `should_require_zero_bad_rows_when_inst_log_is_non_noop`
- `should_convert_region0_of_byfund_and_match_off_on_small_corpus`

函数还没写出来时，checker 只记缺口，不调用 cargo。函数都在之后才跑测试，整次 cargo 预算 90 秒。超时或 cargo 本身失败而没有断言失败，算无法判定，退出码 2，不记成缺口。

### M0 产物

`docs/goals/artifacts/set-oriented-v3-m0-profile.json`：

```json
{
  "routine": "prc_trd_hz_byfund",
  "git_head": "<测量时的完整 SHA>",
  "builds": {
    "debug": {"prefix_ms": 0, "c_get_data_ms": 0, "c_fxj_ms": 0, "qfii_ms": 0},
    "release": {"prefix_ms": 0, "c_get_data_ms": 0, "c_fxj_ms": 0, "qfii_ms": 0}
  }
}
```

四个时间是大于等于 0 的数，单位毫秒。debug 与 release 各一组。

### 黄金产物

`docs/goals/artifacts/set-oriented-v3-golden.json`：

```json
{
  "git_head": "<对账时的完整 SHA>",
  "input_rows": 100000,
  "p_o_succeed": 0,
  "off_matches_gauss": true,
  "batch_atomic_matches_gauss": true
}
```

使第 5 行失效的路径：`spinduck/src/ir/set_oriented/`、`spinduck/src/engine/set_oriented.rs`、`spinduck/src/ir/types.rs`、`spinduck/src/runtime/interp/writebuf.rs`、`spinduck/src/runtime/interp.rs`。

## 边界

实现者是 workmux 窗口 `feat-set-oriented-v3`（Devin）。它不是 amux worker，不能自设 `AMUX_GOAL_CHECK`。owner 写明之后，`SCHED-3` 在 `done` 连续 6 小时不升或比 6 小时前少时退出 1。

审阅由当前 Grok 会话做，用中文，不改产品代码。OpenCode 留作替补，不另开窗口。

当前阶段只做第 2 行（M0）。M1 及以后另发 brief。

实现者可以在 `feat/set-oriented-v3` 上按 TDD 改代码、提交、开 PR。碰到下面任何一条就停，等仓库主人决定：

- `SqlExecutor` 或 `TraceSink` 的签名
- 新依赖、新 feature、新 bin
- 改人类已有测试的断言、fixture 或 golden 期望
- 版本号、tag、把 codeweb 改成 path override
- 规则 `[rejects]` 新增项

一次循环只锁一个行为。未指定 owner 之前，不向任何 workmux 窗口派实现 brief。

## 新鲜度与度量

快速 checker 退出码：0 六行都满足，1 有真实缺口，2 有一行无法判定。标准输出一行 JSON：`{"done":N,"total":6,"summary":"..."}`。

`scripts/goals/set-oriented-v3-measure.sh` 由 amux `SCHED-3` 每小时跑一次，把一行 JSON 追加到 `~/.amux/logs/spinduck-set-oriented-v3-goal.log`。checker 无法判定时度量脚本退出 2；`done` 连续 6 小时没有上升，或比 6 小时前少，则退出 1。

M3（标量求值与 Appender）等第 2 行的 release 数字出来再开，不在这 6 行里。

不给实现者另设 amux `/goal`。不设会唤醒模型的协调日程。Board 保持关闭。
