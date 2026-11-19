-- hz_byfund 参数种子（SpinDuck / openGauss 双端同源；generate_series 两侧同义）
-- 语料基线参数（par_fund_partner F001/F002、par_fund_seat、par_sys_purchase、swh_all_kind）
INSERT INTO par_fund_partner (fund_code, partner_code, market_code, sub_partner_code, inure_begin_date, inure_end_date, accnt_type) VALUES
('F001','A0001','001','S0001','20200101','20991231','0'),
('F002','A0002','001','S0002','20200101','20991231','0');

INSERT INTO par_fund_seat (seat_id, fund_code, seat_code, market_code, inure_begin_date, inure_end_date) VALUES
('1','F001','X001','001','20200101','20991231'),
('2','F002','X002','001','20200101','20991231');

INSERT INTO par_sys_purchase (security_id, purchase_days, purchase_kind, inure_begin_date, inure_end_date, if_capital) VALUES
('0130000001',90,'0','20200101','20991231','0'),
('0130000002',180,'0','20200101','20991231','0');

INSERT INTO swh_all_kind (operation_kind, kind_id, kind_name) VALUES
('COMMISSION_SWITCH','0','佣金方式开关-关闭');

-- 股东→基金映射：GD1..GD200 → F1..F200（fnc_com_get_fund_code 桩经此表）
INSERT INTO par_fund_partner (fund_code, partner_code, market_code, sub_partner_code, inure_begin_date, inure_end_date, accnt_type)
SELECT 'F' || g, 'GD' || g, '001', '000000', '20200101', '20991231', '0'
  FROM generate_series(1,200) AS t(g);

-- 席位参数：F1..F200 × X1..X10 × 3 种 stock_kind，commision_type/return_rate 周期变化
INSERT INTO par_fund_seat_para (fund_code, market_code, seat_code, stock_kind,
                                commision_type, inure_begin_date, inure_end_date,
                                return_rate_flag, return_rate_org, return_rate)
SELECT 'F' || f, '001', 'X' || s, k,
       CASE (f + s) % 5 WHEN 0 THEN '11' WHEN 1 THEN '12' WHEN 2 THEN '21'
                          WHEN 3 THEN '31' ELSE '00' END,
       '20200101', '20991231',
       CASE WHEN (f + s) % 10 = 0 THEN '1' ELSE '0' END,
       0.001, 0.0005
  FROM generate_series(1,200) AS a(f),
       generate_series(1,10)  AS b(s),
       (SELECT * FROM (VALUES ('0501'),('0502'),('0100')) AS v(k)) c;

-- 债券信息（v_par_bond 源）：SEC1..SEC100，偶数 bond_kind='0201'（→accrual2 分支）
INSERT INTO par_sys_bond (security_id, bond_kind, inure_begin_date, inure_end_date)
SELECT 'SEC' || g, CASE WHEN g % 2 = 0 THEN '0201' ELSE '0101' END,
       '20200101', '20991231'
  FROM generate_series(1,100) AS t(g);

-- 经营风险金市场参数（c_get_venture_data：stock_kind_str 为 SQL 片段）
INSERT INTO par_oper_venture (stock_kind_str, operation_kind, market_code) VALUES
('stock_kind LIKE ''05%''', '01', '001');

-- QFII 段字典（qfqscjhbrbb_temp 为空时不触发，仅保证字典完备）
INSERT INTO dic_stock_kind_sub (market_code, begin_code, end_code, stock_kind) VALUES
('001','600000','699999','0501');

-- jyfy_temp：zqdm 609100-609999 的席位成交金额（0100 品种分支可能命中；bs 经 decode 后为 NULL → 恒 0）
INSERT INTO jyfy_temp (zqdm, gddm, data_date, bs, jydy, jyfy)
SELECT '609' || (100 + g), 'GD' || ((g % 200) + 1), '20250715', '1', 'X' || ((g % 10) + 1), g * 1.5
  FROM generate_series(1,50) AS t(g);
