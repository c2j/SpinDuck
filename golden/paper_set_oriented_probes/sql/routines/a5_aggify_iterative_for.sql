-- 探针 A5 —— Aggify（arXiv:2004.05378）§8.2「Optimizing Iterative FOR Loops」。
-- 形态：计数型 FOR 循环（迭代空间是 1..n 的序列，没有游标），循环体逐个 INSERT。
-- 论文做法：把迭代空间表达成关系（recursive CTE 或 dual），从而退化成游标循环再聚合化。
-- 非论文原例：按 §8.2 的示例形态（FOR i = 0..100）自造。
-- 循环变量取名 v_i：若叫 i，会与结果表列名 i 同名，引擎会把 INSERT 列清单里的列名 i
--   一并替换成循环变量（生成 `insert into result_seq ( $v_i , sq )`），属名字解析缺口，另见 REPORT。
CREATE OR REPLACE PROCEDURE prc_aggify_a5(p_n IN INTEGER) IS
BEGIN
  DELETE FROM result_seq;
  COMMIT;
  FOR v_i IN 1 .. p_n LOOP
    INSERT INTO result_seq(i, sq)
    VALUES (v_i, v_i * v_i);
  END LOOP;
  COMMIT;
END;
/
