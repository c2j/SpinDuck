//! 临时探针：验证 DuckDB 下 `ORDER BY ... LIMIT 1` 与
//! `ROW_NUMBER() OVER (ORDER BY ...)` 在并列键上的取行是否一致。
//! 结论决定 Top-1 歧义守卫能否放宽为「运行时取行等价校验」。
#![cfg(feature = "native")]

use spinduck_core::exec::SqlExecutor;
use spinduck_core::{BindMap, DuckExecutor, EngineError};

fn q(ex: &DuckExecutor, sql: &str) -> Result<Vec<Vec<String>>, EngineError> {
    let mut s = ex.query(sql, &BindMap::new())?;
    Ok(s.fetch_many(usize::MAX)
        .iter()
        .map(|r| r.iter().map(|v| format!("{v:?}")).collect())
        .collect())
}

#[test]
fn probe_top1_pick_equivalence() {
    let ex = DuckExecutor::open_in_memory().unwrap();
    // 大规模 + 各类并列：数值键、字符串多列排序、NULL、三元以上并列
    ex.execute(
        "CREATE TABLE t AS SELECT * FROM (VALUES \
         (1,2,'a','10'),(1,2,'b','20'),(1,2,'c','30'),(1,1,'d','40'),\
         (2,5,'x','1'),(2,5,'y','2'),(2,5,'z','3'),(2,4,'w','4'),\
         (3,9,'p',NULL),(3,9,'q',NULL),(3,9,'r','5'),\
         (4,7,'m','1'),(4,7,'n','1'),(4,7,'o','1'),(4,7,'p2','2'),\
         (5,3,'s1','u'),(5,3,'s2','v'),(5,2,'s3','w'),(5,3,'s4','x')) AS v(k,o,c,x)",
        &BindMap::new(),
    )
    .unwrap();
    // 形态 A：窗口 ROW_NUMBER（SO 窗口形态）
    let win = q(
        &ex,
        "SELECT k, c, x FROM (SELECT k,c,x, ROW_NUMBER() OVER (PARTITION BY k ORDER BY o DESC) rn FROM t) WHERE rn=1 ORDER BY k",
    )
    .unwrap();
    // 形态 B：逐键相关 LIMIT 1（SO lateral / 串行 rownum→LIMIT 形态）
    let lim = q(
        &ex,
        "SELECT k, l.c, l.x FROM (SELECT DISTINCT k FROM t) t \
         JOIN LATERAL (SELECT c, x FROM t t2 WHERE t2.k=t.k ORDER BY o DESC LIMIT 1) l ON TRUE \
         ORDER BY k",
    )
    .unwrap();
    eprintln!("win={win:?}\nlim={lim:?}");
    // lim 返回 (k, c, x)；win 返回 (k, c, x)
    assert_eq!(win.len(), lim.len());
    for (w, l) in win.iter().zip(lim.iter()) {
        assert_eq!(w[0], l[0], "key diverge");
        assert_eq!(w[1], l[1], "pick c diverge at {w:?} vs {l:?}");
        assert_eq!(w[2], l[2], "pick x diverge at {w:?} vs {l:?}");
    }
}

/// R1 前提探针：同一 GROUP BY（无 ORDER BY）查询两次执行，
/// 行序是否一致——决定 sd_cur 重查行序 == 游标流行序假设是否成立。
#[test]
fn probe_group_by_order_stability() {
    let ex = DuckExecutor::open_in_memory().unwrap();
    ex.execute(
        "CREATE TABLE t2 AS SELECT (random()*1000)::INT % 50 AS g, (random()*100)::INT AS v \
         FROM range(5000) r(i)",
        &BindMap::new(),
    )
    .unwrap();
    let sql = "SELECT g, SUM(v) s, COUNT(*) c FROM t2 GROUP BY g";
    let a = q(&ex, sql).unwrap();
    let b = q(&ex, sql).unwrap();
    assert_eq!(a.len(), b.len());
    let same = a.iter().zip(b.iter()).all(|(x, y)| x == y);
    eprintln!("order_stable={same} first={:?}", &a[..3.min(a.len())]);
    assert!(same, "GROUP BY 无 ORDER BY 两次执行行序不一致");
}
