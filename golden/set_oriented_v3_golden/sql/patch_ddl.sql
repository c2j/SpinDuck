-- byfund 100k 黄金补丁（两侧同源）：
--  commision_type 脱敏列宽 varchar(1) 无法容纳 '11'..'36' 两字符费型码
--  （seeds_params.sql 实际取值；同 PR#56 extra_tables.sql 口径）。
--  其余依赖表（par_oper_venture/dat_fund_xw_venture/dic_stock_kind_sub/
--  jyfy_temp/v_par_bond 全视图）已由 case_corpus_purchase 依赖闭包补齐，
--  本文件不再重复。
ALTER TABLE par_fund_seat_para ALTER COLUMN commision_type TYPE character varying(2);
