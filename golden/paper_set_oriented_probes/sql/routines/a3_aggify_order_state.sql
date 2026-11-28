-- 探针 A3 —— Aggify（arXiv:2004.05378）§6.1「Order enforcement」与 §7 的状态模型
--   P_i = f(P_{i-1}, T_i)：跨迭代携带的状态，且结果依赖游标顺序。
-- 形态：游标带 ORDER BY acct, tx_id；循环内维护运行余额，换账户时清零，逐行写出当前余额。
-- 这是金融存过里最常见的「游标 + 累计」写法；集合等价物是
--   SUM(amt) OVER (PARTITION BY acct ORDER BY tx_id ROWS UNBOUNDED PRECEDING)。
-- 非论文原例：按 §6.1/§7 的语义要求自造。
CREATE OR REPLACE PROCEDURE prc_aggify_a3 IS
  v_id    INTEGER;
  v_acct  VARCHAR(10);
  v_amt   NUMBER;
  v_bal   NUMBER;
  v_prev  VARCHAR(10);
  CURSOR c_t IS
    SELECT tx_id, acct, amt
      FROM txns
     ORDER BY acct, tx_id;
BEGIN
  DELETE FROM result_running;
  COMMIT;
  OPEN c_t;
  LOOP
    FETCH c_t INTO v_id, v_acct, v_amt;
    EXIT WHEN c_t%NOTFOUND;
    IF v_prev IS NULL OR v_acct <> v_prev THEN
      v_bal := 0;
    END IF;
    v_bal := v_bal + v_amt;
    INSERT INTO result_running(tx_id, acct, amt, bal)
    VALUES (v_id, v_acct, v_amt, v_bal);
    v_prev := v_acct;
  END LOOP;
  CLOSE c_t;
  COMMIT;
END;
/
