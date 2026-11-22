-- 参照侧计时主体：在 postmaster 内部用 clock_timestamp() 夹住过程调用。
-- 口径提醒：这里测的是「服务端语句执行」，不含客户端连接与 gsql 启动；
-- SpinDuck 侧测的是 CLI 进程整段墙钟。两者不可直接等同，比较只看量级。
--
-- 形态说明：openGauss 5.0/7.0 在 DO 块里用 `CALL pkg.proc(...)` 会报
-- `ERROR: query has no destination for result data`（本包实测），
-- 过程必须写成普通语句 `pkg.proc(args);`，OUT 参用局部变量承接。
--
-- 占位符由 reference/sp_call.sh 用 sed 替换：:ROUTINE :DATE
DO $$
DECLARE
  v_out varchar2(4000);
  t0    timestamptz;
  t1    timestamptz;
BEGIN
  t0 := clock_timestamp();
  pkg_split_trade_step3_sh.:ROUTINE(':DATE', v_out);
  t1 := clock_timestamp();
  RAISE NOTICE 'ELAPSED_MS=% out=%',
    round(extract(epoch FROM (t1 - t0)) * 1000, 3), v_out;
END $$;
