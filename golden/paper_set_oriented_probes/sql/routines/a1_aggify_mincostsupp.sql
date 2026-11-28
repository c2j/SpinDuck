-- 探针 A1 —— Aggify（arXiv:2004.05378）图 1：TPC-H Q2 的 minCostSupp。
-- 形态：游标循环 + 「下界约束下的最小值」状态转移；输出是标量 RETURN，循环体不写库。
-- 出处说明：原文为 T-SQL，图 1 在 arXiv 版里是位图，取不到逐字代码；本文件按 §2.1 正文
--   与 §5.1 给出的变量集 V_Δ={pCost,minCost,lb,suppName,sName}、V_fetch={pCost,sName}
--   重写为 GaussDB PL/SQL，语义不变：只接受 ps_supplycost >= lb 的供应记录，并列取游标首行。
-- prc_aggify_a1 对应论文里「每个 part 调用一次 UDF」的调用查询。
CREATE OR REPLACE FUNCTION fnc_min_cost_supp(p_key IN INTEGER, p_lb IN INTEGER) RETURN VARCHAR2 IS
  v_sname  VARCHAR(100);
  v_pcost  NUMBER;
  v_min    NUMBER;
  v_supp   VARCHAR(100);
  CURSOR c_ps IS
    SELECT s.s_name, ps.ps_supplycost
      FROM partsupplier ps, supplier s
     WHERE ps.ps_suppkey = s.s_suppkey
       AND ps.ps_partkey = p_key;
BEGIN
  OPEN c_ps;
  LOOP
    FETCH c_ps INTO v_sname, v_pcost;
    EXIT WHEN c_ps%NOTFOUND;
    IF (v_min IS NULL OR v_pcost < v_min) AND v_pcost >= p_lb THEN
      v_min  := v_pcost;
      v_supp := v_sname;
    END IF;
  END LOOP;
  CLOSE c_ps;
  RETURN v_supp;
END;
/

CREATE OR REPLACE PROCEDURE prc_aggify_a1(p_lb IN INTEGER) IS
BEGIN
  DELETE FROM result_min_supp;
  COMMIT;
  FOR r IN (SELECT p_partkey FROM part ORDER BY p_partkey) LOOP
    INSERT INTO result_min_supp(p_partkey, supp_name)
    VALUES (r.p_partkey, fnc_min_cost_supp(r.p_partkey, p_lb));
  END LOOP;
  COMMIT;
END;
/
