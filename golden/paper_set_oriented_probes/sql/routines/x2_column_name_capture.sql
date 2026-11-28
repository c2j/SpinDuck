-- 探针 X2 —— 列名与循环变量同名（非论文案例，是引擎名字解析的缺口）。
-- FOR i IN 1..n 的循环变量叫 i，结果表 result_seq 的列也叫 i。
-- Oracle PL/SQL 语义下 INSERT 列清单里的 i 是列名；引擎把它一并替换成循环变量，生成
-- `insert into result_seq ( $v_i , sq ) values ( $v_i , $v_i * $v_i )`。
-- 同一类现象另见 procbench 的逐字例程：C0（参数名遮蔽表名）、D0（列别名被变量替换）。
CREATE OR REPLACE PROCEDURE prc_xcol_capture(p_n IN INTEGER) IS
BEGIN
  DELETE FROM result_seq;
  COMMIT;
  FOR i IN 1 .. p_n LOOP
    INSERT INTO result_seq(i, sq)
    VALUES (i, i * i);
  END LOOP;
  COMMIT;
END;
/
