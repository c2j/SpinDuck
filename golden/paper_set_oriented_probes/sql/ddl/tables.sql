-- 论文探针表结构（§12.1 项目模式：被测侧与参照侧都建）。
-- 论文案例所用 TPC-H / TPC-DS 的最小列子集 + 结果表 + 桩表；类型沿用 golden/case_split_agg 的写法。
-- 丢弃顺序：结果表 → 桩表 → 源表（无外键，顺序只为幂等重建）。

DROP TABLE IF EXISTS result_xfile;
DROP TABLE IF EXISTS result_stair;
DROP TABLE IF EXISTS result_exists;
DROP TABLE IF EXISTS result_unfinished;
DROP TABLE IF EXISTS result_role_user;
DROP TABLE IF EXISTS result_dashboard;
DROP TABLE IF EXISTS oracle_qbs_roleuser;
DROP TABLE IF EXISTS result_walk;
DROP TABLE IF EXISTS result_manufact;
DROP TABLE IF EXISTS result_total;
DROP TABLE IF EXISTS result_from_tmp;
DROP TABLE IF EXISTS result_seq;
DROP TABLE IF EXISTS result_nested;
DROP TABLE IF EXISTS result_running;
DROP TABLE IF EXISTS result_cum_roi;
DROP TABLE IF EXISTS result_min_supp;

DROP TABLE IF EXISTS walk_params;
DROP TABLE IF EXISTS policy;
DROP TABLE IF EXISTS actions;
DROP TABLE IF EXISTS states;
DROP TABLE IF EXISTS visit;
DROP TABLE IF EXISTS encounter;
DROP TABLE IF EXISTS patient;
DROP TABLE IF EXISTS roles;
DROP TABLE IF EXISTS users;
DROP TABLE IF EXISTS project;
DROP TABLE IF EXISTS best_promos_catalog;
DROP TABLE IF EXISTS stage_agg;

DROP TABLE IF EXISTS web_sales_history;
DROP TABLE IF EXISTS store_sales_history;
DROP TABLE IF EXISTS catalog_sales_history;
DROP TABLE IF EXISTS web_returns;
DROP TABLE IF EXISTS catalog_returns;
DROP TABLE IF EXISTS web_sales;
DROP TABLE IF EXISTS catalog_sales;
DROP TABLE IF EXISTS reason;
DROP TABLE IF EXISTS promotion;
DROP TABLE IF EXISTS item;
DROP TABLE IF EXISTS date_dim;

DROP TABLE IF EXISTS xchg;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS customer_prefs;
DROP TABLE IF EXISTS customer;
DROP TABLE IF EXISTS txns;
DROP TABLE IF EXISTS monthly_investments;
DROP TABLE IF EXISTS partsupplier;
DROP TABLE IF EXISTS supplier;
DROP TABLE IF EXISTS part;

-- ---------- 结果表：每个探针写自己的结果，供 OFF / 集合化两侧逐值比对 ----------

CREATE TABLE result_min_supp (
    p_partkey   INTEGER,
    supp_name   VARCHAR(100)
);

CREATE TABLE result_cum_roi (
    investor    VARCHAR(10),
    cum_roi     NUMERIC(19,6)
);

CREATE TABLE result_running (
    tx_id       INTEGER,
    acct        VARCHAR(10),
    amt         NUMERIC(19,4),
    bal         NUMERIC(19,4)
);

CREATE TABLE result_nested (
    p_partkey   INTEGER,
    p_name      VARCHAR(100),
    n_suppliers INTEGER,
    total_cost  NUMERIC(19,4)
);

CREATE TABLE result_seq (
    i           INTEGER,
    sq          INTEGER
);

CREATE TABLE result_from_tmp (
    k           INTEGER,
    v10         NUMERIC(19,4)
);

CREATE TABLE result_total (
    c_custkey   INTEGER,
    c_name      VARCHAR(100),
    total_txt   VARCHAR(100)
);

CREATE TABLE result_manufact (
    ws_item_sk  INTEGER,
    manufact    CHAR(50)
);

CREATE TABLE result_stair (
    k           INTEGER,
    tag         VARCHAR(2),
    cost        NUMERIC(19,4)
);

