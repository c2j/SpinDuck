-- 探针 Q10 —— QBS「Optimizing Database-Backed Applications with Query Synthesis」（Cheung 等，PLDI 2013）图 1。
-- 原文是 Java（Hibernate）：getRoleUser() 取出全部用户与角色，两层循环按 roleId 相等收集用户——一个用嵌套循环写的连接。
-- SpinDuck 的输入是库侧 PL/SQL，这里写成库侧的等价游标循环（按论文代码的改写，不是逐字）。
-- 论文综合出的查询（图 3）：SELECT u.* FROM users u, roles r WHERE u.roleId == r.roleId ORDER BY u.roleId, r.roleId
-- ——见 cases/oracle.sql 的 oracle_qbs_roleuser，用来检验 Off 的结果。
-- 论文 §2：内核语言「不建模 SQL 的 NULL 三值逻辑，也不建模对数据库的更新」，只处理只读循环。
CREATE OR REPLACE PROCEDURE prc_qbs_getroleuser IS
BEGIN
  DELETE FROM result_role_user;
  COMMIT;
  FOR u IN (SELECT user_id, role_id, user_name FROM users) LOOP
    FOR r IN (SELECT role_id, role_name FROM roles) LOOP
      IF u.role_id = r.role_id THEN
        INSERT INTO result_role_user(user_id, role_id, user_name, role_name)
        VALUES (u.user_id, r.role_id, u.user_name, r.role_name);
      END IF;
    END LOOP;
  END LOOP;
  COMMIT;
END;
/
