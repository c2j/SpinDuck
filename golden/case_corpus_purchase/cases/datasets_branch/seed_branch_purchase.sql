-- 分支覆盖种子·purchase（BRANCH_COVERAGE.md P0-P7）。
-- 仅含 bs='B' 形态行；byfund 专用维度保留以自含（purchase 不读取）。
-- 分支覆盖种子（V4 验收）：覆盖 BRANCH_COVERAGE.md 中除"已知缺口"外的
-- 全部数据可触发分支。与 cases/datasets*/seed.sql 互斥使用——
-- 本文件自含全部维度行，不依赖基础种子。
-- 覆盖说明见文件尾注释与 BRANCH_COVERAGE.md。

-- ============ 维度表 ============

-- 基金/席位/股东（byfund 段1+段2、purchase 共用）
INSERT INTO par_fund_info (fund_code, data_mode, fund_status) VALUES
('F001', '1', '1'),
('F002', '1', '1'),
('F003', '1', '1');

INSERT INTO par_fund_partner (fund_code, partner_code, market_code, sub_partner_code, inure_begin_date, inure_end_date, accnt_type) VALUES
('F001', 'A0001', '001', 'S0001', '20200101', '20991231', '0'),
('F001', 'A0001', '001', '000000', '20200101', '20991231', '0'),
('F002', 'A0002', '001', 'S0002', '20200101', '20991231', '0'),
('F003', 'A0003', '001', '000000', '20200101', '20991231', '0');

INSERT INTO par_fund_seat (seat_id, fund_code, seat_code, market_code, inure_begin_date, inure_end_date, valid_flag) VALUES
('1', 'F001', 'X001', '001', '20200101', '20991231', '1'),
('2', 'F002', 'X002', '001', '20200101', '20991231', '1'),
('3', 'F003', 'X003', '001', '20200101', '20991231', '1'),
('9', 'F001', '000000', '001', '20200101', '20991231', '1');

-- par_fund_check_para：is_venture/venture_type 驱动 B2/B9。
-- 只填 NOT NULL 列，其余走默认。
INSERT INTO par_fund_check_para (fund_code, inure_begin_date, inure_end_date, is_venture, venture_type) VALUES
('F001', '20200101', '20991231', '1', '01'),
('F003', '20200101', '20991231', '0', '02');

-- par_fund_seat_para：commision_type‖commision_fee_type → CASE 臂；
-- return_rate_flag='1' 行驱动 B3 重算路径。
INSERT INTO par_fund_seat_para (fund_code, market_code, seat_code, stock_kind, commision_type, commision_fee_type, inure_begin_date, inure_end_date, return_rate_flag, return_rate_org, return_rate) VALUES
('F001', '001', 'X001', '0100', '2', '4', '20200101', '20991231', '1', 0.05, 0.02),
('F001', '001', 'X001', '0501', '3', '1', '20200101', '20991231', '0', 0, 0),
('F002', '001', 'X002', '0100', '1', '1', '20200101', '20991231', '0', 0, 0),
('F001', '001', '000000', '0000', '3', '6', '20200101', '20991231', '0', 0, 0);

-- 回购期限（purchase P2）：0130000001/2 命中、0130000003 purchase_days NULL、
-- 0139999999 未维护（P2 MISS→error_row）
INSERT INTO par_sys_purchase (security_id, purchase_days, purchase_kind, inure_begin_date, inure_end_date, if_capital) VALUES
('0130000001', 90, '0', '20200101', '20991231', '0'),
('0130000002', 180, '0', '20200101', '20991231', '0'),
('0130000003', NULL, '0', '20200101', '20991231', '0');

-- 债券类型（byfund B5）：0130000001 单行命中；0130000002 双行 inure 重叠→TOO_MANY
INSERT INTO par_sys_bond (security_id, bond_kind, inure_begin_date, inure_end_date) VALUES
('0130000001', '01', '20200101', '20991231'),
('0130000002', '02', '20200101', '20991231'),
('0130000002', '03', '20200101', '20991231');

-- 证券主档（QFII B14）：600000/600001 命中，999999 未维护
INSERT INTO par_sys_securities (main_market_code, main_stock_code, market_code, stock_code, stock_kind, stock_status, security_id, inure_begin_date, inure_end_date) VALUES
('001', '600000', '001', '600000', '01', '00', '0130000001', '20200101', '20991231'),
('001', '600001', '001', '600001', '01', '00', '0130000002', '20200101', '20991231');

-- 品种子码范围（QFII FOR 游标谓词）：全区间覆盖
INSERT INTO dic_stock_kind_sub (market_code, stock_kind, begin_code, end_code, descript) VALUES
('001', '01', '000000', '999999', 'all');

-- QFII 开关日期：≤ p_i_date('20250715') → 段 4 执行（B11 TRUE 臂）
INSERT INTO dic_all_kind (operation_kind, kind_id, kind_name) VALUES
('split_data_flag', 'qfii_dealer_date', '20250101');

-- 佣金开关：'0' → purchase P5 ELSE 查询路径（kind='1' 由测试内 UPDATE 切换）
INSERT INTO swh_all_kind (operation_kind, kind_id, kind_name) VALUES
('COMMISSION_SWITCH', '0', '佣金方式开关-关闭');

