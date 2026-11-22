-- byfund 参数补件：datasets_byfund/seed.sql 只覆盖了 6 张配置表，
-- 存过体里的其余外部依赖必须补齐，否则两侧都走不到「业务成功」分支
-- （OUT p_o_succeed = 0），一致性/性能对比也就失去意义。逐条理由：
-- 1) 种子内 2 行明细的 security_id 需在债券主数据里可查，否则报
--    「债券类型不存在…请在债券信息表中维护参数！」；
-- 2) QFII 分支的门闩读 dic_all_kind('qfii_dealer_date')，未登记时
--    取值漂移，存过与集合化两侧走不同分支；
-- 3) 席位/子板块换算要 dic_stock_kind_sub 命中市档区间；
-- 4) par_sys_securities 供 main_xwdm 反查；
-- 5) qfqscjhbrbb_temp 是产物表之一，存过体开头清空后重写；
-- 6) par_fund_check_para 决定 is_venture/venture_type，缺件则venture 面为空。
-- 大规模驱动数据另见 sql/gen_byfund.sql（含同形状的 par_sys_bond 生成）。
INSERT INTO par_sys_bond(security_id,bond_kind,inure_begin_date,inure_end_date) VALUES ('0130000001','0201','20200101','20991231');
UPDATE dic_all_kind SET kind_name='20200101' WHERE kind_id='qfii_dealer_date';
INSERT INTO dic_stock_kind_sub(market_code,begin_code,end_code,stock_kind) VALUES ('001','600000','699999','0501');
INSERT INTO par_sys_securities(security_id,main_market_code,stock_status,main_stock_code,stock_kind,inure_begin_date,inure_end_date) VALUES ('QFSEC','001','00','600099','05','20200101','20991231');
INSERT INTO qfqscjhbrbb_temp(gddm,gddmzm,gsdm,bcrq,bs,zqdm,cjsl,cjje,yjlx,yhs,ghf,zgf,jsf,qsf,fxjj,yj,seq_no) VALUES ('A0001','S0001','X001','20250715','B','600099',7,700,0,1,1,1,1,1,1,1,1);
INSERT INTO par_fund_check_para(fund_code,inure_begin_date,inure_end_date,is_venture,venture_type) VALUES ('F001','20200101','20991231','1','0'),('F002','20200101','20991231','1','0');
