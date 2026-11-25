# V4 验收分支覆盖矩阵

> 口径：Off vs Safe 差分必须覆盖例程内**全部可由数据触发的分支**——
> 含异常/trap 路径、查找命中/未命中、条件分支、基数错误、early exit。
> 当前种子（`cases/datasets*/seed.sql`）只覆盖 happy path 主干，
> 本矩阵标注缺口；`cases/datasets_branch/` 下两个自含种子补齐数据：
> `seed_branch_purchase.sql`（purchase 专用维度+行）、
> `seed_branch_byfund.sql`（byfund 全段维度+行）。

## purchase `prc_trd_deal_purchase`（游标 `c_get_purchase`，行级分支）

| # | 分支 | 触发条件 | 现状 | 补齐方式 |
|---|---|---|---|---|
| P0 | 游标行被排除 | 同左 | **已覆盖**（5 行排除样本全形态） | — |
| P1 | `par_fund_partner` 查找 | 同左 | **已覆盖**（HIT + A9999 未维护 MISS） | — |
| P2 | `par_sys_purchase` 查找 | 同左 | **已覆盖**（HIT + 0139999999 MISS + 0130000003 NULL days） | — |
| P3 | `v_tradedate_flag=1` | 同左 | **部分覆盖**（market_code '001'/'002' 均有行，flag 取值由函数内部决定） | — |
| P4 | `v_kind='1'` 分支 | seat_para kind_id='1' → `fnc_com_get_seat_para`+CASE 18 臂 | **已覆盖**（`v4_purchase_branch_kind1_equiv`：测试内 UPDATE `swh_all_kind` 切 '1'） | — |
| P5 | `v_kind≠'1'` ELSE 分支 | `par_fund_seat_para` rownum=1 子查询 → `commision_fee_type` CASE + ELSE→yj05；seat_code 回退 | **已覆盖**（seed 默认 '0'：'2'/'4'、'3'/'1'、'3'/'6' 组合 + '000000' 回退行） | 双字符臂物理不可达见文末注 |
| P6 | INSERT 异常 | `dat_trd_repurchase` 写失败→inst_log+`GOTO error_row` | 未覆盖 | 难以确定性触发（无唯一约束/类型均合法），记录为已知缺口 |
| P7 | 前置 DELETE 异常 | `dat_trd_repurchase` DELETE 失败→`RETURN` | 未覆盖 | 同上，已知缺口 |

## byfund `prc_trd_hz_byfund`

### 段 1 `c_get_data`（行级分支）

| # | 分支 | 触发条件 | 现状 | 补齐方式 |
|---|---|---|---|---|
| B1 | `fnc_com_get_seat_para` | 同左 | **部分覆盖**（'24'/'11'/'31' 臂 + '000000' 回退 + 全默认路径；18 臂全组合与非法 ELSE 组合未穷举） | — |
| B2 | `v_par_fund_check_para.is_venture` | 同左 | **已覆盖**（F001 HIT '1'、F002 MISS、F003 HIT '0'） | — |
| B3 | `v_return_rate_flag='1'` | 同左 | **已覆盖**（F001/X001/0100 行 flag='1'，组1 触发重算） | — |
| B4 | `stock_kind='0100' AND zqdm∈[609100,609999]` | 同左 | **已覆盖**（组1 jyfy 命中、组2 zqdm 609200 无 jyfy 行、组3/4/5 FALSE） | — |
| B5 | `v_par_bond`（`v_temp_stock_kind`） | 同左 | **已覆盖**（HIT 0130000001 + TOO_MANY 0130000002；NO_DATA_FOUND 由 `v4_byfund_branch_nodata_equiv` 变体覆盖——删除 bond 行后同 security_id 全 ND） | `OTHERS` 臂不可由合法种子确定性触发（同 P6 类缺口） |
| B6 | INSERT `dat_fund_cjqs` 异常 | 同左 | 已知缺口（写失败不可确定性触发） | — |

### 段 2 `c_get_venture_data`/`c_fxj`（嵌套动态游标）

| # | 分支 | 触发条件 | 现状 | 补齐方式 |
|---|---|---|---|---|
| B7 | 外层游标行数 | 同左 | **已覆盖**（2 行 '01'/'02' 前缀，各触发内层循环；0 行形态未覆盖——venture 表恒有行） | — |
| B8 | 内层 `c_fxj` 行 | 同左 | **部分覆盖**（两前缀均有聚合行；空聚合 EXIT 子分支未覆盖） | — |
| B9 | 内层 `v_par_fund_check_para` | 同左 | **已覆盖**（内层 fund_code F001/F003 均 HIT） | — |

### 段 3 `c_get_unknown`

| # | 分支 | 触发条件 | 现状 | 补齐方式 |
|---|---|---|---|---|
| B10 | 未维护行 | 同左 | **已覆盖**（ZZ999/X999/S0001 组 → fund_code 未维护路径） | — |