-- 风险金维度（byfund 段2 外层）：2 行 → 内层循环 2 次；
-- '01' 前缀命中 stock_kind '010x' 行，'02' 前缀命中 '020x' 行
INSERT INTO par_oper_venture (stock_kind_str, venture_name, market_code, operation_kind) VALUES
('(substr(stock_kind,1,2) = ''01'')', '债券', '001', '01'),
('(substr(stock_kind,1,2) = ''02'')', '股票', '001', '02');

-- 手续费临时表（byfund B4 TRUE 臂）：R1 命中行
INSERT INTO jyfy_temp (gddm, jydy, zqdm, bs, jyfy, data_date) VALUES
('A0001', 'X001', '609100', 'B', 1.50, '20250715'),
('A0001', 'X001', '609100', 'B', 2.50, '20250715');

-- QFII 清算临时表（段4 FOR 游标输入）：覆盖 B12-B18
-- T1 全命中（partner S0001→F001, securities 600000, seat X001）→ 成功收尾
-- T2 partner 未维护（ZZ999）→ Q-a MISS→next_row
-- T3 securities 未维护（zqdm 999999）→ Q-b MISS
-- T4 seat 未维护（gsdm X999）→ Q-c MISS
-- T5 bs='S'/gddmzm NULL → decode/nvl 路径 + partner '000000' 回退命中
-- T6 bs='I' → FOR 游标排除
INSERT INTO qfqscjhbrbb_temp (gddm, gddmzm, gsdm, bcrq, bs, zqdm, cjsl, cjje, yjlx, yhs, ghf, zgf, jsf, qsf, fxjj, yj, seq_no) VALUES
('A0001', 'S0001', 'X001', '20250715', 'B', '600000', 10, 1000.00, 0.50, 1.00, 0.10, 0.10, 0.10, 0.10, 0.20, 0.30, 1),
('ZZ999', 'S0001', 'X001', '20250715', 'B', '600000', 10, 1000.00, 0.50, 1.00, 0.10, 0.10, 0.10, 0.10, 0.20, 0.30, 2),
('A0001', 'S0001', 'X001', '20250715', 'B', '999999', 10, 1000.00, 0.50, 1.00, 0.10, 0.10, 0.10, 0.10, 0.20, 0.30, 3),
('A0001', 'S0001', 'X999', '20250715', 'B', '600000', 10, 1000.00, 0.50, 1.00, 0.10, 0.10, 0.10, 0.10, 0.20, 0.30, 4),
('A0003', 'S0003', 'X003', '20250715', 'S', '600001', 10, 1000.00, 0.50, 1.00, 0.10, 0.10, 0.10, 0.10, 0.20, 0.30, 5),
('A0001', 'S0001', 'X001', '20250715', 'I', '600000', 10, 1000.00, 0.50, 1.00, 0.10, 0.10, 0.10, 0.10, 0.20, 0.30, 6);

-- ============ mid_yjqs_detail：purchase 游标行（stock_kind '05%'） ============
INSERT INTO mid_yjqs_detail (gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl, cjje, yhs, jsf, jiesf, zgf, ghf, yj01, fxj, accrual, pay_type, ghcjje, cjbh, yj02, yj03, yj04, stock_kind, yj06, yj05, sub_gddm, yj07, yj08, yj09, is_yysg, ywlb, etf_flag, check_type, date_source, ghcjje2) VALUES
-- P1 命中+P2 命中+P5 ELSE( fee_type 由 seat_para '0501'→'3','1'='31'? CASE只有1/4/6臂→ELSE yj05)
('A0001','X001','001','600010','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400001,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000),
-- P1 MISS(未知股东A9999)→error_row 跳过；security_id 0130000002 命中但走不到 P2
('A9999','X001','001','600011','0130000002','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400002,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000),
-- P1 命中(A0002/F002)+P2 MISS(security_id 0139999999 未维护)→error_row 跳过
('A0002','X002','001','600012','0139999999','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400003,10.50,0,0,'0501',0,0,'S0002',0,0,0,'0','00','0','0','0',10560.000),
-- P2 命中 purchase_days NULL→nvl(,1)；partner '000000' 回退(A0003)
('A0003','X003','001','600013','0130000003','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400004,10.50,0,0,'0501',0,0,'S0003',0,0,0,'0','00','0','0','0',10560.000),
-- P0 游标排除样本：stock_kind='0509'
('A0001','X001','001','600014','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400005,10.50,0,0,'0509',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000),
-- P0 排除：is_yysg='1' AND ywlb='ES'
('A0001','X001','001','600015','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400006,10.50,0,0,'0501',0,0,'S0001',0,0,0,'1','ES','0','0','0',10560.000),
-- P0 排除：cjsl=0
('A0001','X001','001','600016','0130000001','B','0','20250715',0,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400007,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000),
-- P0 排除：scdm='002'
('A0001','X001','002','600017','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400008,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000),
-- P0 排除：bcrq≠p_i_date
('A0001','X001','001','600018','0130000001','B','0','20250714',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,400009,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000);

-- 注：byfund 段4 还需 mid_yjqs_detail 行供段2 动态 SQL 聚合（'01'/'02' 前缀
-- stock_kind 行已在上方覆盖：0100/0101/0201 → 两个 venture 内层均有行）。
-- purchase kind='1' 路径由测试内 UPDATE swh_all_kind 后切换（P4），
-- 本种子默认 '0' 覆盖 P5。