CREATE TABLE result_xfile (
    fx_from     CHAR(3),
    rate        NUMERIC(19,6)
);

-- E1：walk(start_state, success_at, failure_at, max_steps) 的返回值（论文里叫 steps）
CREATE TABLE result_walk (
    start_state  INTEGER,
    success_at   INTEGER,
    failure_at   INTEGER,
    max_steps    INTEGER,
    steps        INTEGER
);

CREATE TABLE result_exists (
    n           INTEGER
);

-- D09：DBridge 2017 图 1 getUnfinishedProjects 的过滤结果
CREATE TABLE result_unfinished (
    project_id   INTEGER
);

-- Q10：QBS 2013 图 1 getRoleUser 的连接结果
CREATE TABLE result_role_user (
    user_id      INTEGER,
    role_id      INTEGER,
    user_name    VARCHAR(50),
    role_name    VARCHAR(50)
);

-- oracle_qbs_roleuser 由 cases/oracle.sql 以 CREATE TABLE … AS SELECT 创建，这里只保留丢弃语句。

-- S11：Sloth 2014 图 1 患者面板的四个查询结果
CREATE TABLE result_dashboard (
    patient_id    INTEGER,
    patient_name  VARCHAR(50),
    encounters    INTEGER,
    visits        INTEGER,
    active_visits INTEGER
);

-- ---------- TPC-H 子集：Aggify 图 1（partsupplier / supplier / part）与图 2 ----------

CREATE TABLE part (
    p_partkey   INTEGER NOT NULL,
    p_name      VARCHAR(100),
    p_mfgr      VARCHAR(50)
);

CREATE TABLE supplier (
    s_suppkey   INTEGER NOT NULL,
    s_name      VARCHAR(100),
    s_acctbal   NUMERIC(19,4)
);

CREATE TABLE partsupplier (
    ps_partkey  INTEGER NOT NULL,
    ps_suppkey  INTEGER NOT NULL,
    ps_supplycost NUMERIC(19,4) NOT NULL,
    ps_availqty INTEGER
);

CREATE TABLE monthly_investments (
    investor    VARCHAR(10) NOT NULL,
    m_month     VARCHAR(6) NOT NULL,
    roi         NUMERIC(19,6) NOT NULL
);

-- A3：按账户有序流水（游标带 ORDER BY，循环内维护运行余额）
CREATE TABLE txns (
    tx_id       INTEGER NOT NULL,
    acct        VARCHAR(10) NOT NULL,
    amt         NUMERIC(19,4) NOT NULL
);

-- A6：循环体写入的中间表（Aggify：DML on local table variables / temp tables）
CREATE TABLE stage_agg (
    k           INTEGER,
    v           NUMERIC(19,4)
);

-- ---------- TPC-DS 子集：SQL-ProcBench / CIDR 2024 / PRISM 的 getManufact 族 ----------

CREATE TABLE date_dim (
    d_date_sk   INTEGER NOT NULL,
    d_year      INTEGER NOT NULL,
    d_moy       INTEGER
);

CREATE TABLE item (
    i_item_sk   INTEGER NOT NULL,
    i_manufact  CHAR(50),
    i_class     CHAR(50)
);

CREATE TABLE store_sales_history (
    ss_sold_date_sk INTEGER,
    ss_item_sk      INTEGER
);

CREATE TABLE catalog_sales_history (
    cs_sold_date_sk INTEGER,
    cs_item_sk      INTEGER
);

CREATE TABLE web_sales (
    ws_item_sk      INTEGER,
    ws_order_number INTEGER,
    ws_net_paid_inc_ship_tax NUMERIC(7,2)
);

CREATE TABLE catalog_sales (
    cs_order_number INTEGER,
    cs_item_sk      INTEGER,
    cs_net_paid_inc_ship_tax NUMERIC(7,2)
);

CREATE TABLE web_returns (
    wr_order_number INTEGER,
    wr_item_sk      INTEGER,
    wr_refunded_cash NUMERIC(7,2),
    wr_reason_sk    INTEGER
);

