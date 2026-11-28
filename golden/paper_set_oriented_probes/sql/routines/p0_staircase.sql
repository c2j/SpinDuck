-- 阶梯探针（非规范基底）—— 非论文案例，用来给矩阵定标。
-- 业务相同：游标逐行写 result_stair。这一组 INSERT 外面**没有**跳行异常块，
-- 用来证明「跳行 trap 是集合化批写的硬前提」：引擎只在 BatchAtomic 且循环体带 trap
-- 时才允许过滤（analyze.rs `allow_filter = BatchAtomic && has_trap`），否则需要
-- 值域守卫的投影一律 UnsupportedWrite。
--
--   写法后缀   .s = FETCH c INTO v1, v2（标量目标列表，Oracle 显式游标最常见写法）
--              .r = FETCH c INTO r（%ROWTYPE 整行记录）
--              .f = FOR r IN (SELECT …) LOOP（隐式游标）
--   r2 / f2   投影里没有 NULL 字面量（排除「NULL 字面量需要守卫」这个干扰项）
-- 规范基底（trap 内 INSERT）见 t0_canonical.sql。
CREATE OR REPLACE FUNCTION fnc_tag_simple(p IN NUMBER) RETURN VARCHAR2 IS
BEGIN
  RETURN CASE WHEN p > 100 THEN 'H' ELSE 'L' END;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_p0s IS
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
    INSERT INTO result_stair(k, tag, cost)
    VALUES (v_pk, NULL, v_cost);
  END LOOP;
  CLOSE c;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_p0r IS
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
    INSERT INTO result_stair(k, tag, cost)
    VALUES (r.ps_partkey, NULL, r.ps_supplycost);
  END LOOP;
  CLOSE c;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_p0r2 IS
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
    INSERT INTO result_stair(k, tag, cost)
    VALUES (r.ps_partkey, fnc_tag_simple(r.ps_supplycost), r.ps_supplycost);
  END LOOP;
  CLOSE c;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_p0f IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    INSERT INTO result_stair(k, tag, cost)
    VALUES (r.ps_partkey, NULL, r.ps_supplycost);
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_stair_p0f2 IS
BEGIN
  DELETE FROM result_stair;
  COMMIT;
  FOR r IN (SELECT ps_partkey, ps_supplycost
              FROM partsupplier
             ORDER BY ps_partkey, ps_supplycost) LOOP
    INSERT INTO result_stair(k, tag, cost)
    VALUES (r.ps_partkey, fnc_tag_simple(r.ps_supplycost), r.ps_supplycost);
  END LOOP;
  COMMIT;
END;
/
