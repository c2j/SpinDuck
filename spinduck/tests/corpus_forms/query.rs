//! 查询形态（FROM/WHERE/JOIN/集合运算/分组）——逐行断言。

#![allow(dead_code)]

pub struct Q {
    pub key: &'static str,
    pub why: &'static str,
    pub sql: &'static str,
    pub want: &'static [&'static [&'static str]],
}

pub const QUERIES: &[Q] = &[
    Q {
        key: "head_select",
        why: "SELECT 列表 + WHERE + ORDER BY（语料 236 处）",
        sql: "SELECT fund, sec, qty FROM t_item WHERE kind = 'STOCK' ORDER BY id",
        // id=5 是 FUND，不在 STOCK 命中集内
        want: &[&["F01", "S001", "100"], &["F01", "S002", "200"], &["F02", "S003", "300"]],
    },
    Q {
        key: "from_multi_table",
        why: "逗号多表笛卡尔（语料 64 处）",
        sql: "SELECT COUNT(*) FROM t_item i, t_dim d",
        want: &[&["10"]],
    },
    Q {
        key: "join_implicit_where",
        why: "旧式 WHERE 等值连接",
        sql: "SELECT i.sec, d.name FROM t_item i, t_dim d WHERE i.sec = d.sec ORDER BY 1, 2",
        want: &[
            &["S001", "平安"],
            &["S001", "平安"],
            &["S002", "招港"],
            &["S002", "招港"],
        ],
    },
    Q {
        key: "join_explicit",
        why: "INNER JOIN … ON（语料 7 处）",
        sql: "SELECT COUNT(*) FROM t_item i JOIN t_dim d ON i.sec = d.sec",
        want: &[&["4"]],
    },
    Q {
        key: "join_left",
        why: "LEFT JOIN 保留未匹配行（语料 4 处）",
        sql: "SELECT i.sec, d.name FROM t_item i LEFT JOIN t_dim d ON i.sec = d.sec WHERE i.id = 4",
        want: &[&["S003", "NULL"]],
    },
    Q {
        key: "oracle_outer_join_marker",
        why: "(+) 外连接标记 → LEFT JOIN（语料 8 处）",
        sql: "SELECT i.sec, d.name FROM t_item i, t_dim d WHERE i.sec = d.sec(+) AND i.id = 4",
        want: &[&["S003", "NULL"]],
    },
    Q {
        key: "oracle_outer_join_marker",
        why: "语料原形：等值标记 + 区间双端标记同时落在末表（PKG_SPLIT_TRADE_STEP2.sql:168-175）",
        sql: "SELECT t.id, v.tax FROM t_item t, t_valid v \
              WHERE t.kind = 'STOCK' AND t.sec = v.sec(+) \
              AND t.bcrq BETWEEN v.begin_c(+) AND v.end_c(+) ORDER BY t.id",
        // id=2 区间不命中但标记侧补空保留；id=4 无维度行同样保留
        want: &[&["1", "0.001"], &["2", "NULL"], &["4", "NULL"]],
    },
    Q {
        key: "union_all",
        why: "UNION ALL 多段汇总（语料 45 处）",
        sql: "SELECT fund FROM t_item WHERE kind = 'STOCK' UNION ALL SELECT fund FROM t_item WHERE kind = 'BOND' ORDER BY 1",
        // STOCK 三段（F01, F01, F02）+ BOND 一段（F01）= 4 行，UNION ALL 不去重
        want: &[&["F01"], &["F01"], &["F01"], &["F02"]],
    },
    Q {
        key: "subquery_in",
        why: "IN (SELECT …)（语料 29 处）",
        sql: "SELECT COUNT(*) FROM t_item WHERE sec IN (SELECT sec FROM t_dim)",
        want: &[&["4"]],
    },
    Q {
        key: "exists",
        why: "EXISTS 相关子查询（语料 30 处）",
        sql: "SELECT COUNT(*) FROM t_item i WHERE EXISTS (SELECT 1 FROM t_dim d WHERE d.sec = i.sec)",
        want: &[&["4"]],
    },
    Q {
        key: "group_by",
        why: "GROUP BY + HAVING 前的分组聚合（语料 24 处）",
        sql: "SELECT fund, SUM(qty) FROM t_item GROUP BY fund ORDER BY 1",
        want: &[&["F01", "350"], &["F02", "300"]],
    },
    Q {
        key: "group_by_expression",
        why: "GROUP BY 表达式（按 kind 前缀归类，语料 6 处）",
        sql: "SELECT SUBSTR(kind, 1, 1) k, COUNT(*) FROM t_item GROUP BY SUBSTR(kind, 1, 1) ORDER BY 1",
        want: &[&["B", "1"], &["F", "1"], &["S", "3"]],
    },
    Q {
        key: "order_by",
        why: "ORDER BY 多列 + 方向（语料 24 处）",
        sql: "SELECT id, amt FROM t_item ORDER BY amt DESC NULLS LAST, id",
        want: &[&["3", "5000"], &["1", "1200"], &["4", "900"], &["2", "800"], &["5", "NULL"]],
    },
    Q {
        key: "distinct",
        why: "SELECT DISTINCT 去重",
        sql: "SELECT DISTINCT kind FROM t_item ORDER BY 1",
        want: &[&["BOND"], &["FUND"], &["STOCK"]],
    },
    Q {
        key: "derived_table",
        why: "派生表 JOIN",
        sql: "SELECT t.fund, t.c, d.total \
              FROM (SELECT fund, COUNT(*) c FROM t_item GROUP BY fund) t \
              JOIN (SELECT fund, SUM(qty) total FROM t_item GROUP BY fund) d ON t.fund = d.fund \
              ORDER BY 1",
        want: &[&["F01", "3", "350"], &["F02", "2", "300"]],
    },
    Q {
        key: "sql_with_cte",
        why: "WITH 子句多引用",
        sql: "WITH s AS (SELECT fund, qty FROM t_item WHERE kind = 'STOCK') SELECT fund, SUM(qty) FROM s GROUP BY fund ORDER BY 1",
        want: &[&["F01", "300"], &["F02", "300"]],
    },
];