### 段 4 QFII `FOR res IN (...)`（需 `qfii_dealer_date <= p_i_date`）

| # | 分支 | 触发条件 | 现状 | 补齐方式 |
|---|---|---|---|---|
| B11 | 段开关 | 同左 | **已覆盖**（'20250101' ≤ '20250715' → TRUE 臂执行；FALSE 臂由基础种子/小规模用例覆盖——原基础种子日期 > p_i_date） | — |
| B12 | FOR 游标行 | 同左 | **部分覆盖**（bs 'B'×4 + 'S'×1 + 'I'×1（排除）；'G'/'H'、gddmzm '000000' 回退臂未穷举） | — |
| B13 | Q-a `par_fund_partner` fund_code | 同左 | **已覆盖**（T1 HIT S0001、T5 '000000' 回退 HIT、T2 ZZ999 MISS→next_row） | — |
| B14 | Q-b `par_sys_securities` | 同左 | **已覆盖**（600000/600001 HIT、999999 MISS→next_row） | — |
| B15 | Q-c `par_fund_seat` seat_id | 同左 | **已覆盖**（X001/X003 HIT、X999 MISS→next_row） | — |
| B16 | `fnc_get_bond_yjlx` | 同左 | **部分覆盖**（T1 走到函数且 bond_kind HIT；函数异常臂需 securities HIT 但 bond 数据缺失的组合，未单独构造） | — |
| B17 | INSERT/UPDATE 异常 | →inst_log / split_flag 更新异常 | 未覆盖 | 已知缺口 |
| B18 | 成功收尾 | 同左 | **已覆盖**（T1 全命中行触发） | — |

## 已知不可确定性覆盖项（记录为验收边界）

- **P6/P7/B6/B17**：例程内 INSERT/DELETE 的 `WHEN OTHERS` 兜底——目标表无唯一约束、列宽已对齐合法值，无法用合法种子确定性触发写失败；两模式同点执行同一语句，Off/Safe 等价性不受影响（该路径两模式均不可达）。
- **基数 TOO_MANY（B5）**：可覆盖——`par_sys_bond` 同 `security_id` 两行 inure 日期重叠。
- **purchase `commision_fee_type` 2 字符臂**：`par_fund_seat_para.commision_fee_type` 为 `varchar(1)`（基线列宽），CASE '11' 等双字符臂**物理不可达**——种子注释已载明，需修基线列宽另行评审，本矩阵不重复造数。

## 验收执行结果（`tests/zz_v4_loop_parallel_probe.rs`，min_rows=1 强制并行路径）

| 用例 | 数据 | 结果 |
|---|---|---|
| `v4_purchase_branch_equiv` | seed_branch_purchase（P0-P5 全形态） | outs=EQ state=EQ，`Parallelized` rows=4 fallback=0 |
| `v4_purchase_branch_kind1_equiv` | 同上 + `UPDATE swh_all_kind` 切 '1'（P4） | outs=EQ state=EQ，`Parallelized` rows=4 fallback=0 |
| `v4_byfund_branch_equiv` | seed_branch_byfund（B1-B5+B7-B18 主要臂） | outs=EQ state=EQ，`Parallelized` rows=5 fallback=0 |
| `v4_byfund_branch_nodata_equiv` | 同上 + 删 0130000002 bond 行（B5 NO_DATA_FOUND 臂） | outs=EQ state=EQ，`Parallelized` rows=5 fallback=0 |
| `v4_byfund_branch_serial_stability` | 两 Off 引擎同数据对照 | outs=EQ state=EQ（串行确定性守护） |

### 种子确定性约束（重要）

byfund `p_o_succeed` 为**末次异常写赢**的 OUT 参，游标为 `GROUP BY` 无
`ORDER BY`——行序本身不稳定。种子设计因此约束：**所有会写
`p_o_succeed` 的异常行必须产生同一消息文本**（统一
`security_id='0130000002'` → TOO_MANY 臂同消息；ND 变体删除该
security_id 的 bond 行 → ND 臂同消息）。混合不同消息的异常行会使
Off↔Safe 乃至 Off↔Off 差分不稳定——`v4_byfund_branch_serial_stability`
用例即为该约束的守护（曾实测捕获 DIFF，修正种子后转绿）。

### 仍存缺口（如实记录）

- P6/P7/B6/B17：写失败路径两模式同点执行、合法种子不可确定性触发
- B1：CASE 18 臂未穷举（覆盖 '11'/'24'/'31' + 默认回退 + '000000' 回退）
- B8：内层 `c_fxj` 空聚合 EXIT 子分支未覆盖
- B12：bs 'G'/'H'、gddmzm '000000' 臂未穷举
- B16：`fnc_get_bond_yjlx` 异常臂未单独构造（需 securities HIT 但
  bond 数据缺失的组合）
- purchase P3：`v_tradedate_flag` 取值由 `fnc_com_if_deal_date` 内部
  逻辑决定，种子层面无法直接指定两臂