CREATE TABLE catalog_returns (
    cr_order_number INTEGER,
    cr_item_sk      INTEGER,
    cr_refunded_cash NUMERIC(7,2),
    cr_reason_sk    INTEGER,
    cr_returned_date_sk INTEGER
);

CREATE TABLE reason (
    r_reason_sk INTEGER,
    r_reason_id VARCHAR(16),
    r_reason_desc VARCHAR(100)
);

CREATE TABLE promotion (
    p_promo_sk  INTEGER NOT NULL,
    p_end_date_sk INTEGER,
    p_discount_active CHAR(1)
);

-- D2'：原基准的游标源是表值函数 bestPromosCatalog()（其 PL/SQL 版依赖 BULK COLLECT，
-- 引擎不支持），这里换成同名语义的普通表，见 procbench/README 说明。
CREATE TABLE best_promos_catalog (
    promo_sk    INTEGER
);

CREATE TABLE web_sales_history (
    ws_sold_date_sk INTEGER,
    ws_bill_customer_sk INTEGER,
    ws_net_paid_inc_ship_tax NUMERIC(7,2)
);

-- 两种列名并存：TPC-H（Froid 图 1）叫 c_custkey，TPC-DS（SQL-ProcBench）叫 c_customer_sk。
CREATE TABLE customer (
    c_custkey     INTEGER NOT NULL,
    c_customer_sk INTEGER NOT NULL,
    c_name        VARCHAR(100)
);

-- ---------- Froid 图 1：total_price / xchg_rate ----------

-- Froid 图 1：customer_prefs(custkey, currency)，列名与论文一致。
CREATE TABLE customer_prefs (
    custkey     INTEGER NOT NULL,
    currency    CHAR(3)
);

CREATE TABLE orders (
    o_orderkey  INTEGER NOT NULL,
    o_custkey   INTEGER NOT NULL,
    o_totalprice NUMERIC(19,2)
);

-- Froid 图 1：dbo.xchg(from_cur, to_cur, rate)，列名与论文一致。
CREATE TABLE xchg (
    from_cur    CHAR(3),
    to_cur      CHAR(3),
    rate        NUMERIC(19,6)
);

-- ---------- Hirn & Grust：walk() 的马尔可夫决策过程 ----------
-- 表名与列名取自案例集 markov/markov-preparation.sql（states、policy、actions）；
-- 案例集用随机数生成 MDP，这里换成手工给定的 3 个状态、每状态 2 个动作。

CREATE TABLE states (
    id          INTEGER NOT NULL,
    name        VARCHAR(20),
    r           DECIMAL(10,2)
);

CREATE TABLE policy (
    state_id    INTEGER NOT NULL,
    action_name VARCHAR(10)
);

CREATE TABLE actions (
    id          INTEGER NOT NULL,
    name        VARCHAR(10),
    s_from      INTEGER NOT NULL,
    s_to        INTEGER NOT NULL,
    p           DECIMAL(5,4)
);

-- E1 的驱动参数：每个状态机一组 (start_state, success_at, failure_at, max_steps)
CREATE TABLE walk_params (
    start_state INTEGER,
    success_at  INTEGER,
    failure_at  INTEGER,
    max_steps   INTEGER
);

-- ---------- DBridge 2017 图 1：项目表（isFinished） ----------

CREATE TABLE project (
    project_id  INTEGER NOT NULL,
    is_finished INTEGER NOT NULL,
    name        VARCHAR(50)
);

-- ---------- QBS 2013 图 1：users / roles ----------

CREATE TABLE users (
    user_id     INTEGER NOT NULL,
    role_id     INTEGER,
    user_name   VARCHAR(50)
);

CREATE TABLE roles (
    role_id     INTEGER NOT NULL,
    role_name   VARCHAR(50)
);

-- ---------- Sloth 2014 图 1：患者、就诊、随访 ----------

CREATE TABLE patient (
    patient_id  INTEGER NOT NULL,
    name        VARCHAR(50)
);

CREATE TABLE encounter (
    encounter_id INTEGER NOT NULL,
    patient_id   INTEGER NOT NULL
);

CREATE TABLE visit (
    visit_id    INTEGER NOT NULL,
    patient_id  INTEGER NOT NULL,
    active      INTEGER NOT NULL
);
