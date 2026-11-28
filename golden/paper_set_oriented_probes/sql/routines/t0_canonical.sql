-- 阶梯探针（规范基底）—— 非论文案例，给矩阵定标、定位「可转换形状」的边界。
-- 规范基底取自仓库自己的可转换夹具（spinduck/tests/set_oriented_dummy.rs 的
-- caller_expr）：FOR r IN (SELECT …) LOOP … BEGIN INSERT …
-- EXCEPTION WHEN OTHERS THEN NULL; END; END LOOP——INSERT 套在跳行异常块里。
-- 之后每个过程只往基底上加**一个**特性，看哪个特性单独就能让转换失败：
--
--   T0      基底（阳性对照：必须 Converted，否则矩阵里的「全 Skipped」不可解读）
--   T0.null 基底，INSERT 的字符串字面量换成 NULL 字面量
--   T0.s    基底，游标改成 FETCH c INTO v1, v2（标量目标列表）
--   T0.r    基底，游标改成 FETCH c INTO r（%ROWTYPE 记录；游标先声明，与语料写法一致）
--   T0.rf   同 T0.r，但记录变量写在游标声明之前（前向引用；计划面把它解析成缺件）
--   T2      + 循环内 IF/ELSE 给局部变量赋值（每轮必赋）
--   T2b     + 循环内 IF … THEN 赋值 END IF（无 ELSE；变量只有声明期初值）
--   T2c     + 循环内先赋值再 IF … THEN 覆写 END IF（字符串；夹具里的「覆写门」形态）
--   T2d     + 循环内先赋值再 IF … THEN 覆写 END IF（数值运算）
--   T3      + 跨迭代累计（v_sum := v_sum + cost，结果依赖游标顺序）
--   T7      + INSERT 里调用单 RETURN 纯表达式函数（可编译成 DuckDB MACRO）
--   T8      + 每行一次点查 SELECT INTO，带 WHEN OTHERS 兜底（语料里最常见的维表查找）
--   T8b     + 每行一次点查数值列（带 WHEN OTHERS 兜底），查表值直接写入
--   T2e     + T8b 之后再 IF … THEN 数值乘法覆写（byfund 区域 0 的「算术覆写门」形态）
--   T8n     + 每行一次点查 SELECT INTO，不带异常块
--   T9      + INSERT 之后每行一次按主键 UPDATE 另一张表
--   T10     + INSERT 之后每行一次 COMMIT
--   T4      嵌套：外层游标逐个 part，内层游标（相关子查询）逐条供应记录后 INSERT
CREATE OR REPLACE PROCEDURE prc_stair_t0 IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t0null IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, NULL, r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t0s IS
  v_pk    INTEGER;
  v_cost  NUMERIC(19,4);
  CURSOR c IS
    SELECT ps_partkey, ps_supplycost
      FROM partsupplier
     ORDER BY ps_partkey, ps_supplycost;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  OPEN c;
  LOOP
    FETCH c INTO v_pk, v_cost;
    EXIT WHEN c%NOTFOUND;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (v_pk, 'x', v_cost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  CLOSE c;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t0r IS
  CURSOR c IS
    SELECT ps_partkey, ps_supplycost
      FROM partsupplier
     ORDER BY ps_partkey, ps_supplycost;
  r  c%ROWTYPE;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  OPEN c;
  LOOP
    FETCH c INTO r;
    EXIT WHEN c%NOTFOUND;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  CLOSE c;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t0rf IS
  r  c%ROWTYPE;
  CURSOR c IS
    SELECT ps_partkey, ps_supplycost
      FROM partsupplier
     ORDER BY ps_partkey, ps_supplycost;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  OPEN c;
  LOOP
    FETCH c INTO r;
    EXIT WHEN c%NOTFOUND;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  CLOSE c;
  COMMIT;
END;
/
CREATE OR REPLACE PROCEDURE prc_stair_t2 IS
  v_tag  VARCHAR(2);
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    IF r.ps_supplycost > 100 THEN
      v_tag := 'H';
    ELSE
      v_tag := 'L';
    END IF;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, v_tag, r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t2b IS
  v_tag  VARCHAR(2) := 'L';
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    IF r.ps_supplycost > 100 THEN
      v_tag := 'H';
    END IF;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, v_tag, r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t2c IS
  v_tag  VARCHAR(2);
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    v_tag := 'L';
    IF r.ps_supplycost > 100 THEN
      v_tag := 'H';
    END IF;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, v_tag, r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t2d IS
  v_cost  NUMERIC(19,4);
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    v_cost := r.ps_supplycost;
    IF r.ps_supplycost > 100 THEN
      v_cost := r.ps_supplycost * 0.9;
    END IF;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', v_cost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t3 IS
  v_sum  NUMERIC(19,4) := 0;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    v_sum := v_sum + r.ps_supplycost;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', v_sum);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t7 IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, fnc_tag_simple(r.ps_supplycost), r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t8 IS
  v_name  supplier.s_name%TYPE;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_suppkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      SELECT s_name
        INTO v_name
        FROM supplier
       WHERE s_suppkey = r.ps_suppkey;
    EXCEPTION
      WHEN OTHERS THEN
        v_name := NULL;
    END;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, SUBSTR(v_name, 10, 2), r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t8n IS
  v_name  supplier.s_name%TYPE;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_suppkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    SELECT s_name
      INTO v_name
      FROM supplier
     WHERE s_suppkey = r.ps_suppkey;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, SUBSTR(v_name, 10, 2), r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t9 IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_suppkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
    UPDATE supplier
       SET s_acctbal = s_acctbal + r.ps_supplycost
     WHERE s_suppkey = r.ps_suppkey;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t10 IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', r.ps_supplycost);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
    COMMIT;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t4 IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR p IN (SELECT p_partkey, p_name
              FROM part
             ORDER BY p_partkey) LOOP
    FOR s IN (SELECT ps_supplycost
                FROM partsupplier
               WHERE ps_partkey = p.p_partkey
               ORDER BY ps_supplycost) LOOP
      BEGIN
        INSERT INTO result_stair(k, tag, cost)
        VALUES (p.p_partkey, SUBSTR(p.p_name, 6, 1), s.ps_supplycost);
      EXCEPTION
        WHEN OTHERS THEN
          NULL;
      END;
    END LOOP;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t8b IS
  v_acct  supplier.s_acctbal%TYPE;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_suppkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      SELECT s_acctbal
        INTO v_acct
        FROM supplier
       WHERE s_suppkey = r.ps_suppkey;
    EXCEPTION
      WHEN OTHERS THEN
        v_acct := 0;
    END;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', v_acct);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_t2e IS
  v_acct  supplier.s_acctbal%TYPE;
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_suppkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    BEGIN
      SELECT s_acctbal
        INTO v_acct
        FROM supplier
       WHERE s_suppkey = r.ps_suppkey;
    EXCEPTION
      WHEN OTHERS THEN
        v_acct := 0;
    END;
    IF r.ps_supplycost > 100 THEN
      v_acct := r.ps_supplycost * 0.9;
    END IF;
    BEGIN
      INSERT INTO result_stair(k, tag, cost)
      VALUES (r.ps_partkey, 'x', v_acct);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/
