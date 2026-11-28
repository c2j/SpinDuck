-- 探针 E1 —— Hirn & Grust「Compiling PL/SQL Away」（arXiv:1909.03291，CIDR 2020）图 3 的 PL/SQL 函数 walk()。
-- 代码逐字取自配套仓库 One-WITH-RECURSIVE-is-Worth-Many-GOTOs/Code 的 markov/markov-pl.sql
-- （PostgreSQL 11 的 PL/pgSQL）。改写成 GaussDB PL/SQL 的改动：
--   [头]  头部语法；
--   [确]  roll = random() → steps MOD 100 / 100.0。random() 让结果不可复现，Off 与集合化两轮无法比对；
--   [求]  这一步经 SQL（SELECT … FROM sys_dummy）求值：PL 表达式里的中缀 MOD 与 1 / 100.0 都算不对
--         （见探针 X4.mod / X4.small），留在 PL 侧会让函数在 Off 下就取不到行。
--   [空]  取不到行时 PostgreSQL 得 NULL，Oracle 抛 NO_DATA_FOUND → 加异常块；
--   [名]  形参与局部变量加 p_ / v_ 前缀，避免与表列同名（curr_state 既是变量名也是谓词里的列值）。
-- 迁移概率区间（p_from / p_to）用窗口函数计算的标量子查询、以及「FOR 1..max_steps + 每步三次查表 + 提前 RETURN」
-- 的控制流保持原样。形态：步数由数据决定（提前 RETURN），每一步都带内嵌 SQL——论文用递归 SQL 编译它。

CREATE OR REPLACE FUNCTION walk(p_start_state IN INTEGER, p_success_at IN INTEGER, p_failure_at IN INTEGER, p_max_steps IN INTEGER) RETURN INTEGER IS
  v_total_reward  INTEGER := 0;
  v_curr_state    INTEGER := p_start_state;
  v_curr_action   VARCHAR(10) := '';
  v_roll          NUMBER;
  v_r             NUMBER;
BEGIN
  FOR v_steps IN 1 .. p_max_steps LOOP
    -- Find the action the policy finds appropriate in the current state
    SELECT p.action_name
      INTO v_curr_action
      FROM policy p, states s
     WHERE v_curr_state = s.id
       AND p.state_id = s.id;

    -- [确][求] Random number (double precision) roll ∈ [0.0, 1.0)
    SELECT MOD(v_steps, 100) / 100.0
      INTO v_roll
      FROM sys_dummy;

    -- Find the state we actually reach. There may be a chance we end up in another state.
    SELECT possible_move.s_to
      INTO v_curr_state
      FROM (SELECT a.s_to,
                   COALESCE(SUM(a.p) OVER (ORDER BY a.id ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0.0) AS p_from,
                   SUM(a.p) OVER (ORDER BY a.id) AS p_to
              FROM actions a
             WHERE v_curr_state = a.s_from
               AND v_curr_action = a.name) possible_move
     WHERE possible_move.p_from <= v_roll
       AND v_roll < possible_move.p_to;

    -- Add the reward we receive by stepping on the state we actually reached
    SELECT s.r
      INTO v_r
      FROM states s
     WHERE v_curr_state = s.id;
    v_total_reward := v_total_reward + v_r;

    IF v_total_reward >= p_success_at OR v_total_reward <= p_failure_at THEN
      RETURN v_steps * SIGN(v_total_reward);
    END IF;
  END LOOP;
  RETURN 0;
END;
/

-- 驱动：对 walk_params 里每组参数各走一次（规范基底）。
CREATE OR REPLACE PROCEDURE prc_hirn_e1 IS
BEGIN
  DELETE FROM result_walk;
  COMMIT;
  FOR r IN (SELECT start_state, success_at, failure_at, max_steps FROM walk_params ORDER BY max_steps, start_state) LOOP
    BEGIN
      INSERT INTO result_walk(start_state, success_at, failure_at, max_steps, steps)
      VALUES (r.start_state, r.success_at, r.failure_at, r.max_steps,
              walk(r.start_state, r.success_at, r.failure_at, r.max_steps));
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/
