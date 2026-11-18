-- v_par_bond：债券信息视图（par_sys_bond 最小投影，供 bond_kind 查询）
CREATE OR REPLACE VIEW v_par_bond AS
SELECT security_id, bond_kind, inure_begin_date, inure_end_date
  FROM par_sys_bond;
