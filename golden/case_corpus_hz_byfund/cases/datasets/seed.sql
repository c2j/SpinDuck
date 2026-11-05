-- 种子（自实现）：驱动 prc_trd_hz_byfund 主聚合循环。
-- 覆盖：两笔正常成交（不同基金/席位）→ dat_fund_cjqs 两行；债券类型未维护
--        （v_par_bond 无行）→ 两侧同为 NO_DATA_FOUND 路径；风险金维度表
--        par_oper_venture 一行 → 动态 SQL 段（OPEN FOR + 函数下沉）执行，
--        结果落 dat_fund_xw_venture；QFII 开关日期设为远期 → QFII 段不执行。

INSERT INTO par_fund_info (fund_code, data_mode, fund_status) VALUES
('F001', '1', '1'),
('F002', '1', '1');

INSERT INTO par_fund_partner (fund_code, partner_code, market_code, sub_partner_code, inure_begin_date, inure_end_date, accnt_type) VALUES
('F001', 'A0001', '001', 'S0001', '20200101', '20991231', '0'),
('F002', 'A0002', '001', 'S0002', '20200101', '20991231', '0');

INSERT INTO par_fund_seat (seat_id, fund_code, seat_code, market_code, inure_begin_date, inure_end_date, valid_flag) VALUES
('1', 'F001', 'X001', '001', '20200101', '20991231', '1'),
('2', 'F002', 'X002', '001', '20200101', '20991231', '1');

-- 风险金维度（动态 SQL 段）：stock_kind_str 是拼进动态 SQL 的谓词片段
INSERT INTO par_oper_venture (stock_kind_str, venture_name, market_code, operation_kind) VALUES
('(substr(stock_kind,1,2) = ''01'')', '债券', '001', '01');

-- QFII 开关日期：远期 → 过程内 IF p_i_date >= split_date 为假，QFII 段跳过
INSERT INTO dic_all_kind (operation_kind, kind_id, kind_name) VALUES
('split_data_flag', 'qfii_dealer_date', '99991231');

INSERT INTO mid_yjqs_detail (gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl, cjje, yhs, jsf, jiesf, zgf, ghf, yj01, fxj, accrual, pay_type, ghcjje, cjbh, yj02, yj03, yj04, stock_kind, yj06, yj05, sub_gddm, yj07, yj08, yj09, is_yysg, ywlb, etf_flag, check_type, date_source, ghcjje2) VALUES
('A0001', 'X001', '001', '010001', '0130000001', '1', '0', '20250715', 100, 10000.000, 1.00, 2.00, 0.50, 0.50, 0.50, 11.00, 3.00, 0, '0', 10500.000, 200001, 10.50, 0, 0, '0101', 0, 0, 'S0001', 0, 0, 0, '0', '00', '0', '0', '0', 10560.000),
('A0002', 'X002', '001', '010001', '0130000001', '1', '0', '20250715', 200, 20000.000, 2.00, 4.00, 1.00, 1.00, 1.00, 11.00, 6.00, 0, '0', 21000.000, 200002, 20.00, 0, 0, '0101', 0, 0, 'S0002', 0, 0, 0, '0', '00', '0', '0', '0', 21120.000);
