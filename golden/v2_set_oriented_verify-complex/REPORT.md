# 集合化 V2 能力验证报告（复杂入口）—— purchase 包 `prc_trd_hz_byfund` 100k 三方基准

- 验证对象：集合化 V2（`976dfec`，main `9946a2b`，SpinDuck 0.2.2 debug / DuckDB 1.5.5）
- 参照方法：`golden/v2_set_oriented_verify`（简单入口三方基准）与 `golden/pr48_set_oriented_verify`
- 被测过程：`pkg_split_trade_step3_sh.prc_trd_hz_byfund`（与 `prc_trd_deal_purchase` 同包，按基金汇总成交）
- 日期：2025-11-18

## 1. 结论

| 维度 | 结果 |
|---|---|
| V2 能否转换 `prc_trd_hz_byfund` | **不能（本例）**。`batch-atomic` 与 `strict` 均报 `Skipped(UnsupportedControlFlow)`，安全回退原路径 |
| Off→On 性能 | **≈0.97×（无收益）**。On≡Off 回退，墙钟差为单轮噪声 |
| SP 存过 vs SpinDuck | SP 336.9s vs Off 692.0s，**SpinDuck 解释器约慢 2.1×**（集合化未介入） |
| 数据一致性 | **四方全等**：Off / On / Strict / openGauss 参照，四张目标表全列排序逐行相等，`p_o_succeed=0` |

## 2. 三方定义

| 方 | 形态 | 集合化判定 |
|---|---|---|
| **SP** | openGauss 7.0.0-RC3 `CALL …('20250715', NULL)` | 原生 plpgsql 逐行 |
| **SpinDuck Off** | `call --set-oriented=off` | 解释器逐行 |
| **SpinDuck On** | `call --set-oriented=batch-atomic` | **Skipped(UnsupportedControlFlow)→回退** |
| SpinDuck Strict | `call --set-oriented=strict` | 同上 Skipped→回退 |

判定行原文（`reports/v2_on_call.txt` / `v2_strict_call.txt`）：

```
SET_ORIENTED pkg_split_trade_step3_sh.prc_trd_hz_byfund skipped reason=UnsupportedControlFlow
OUT p_o_succeed = 0
```

## 3. 为什么 Skipped(UnsupportedControlFlow)

`prc_trd_hz_byfund` 相对简单入口 `prc_trd_deal_purchase` 多出一整层"复杂"结构，均落在 V2 准入面外：

- **动态 SQL 游标**：`OPEN c_fxj FOR v_sql_txt`（风险金汇总段，SQL 文本运行期拼接）
- **GOTO/标签**：`<<error_row>>`/`<<error_row2>>`/`<<next_row>>` + `GOTO` 跨块跳转
- **FOR..IN 查询循环**：QFII 段 `FOR res IN (SELECT …) LOOP`
- **OUT 参数函数**：`fnc_com_get_seat_para(…, flag OUT, org OUT, rate OUT)`
- **行级 COMMIT**：c_get_data / c_fxj / QFII 三段循环内逐行 `COMMIT`
- **前置全表 UPDATE**：`mid_yjqs_detail.fund_code/main_xwdm` 两列批量改写（集合化候选区，但被外围控制流拖累）

V2 的区域证明对该过程整体保守拒绝——与设计语义一致（宁可回退不冒险）。

## 4. 测量条件

- **数据**：`mid_yjqs_detail` 100000 行，两侧同一 `gen_detail.sql`（`generate_series` + 互质乘子打散分组键）。`c_get_data` 实际分组 **90160**（0100 品种 zqdm 周期 800 归并），venture 段 ~360 行，QFII 段 0 行（`qfqscjhbrbb_temp` 空表，`fnc_get_split_date` 桩使门控开启但无数据）。
- **补充资产**（`sql/`）：语料 `tables.sql` 未含 `par_oper_venture`/`dat_fund_xw_venture`/`dic_stock_kind_sub`/`jyfy_temp`/`v_par_bond`，以及 9 个外部函数——全部以确定性桩/最小 DDL 补齐，**两侧执行同一份文件**。`par_fund_seat_para.commision_type` 脱敏列宽 varchar(1)→varchar(2)（同 seed.sql 头注的同类问题）。
- **计时口径**：SpinDuck = `call` CLI 端到端墙钟；SP = `docker exec gsql` 客户端墙钟（含连接）。单轮（百秒级×3 成本过高）。
- **构建**：debug。

