-- TPC-H 形状的表结构（Hirn & Grust「One WITH RECURSIVE is Worth Many GOTOs」案例集里
-- 8 个基于 TPC-H 的函数所需）。列集与类型沿用 TPC-H 的 dss.ddl，写法用本包统一的
-- GaussDB 风格（与 golden/paper_set_oriented_probes/sql/ddl 一致）。
-- 注意：这是独立的探针工程，表名与主工程里的最小列子集表（orders、customer 等）不共用。
-- 丢弃顺序无外键依赖，顺序只为幂等重建。

DROP TABLE IF EXISTS result_q3;
DROP TABLE IF EXISTS result_prism_hoist;
DROP TABLE IF EXISTS result_goto_savings;
DROP TABLE IF EXISTS result_goto_margin;
DROP TABLE IF EXISTS result_goto_late;
DROP TABLE IF EXISTS result_goto_global;
DROP TABLE IF EXISTS result_goto_ship;
DROP TABLE IF EXISTS result_dual;
DROP TABLE IF EXISTS result_goto_service;

DROP TABLE IF EXISTS oracle_prism_hoist;
DROP TABLE IF EXISTS oracle_q3;
DROP TABLE IF EXISTS oracle_goto_savings;
DROP TABLE IF EXISTS oracle_goto_margin;
DROP TABLE IF EXISTS oracle_goto_late;
DROP TABLE IF EXISTS oracle_goto_global;
DROP TABLE IF EXISTS oracle_goto_ship;
DROP TABLE IF EXISTS oracle_goto_service;

DROP TABLE IF EXISTS lineitem;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS partsupp;
DROP TABLE IF EXISTS supplier;
DROP TABLE IF EXISTS customer;
DROP TABLE IF EXISTS part;
DROP TABLE IF EXISTS nation;
DROP TABLE IF EXISTS region;

CREATE TABLE nation (
    n_nationkey  INTEGER NOT NULL,
    n_name       CHAR(25) NOT NULL,
    n_regionkey  INTEGER NOT NULL,
    n_comment    VARCHAR(152)
);

CREATE TABLE region (
    r_regionkey  INTEGER NOT NULL,
    r_name       CHAR(25) NOT NULL,
    r_comment    VARCHAR(152)
);

CREATE TABLE part (
    p_partkey     INTEGER NOT NULL,
    p_name        VARCHAR(55) NOT NULL,
    p_mfgr        CHAR(25) NOT NULL,
    p_brand       CHAR(10) NOT NULL,
    p_type        VARCHAR(25) NOT NULL,
    p_size        INTEGER NOT NULL,
    p_container   CHAR(10) NOT NULL,
    p_retailprice DECIMAL(15,2) NOT NULL,
    p_comment     VARCHAR(23) NOT NULL
);

CREATE TABLE supplier (
    s_suppkey     INTEGER NOT NULL,
    s_name        CHAR(25) NOT NULL,
    s_address     VARCHAR(40) NOT NULL,
    s_nationkey   INTEGER NOT NULL,
    s_phone       CHAR(15) NOT NULL,
    s_acctbal     DECIMAL(15,2) NOT NULL,
    s_comment     VARCHAR(101) NOT NULL
);

CREATE TABLE partsupp (
    ps_partkey     INTEGER NOT NULL,
    ps_suppkey     INTEGER NOT NULL,
    ps_availqty    INTEGER NOT NULL,
    ps_supplycost  DECIMAL(15,2) NOT NULL,
    ps_comment     VARCHAR(199) NOT NULL
);

CREATE TABLE customer (
    c_custkey     INTEGER NOT NULL,
    c_name        VARCHAR(25) NOT NULL,
    c_address     VARCHAR(40) NOT NULL,
    c_nationkey   INTEGER NOT NULL,
    c_phone       CHAR(15) NOT NULL,
    c_acctbal     DECIMAL(15,2) NOT NULL,
    c_mktsegment  CHAR(10) NOT NULL,
    c_comment     VARCHAR(117) NOT NULL
);

CREATE TABLE orders (
    o_orderkey       INTEGER NOT NULL,
    o_custkey        INTEGER NOT NULL,
    o_orderstatus    CHAR(1) NOT NULL,
    o_totalprice     DECIMAL(15,2) NOT NULL,
    o_orderdate      DATE NOT NULL,
    o_orderpriority  CHAR(15) NOT NULL,
    o_clerk          CHAR(15) NOT NULL,
    o_shippriority   INTEGER NOT NULL,
    o_comment        VARCHAR(79) NOT NULL
);

CREATE TABLE lineitem (
    l_orderkey      INTEGER NOT NULL,
    l_partkey       INTEGER NOT NULL,
    l_suppkey       INTEGER NOT NULL,
    l_linenumber    INTEGER NOT NULL,
    l_quantity      DECIMAL(15,2) NOT NULL,
    l_extendedprice DECIMAL(15,2) NOT NULL,
    l_discount      DECIMAL(15,2) NOT NULL,
    l_tax           DECIMAL(15,2) NOT NULL,
    l_returnflag    CHAR(1) NOT NULL,
    l_linestatus    CHAR(1) NOT NULL,
    l_shipdate      DATE NOT NULL,
    l_commitdate    DATE NOT NULL,
    l_receiptdate   DATE NOT NULL,
    l_shipinstruct  CHAR(25) NOT NULL,
    l_shipmode      CHAR(10) NOT NULL,
    l_comment       VARCHAR(44) NOT NULL
);

-- ---------- 结果表与参照表 ----------

CREATE TABLE result_goto_service (c_custkey INTEGER, service_level VARCHAR(20));
CREATE TABLE result_goto_ship (c_custkey INTEGER, shipmode VARCHAR(10));
CREATE TABLE result_goto_margin (p_partkey INTEGER, buy_orderkey INTEGER, sell_orderkey INTEGER, margin DECIMAL(38,10));
CREATE TABLE result_goto_global (l_orderkey INTEGER, long_distance INTEGER);
CREATE TABLE result_goto_late (s_name CHAR(25), o_orderkey INTEGER, kept_waiting INTEGER);
CREATE TABLE result_goto_savings (o_orderkey INTEGER, savings_pct DECIMAL(38,10), changes INTEGER);
CREATE TABLE result_q3 (l_orderkey INTEGER, l_linenumber INTEGER, q3 INTEGER, disc_price DECIMAL(38,6));
CREATE TABLE result_prism_hoist (l_orderkey INTEGER, l_linenumber INTEGER);
CREATE TABLE result_dual (n INTEGER);
