-- 探针 X1 —— 跨文件裸名函数调用的注册顺序（非论文案例，是本引擎计划面的行为）。
-- prc_xfile_call 在本文件（文件名排序在 b1_froid_total_price.sql 之前），被调的
-- xchg_rate（Froid 图 1 的那个函数）定义在 b1 文件里。
-- SQL 语句（INSERT ... VALUES）里出现裸名函数调用时，引擎只有在「被调方已注册」时才把它
-- 识别成 fn_site；调用方先注册则原样下发给 DuckDB，报 KERNEL-0010
-- 「Scalar Function ... does not exist」。
-- --plan 的 ddl_routines 把入口文件排在最前，所以这种跨文件调用在计划形态下按构造失败；
-- 把被调方文件排在前面用 --file 注册则正常（run_probes.sh 的 call-ordered 行就是它）。
CREATE OR REPLACE PROCEDURE prc_xfile_call IS
BEGIN
  DELETE FROM result_xfile;
  COMMIT;
  FOR r IN (SELECT from_cur FROM xchg ORDER BY from_cur) LOOP
    INSERT INTO result_xfile(fx_from, rate)
    VALUES (r.from_cur, xchg_rate(r.from_cur, 'EUR'));
  END LOOP;
  COMMIT;
END;
/
