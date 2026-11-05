-- 种子（post-step3 形态：直接构造 prc_trd_deal_purchase 游标输入）
-- 说明：par_fund_seat_para.commision_fee_type 在脱敏基线中为 varchar(1)，
-- 而 step4 CASE 分支费型码为 2 字符（'11'/'12'…）无法存储——考题脱敏件自身
-- 的列宽不一致。黄金口径取确定性路径 swh_all_kind.kind_id='0'
-- （purchase_yj 两侧同为 NULL）；CASE 路径须先修基线列宽后另行覆盖。

INSERT INTO mid_yjqs_detail (gddm, xwdm, scdm, zqdm, security_id, bs, trade_type, bcrq, cjsl, cjje, yhs, jsf, jiesf, zgf, ghf, yj01, fxj, accrual, pay_type, ghcjje, cjbh, yj02, yj03, yj04, stock_kind, yj06, yj05, sub_gddm, yj07, yj08, yj09, is_yysg, ywlb, etf_flag, check_type, date_source, ghcjje2) VALUES
('A0001','X001','001','600000','0130000001','B','0','20250715',100,10000.000,1.00,2.00,0.50,0.50,0.50,11.00,3.00,0,'0',10500.000,100001,10.50,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',10560.000),
('A0002','X002','001','600001','0130000002','B','0','20250715',200,20000.000,2.00,4.00,1.00,1.00,1.00,11.00,6.00,0,'0',21000.000,100002,20.00,0,0,'0501',0,0,'S0002',0,0,0,'0','00','0','0','0',21120.000),
('A999','X001','001','600002','0130000001','B','0','20250715',50,5000.000,0.50,1.00,0.25,0.25,0.25,5.00,1.50,0,'0',5250.000,100003,5.00,0,0,'0501',0,0,'S0001',0,0,0,'0','00','0','0','0',5280.000);

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