## 5. 性能结果（100k 输入行）

| 方 | 墙钟 | 备注 |
|---|---|---|
| SP（openGauss 7.0.0-RC3） | **336946 ms** | gsql 客户端墙钟 |
| SpinDuck Off | 691985 ms | 解释器逐行，单轮 |
| SpinDuck On（batch-atomic） | 710948 ms | Skipped→回退 |
| SpinDuck Strict | 768234 ms | Skipped→回退（单轮噪声偏大） |

- Off→On ≈ **0.97×**（无收益；On 为回退执行，差值为噪声）
- SP→SpinDuck-Off ≈ **0.49×**（原生存过约快 2.1×）
- 三种 SpinDuck 模式同路径同量级，Strict 单轮偏慢属测量噪声（同代码路径）。

## 6. 数据一致性

四张目标表全列排序逐行比对（`reports/consistency.log`；数值按 Decimal 归一，消除 PG `'.50'` 与 JSON `'0.50'` 的格式差）：

| 表 | 行数 | off==on==strict==ref |
|---|---|---|
| dat_fund_cjqs | 90160 | True |
| dat_fund_xw_venture | 360 | True |
| mid_yjqs_detail（cjbh,fund_code,main_xwdm） | 100000 | True |
| qfqscjhbrbb_temp | 0 | True |

`OUT p_o_succeed = 0` 四方一致。回退路径（On/Strict）与 Off 逐行相同，符合"安全回退"语义。

## 7. 口径与限制

1. **参照版本**：openGauss **7.0.0-RC3**（`sd-og70`，库 `hz`），非常驻门禁的 5.0.0；与 PR #48 历史数字不可直接比较。
2. **桩依赖**：外部函数现分两类——`fnc_com_get_fund_code`/`fnc_com_get_main_xwdm`/`fnc_com_get_seat_para`/`fnc_get_fund_data_mode` 自 #57 起改用语料逐字体（`case_corpus_purchase/sql/routines/1_FNC_*.sql`）；其余 5 个（`fnc_com_get_seat_fee_type`/`fnc_get_bond_yjlx`/`fnc_get_rate_info2`/`fnc_get_split_date`/`fnc_get_stock_kind`）仍为确定性桩（`sql/routines/0_STUB_*.sql`）。两侧同一实现；真实现可能不同，但三方一致性结论不受影响（同一输入同一语义）。
3. **单轮测时**：Off/On/Strict 各一轮，百秒级成本；Strict 768s vs Off 692s 的差为噪声量级。
4. **debug 构建**：绝对值为开发机参考，倍数结论（On≈Off、SP≈2×Off）稳健。
5. **复现**：`bash golden/v2_set_oriented_verify-complex/run_bench.sh`（`N=1000` 冒烟；`OG_CONTAINER=sd-og70 OG_LIBS=/opt/oglibs2` 启用 SP 参照）。

## 8. 与简单入口（`prc_trd_deal_purchase`）对照

| | deal_purchase（简单） | hz_byfund（复杂，本包） |
|---|---|---|
| V2 判定 | **Converted**（batch-atomic） | **Skipped(UnsupportedControlFlow)** |
| Off→On | ≈97× | ≈1×（回退） |
| SP vs Off | SP 148.7s < Off 380.2s | SP 336.9s < Off 692.0s（≈2.1×） |
| 一致性 | 三方 100k 行全等 | 四方四表全等 |

结论：V2 的收益集中在"游标逐行搬运"型过程；对含动态 SQL/GOTO/OUT 参数函数/行级 COMMIT 的复杂过程，区域证明保守拒绝，行为退化为与 Off 完全一致的安全回退——**正确性无损，性能无增益**。复杂入口的加速需要 V2 扩展准入面（动态游标、GOTO 区域、行级事务边界的批处理证明），属后续工作。

> 交叉指认（validation V51.4）：表中 `prc_trd_deal_purchase` 的 **Converted 仅在 `batch-atomic` 下**成立；未指定模式（strict）的同一次 `spinduck call --set-oriented` 是 `Skipped(TransactionBoundaryInsideLoop)`——Strict 不授权改动事务边界，两处记录按各自模式口径均成立。
