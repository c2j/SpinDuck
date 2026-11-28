-- 参照表（oracle）：用来独立检验 SpinDuck 解释执行（Off）的结果是否正确，与集合化无关。
--   [论文] 取自论文给出的 SQL；
--   [手写] 由本包按同一语义手写等价 SQL。
-- run_probes.sh 在种子之后按 `;` 切分逐条执行。

-- ================= [论文] QBS 2013 图 3「Translated code」里的查询，逐字 =================
CREATE TABLE oracle_qbs_roleuser AS
SELECT u.user_id, u.role_id, u.user_name, r.role_name
  FROM users u, roles r
 WHERE u.role_id = r.role_id
 ORDER BY u.role_id, r.role_id;
