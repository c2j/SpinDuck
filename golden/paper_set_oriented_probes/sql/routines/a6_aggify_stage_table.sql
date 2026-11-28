-- 探针 A6 —— Aggify（arXiv:2004.05378）§4「Supported operations」：
--   「DML operations (INSERT, UPDATE, DELETE) on local table variables or temporary tables
--    or collections are supported」；其真实负载 L2/L3/L6 都是「includes table inserts」。
-- 形态：循环体写中间表 stage_agg，循环结束后再从中间表汇总写入结果表。
-- 语义要点：后一条语句读前一个循环写的东西（读己所写），集合化时必须保序。
-- 非论文原例：按上述支持面自造。
CREATE OR REPLACE PROCEDURE prc_aggify_a6 IS
  v_k  INTEGER;
  CURSOR c IS
    SELECT p_partkey
      FROM part
     ORDER BY p_partkey;
BEGIN
  DELETE FROM result_from_tmp;
  DELETE FROM stage_agg;
  COMMIT;
  OPEN c;
  LOOP
    FETCH c INTO v_k;
    EXIT WHEN c%NOTFOUND;
    INSERT INTO stage_agg(k, v)
    VALUES (v_k, v_k * 1.5);
  END LOOP;
  CLOSE c;
  COMMIT;
  INSERT INTO result_from_tmp(k, v10)
  SELECT k, SUM(v)
    FROM stage_agg
   GROUP BY k;
  COMMIT;
END;
/
