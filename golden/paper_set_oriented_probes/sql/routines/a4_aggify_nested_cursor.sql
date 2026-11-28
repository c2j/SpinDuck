-- 探针 A4 —— Aggify（arXiv:2004.05378）§6.3.1「Nested cursor loops」。
-- 形态：外层游标逐个 part；内层游标（引用外层当前行的变量 v_pk）累加该 part 的供应记录；
--   外层每轮写一行聚合结果。内层无写、外层写库——与 prc_trd_hz_byfund 的
--   c_get_venture_data / c_fxj 结构同型，只是内层游标是静态的。
-- 论文做法：先对内层循环跑算法得到查询，再对内层循环（已变为查询）跑一遍。
-- 非论文原例：论文只给了处理步骤（其示例 L8-W2 在另一篇工作负载报告里），按步骤自造。
CREATE OR REPLACE PROCEDURE prc_aggify_a4 IS
  v_pk    INTEGER;
  v_cost  NUMBER;
  v_cnt   NUMBER;
  v_sum   NUMBER;
  CURSOR c_supp IS
    SELECT ps_supplycost
      FROM partsupplier
     WHERE ps_partkey = v_pk;
BEGIN
  DELETE FROM result_nested;
  COMMIT;
  FOR p IN (SELECT p_partkey, p_name FROM part ORDER BY p_partkey) LOOP
    v_pk  := p.p_partkey;
    v_cnt := 0;
    v_sum := 0;
    OPEN c_supp;
    LOOP
      FETCH c_supp INTO v_cost;
      EXIT WHEN c_supp%NOTFOUND;
      v_cnt := v_cnt + 1;
      v_sum := v_sum + v_cost;
    END LOOP;
    CLOSE c_supp;
    INSERT INTO result_nested(p_partkey, p_name, n_suppliers, total_cost)
    VALUES (p.p_partkey, p.p_name, v_cnt, v_sum);
  END LOOP;
  COMMIT;
END;
/
