-- M0 剖面种子：参数表 + @DETAIL_N@ 行 mid_yjqs_detail（组键打散）+
-- @QFII_N@ 行 qfqscjhbrbb_temp（QFII 段开启：qfii_dealer_date='20200101'）。
-- 其余参数表沿用 case_corpus_hz_byfund 退役语料的最小集合口径。

INSERT INTO par_fund_info (fund_code, data_mode, fund_status) VALUES
('F001', '1', '1'),
('F002', '1', '1');

INSERT INTO par_fund_partner (fund_code, partner_code, market_code, sub_partner_code, inure_begin_date, inure_end_date, accnt_type) VALUES
('F001', 'A0001', '001', 'S0001', '20200101', '20991231', '0'),
('F002', 'A0002', '001', 'S0002', '20200101', '20991231', '0'),
('F001', 'QF0001', '001', '000000', '20200101', '20991231', '0'),
('F001', 'QF0002', '001', '000000', '20200101', '20991231', '0'),
('F002', 'QF0003', '001', '000000', '20200101', '20991231', '0'),
('F002', 'QF0004', '001', '000000', '20200101', '20991231', '0');

INSERT INTO par_fund_seat (seat_id, fund_code, seat_code, market_code, inure_begin_date, inure_end_date, valid_flag) VALUES
('1', 'F001', 'X001', '001', '20200101', '20991231', '1'),
('2', 'F002', 'X002', '001', '20200101', '20991231', '1'),
('3', 'F001', 'QX01', '001', '20200101', '20991231', '1'),
('4', 'F001', 'QX02', '001', '20200101', '20991231', '1'),
('5', 'F002', 'QX03', '001', '20200101', '20991231', '1'),
('6', 'F002', 'QX04', '001', '20200101', '20991231', '1');

-- 风险金维度（c_fxj 动态 SQL 段）：stock_kind_str 是拼进动态 SQL 的谓词片段
INSERT INTO par_oper_venture (stock_kind_str, venture_name, market_code, operation_kind) VALUES
('(substr(stock_kind,1,2) = ''01'')', '债券', '001', '01');

-- QFII 开关日期：早于业务日 → 过程内 IF p_i_date >= split_date 为真，QFII 段执行
INSERT INTO dic_all_kind (operation_kind, kind_id, kind_name) VALUES
('split_data_flag', 'qfii_dealer_date', '20200101');

-- QFII 段证券区间（zqdm 600001..600010 全覆盖）
INSERT INTO dic_stock_kind_sub (market_code, stock_kind, begin_code, end_code, descript) VALUES
('001', '0101', '600000', '699999', 'm0');

INSERT INTO par_sys_securities (main_market_code, main_stock_code, stock_kind, stock_status, security_id, inure_begin_date, inure_end_date)
SELECT '001', '60000' || lpad(g::text, 2, '0'), '01', '00', '0132000' || lpad(g::text, 3, '0'), '20200101', '20991231'
FROM generate_series(1, 10) AS t(g);

-- 债券主数据：c_get_data 每组都查 v_par_bond(bond_kind)，
-- security_id 与下方 mid_yjqs_detail 的 '0131'||lpad(g%211,15,'0') 全覆盖
INSERT INTO par_sys_bond (security_id, bond_kind, inure_begin_date, inure_end_date)
SELECT '0131' || lpad(g::text, 15, '0'), '0101', '20200101', '20991231'
FROM generate_series(0, 210) AS t(g);

-- mid_yjqs_detail：@DETAIL_N@ 行，security_id/gddm 组合打散形成大量游标组
INSERT INTO mid_yjqs_detail (gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl, cjje, yhs, jsf, jiesf, zgf, ghf, yj01, fxj, accrual, pay_type, ghcjje, cjbh, yj02, yj03, yj04, stock_kind, yj06, yj05, sub_gddm, yj07, yj08, yj09, is_yysg, ywlb, etf_flag, check_type, date_source, ghcjje2)
SELECT CASE WHEN g % 2 = 0 THEN 'A0001' ELSE 'A0002' END,
       CASE WHEN g % 2 = 0 THEN 'X001' ELSE 'X002' END,
       '001',
       '010001',
       '0131' || lpad((g % 211)::text, 15, '0'),
       CASE WHEN g % 3 = 0 THEN '-1' ELSE '1' END,
       '0', '20250715',
       100 + (g % 7), 10000.000 + g, 1.00, 2.00, 0.50, 0.50, 0.50,
       11.00, 3.00 + (g % 5), 0, '0', 10500.000, 300000 + g,
       10.50, 0, 0, '0101', 0, 0,
       CASE WHEN g % 2 = 0 THEN 'S0001' ELSE 'S0002' END,
       0, 0, 0, '0', '00', '0', '0', '0', 10560.000
FROM generate_series(1, @DETAIL_N@) AS t(g);

-- qfqscjhbrbb_temp：@QFII_N@ 行，按 (gddm,gddmzm,bs,zqdm,bcrq,gsdm,stock_kind) 聚成组
INSERT INTO qfqscjhbrbb_temp (gddm, gddmzm, gsdm, bcrq, bs, zqdm, cjsl, cjje, yjlx, yhs, ghf, zgf, jsf, qsf, fxjj, yj, seq_no)
SELECT 'QF000' || (1 + g % 4),
       '000000',
       'QX0' || (1 + g % 4),
       '20250715',
       CASE WHEN g % 2 = 0 THEN 'B' ELSE 'S' END,
       '60000' || (1 + g % 10),
       10 + (g % 5), 1000.00 + g, 1.00, 0.50, 0.25, 0.25, 0.10, 0.05, 0.02, 0.30,
       g
FROM generate_series(1, @QFII_N@) AS t(g);
