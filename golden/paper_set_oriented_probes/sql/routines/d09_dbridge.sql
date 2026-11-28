-- 探针 D09.* —— DBridge「Translating Imperative Code to SQL」（Emani 等，SIGMOD 2017 演示论文）§2.1 的能力清单。
-- 原文案例是应用侧的 Java 方法（JDBC/Hibernate），如图 1 的 getUnfinishedProjects。SpinDuck 的输入是库侧 PL/SQL，
-- 所以这里把论文列出的每种「用命令式代码表达的关系操作」写成库侧的等价游标循环：
--   应用侧「循环里逐行判断再收集」 = 库侧「游标循环里 IF 再 INSERT」。
-- 这是按论文描述的改写，不是逐字代码。论文给出的改写结果写在各过程上方。

-- 图 1 getUnfinishedProjects：过滤。论文改写为 select * from Project where isFinished <> 1
-- （原文实测：10 万行、20% 选择率时快 2.3 倍、传输量少 2.5 倍）。
CREATE OR REPLACE PROCEDURE prc_dbridge_filter IS
BEGIN
  DELETE FROM result_unfinished;
  COMMIT;
  FOR p IN (SELECT project_id, is_finished FROM project) LOOP
    IF p.is_finished <> 1 THEN
      INSERT INTO result_unfinished(project_id)
      VALUES (p.project_id);
    END IF;
  END LOOP;
  COMMIT;
END;
/

-- §2.1「Other operations：checking for existence of a tuple using a loop over query results」
-- 论文改写为 EXISTS 子查询。
CREATE OR REPLACE FUNCTION fnc_dbridge_exists(p_project_id IN INTEGER) RETURN INTEGER IS
  v_found  INTEGER := 0;
BEGIN
  FOR r IN (SELECT 1 AS n FROM project WHERE project_id = p_project_id) LOOP
    v_found := 1;
    EXIT;
  END LOOP;
  RETURN v_found;
END;
/

CREATE OR REPLACE PROCEDURE prc_dbridge_exists IS
BEGIN
  DELETE FROM result_exists;
  COMMIT;
  FOR i IN 1 .. 5 LOOP
    BEGIN
      INSERT INTO result_exists(n)
      VALUES (fnc_dbridge_exists(i));
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/
