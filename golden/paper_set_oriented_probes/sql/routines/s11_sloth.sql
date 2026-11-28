-- 探针 S11 —— Sloth「Being Lazy Is a Virtue (When Issuing Database Queries)」（Cheung 等，SIGMOD 2014）图 1。
-- 原文是 OpenMRS 控制器里的一段 Java：getPatient 一次查询，随后 getEncounters / getVisits / getActiveVisits 三次查询，
-- 后三次的结果只是放进模型。Sloth 把它们推迟并合成一次数据库往返（原文：页面加载时间中位数快 1.3–2.2 倍）。
-- 这里写成库侧的等价 PL/SQL：四个先后执行的点查，结果写进一张结果表（按论文描述的改写，不是逐字代码）。
-- 注意：Sloth 优化的是应用与数据库之间的网络往返；SpinDuck 是进程内引擎，没有这项成本。
CREATE OR REPLACE PROCEDURE prc_sloth_dashboard IS
  v_name  patient.name%TYPE;
  v_enc   INTEGER;
  v_vis   INTEGER;
  v_act   INTEGER;
BEGIN
  DELETE FROM result_dashboard;
  COMMIT;
  FOR p IN (SELECT patient_id FROM patient ORDER BY patient_id) LOOP
    BEGIN
      SELECT name
        INTO v_name
        FROM patient
       WHERE patient_id = p.patient_id;
      SELECT COUNT(*)
        INTO v_enc
        FROM encounter
       WHERE patient_id = p.patient_id;
      SELECT COUNT(*)
        INTO v_vis
        FROM visit
       WHERE patient_id = p.patient_id;
      SELECT COUNT(*)
        INTO v_act
        FROM visit
       WHERE patient_id = p.patient_id
         AND active = 1;
      INSERT INTO result_dashboard(patient_id, patient_name, encounters, visits, active_visits)
      VALUES (p.patient_id, v_name, v_enc, v_vis, v_act);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/
