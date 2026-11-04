#!/usr/bin/env python3
"""
SpinDuck 设计验证：按 docs/architecture.md 的设计管线，模拟运行
PKG_SPLIT_TRADE_STEP3_SH.prc_trd_hz_byfund(p_i_date, p_o_succeed)。

- DuckDB(真实内核) 承载业务表（语料 DDL 原样建表，仅剥离 ::character varying）
- PL 函数按 M6.5 设计注册为 DuckDB scalar UDF（SQL 部分用第二连接）
- 解释器循环用 Python 模拟：变量帧 dict + 游标状态机 + 异常 handler
- 每条 SQL 按"改写器"编译期产物形态降级为 DuckDB SQL
"""
import duckdb, json, re, pathlib, subprocess

CORPUS = pathlib.Path(__file__).parent.parent / "lib/fastaas/exam/清算拆分优化考题/基线代码"
OGSQL = pathlib.Path(__file__).parent.parent / "lib/ogsql-parser/target/release/ogsql"
P_I_DATE = "20240105"

try:
    from duckdb.sqltypes import VARCHAR      # DuckDB ≥1.5
except ImportError:
    from duckdb.typing import VARCHAR        # DuckDB ≤1.4 兼容

db = duckdb.connect(":memory:")
main = db.cursor()      # 主连接（解释器持有）
udfconn = db.cursor()   # UDF 第二连接（M6.5 设计约束）

# ── 1. 建表：语料 DDL 原样 + 改写规则 R1（剥离 ::character varying，实测必需）──
def load_table(name):
    sql = (CORPUS / "1_表结构" / f"{name}.sql").read_text()
    sql = re.sub(r"::character varying", "", sql)
    main.execute(sql)

for t in ["mid_yjqs_detail","dat_fund_cjqs","dat_xw_venture",
          "dat_fund_xw_venture","jyfy_temp","qfqscjhbrbb_temp"]:
    load_table(t)

main.execute("""CREATE TABLE par_oper_venture (stock_kind_str VARCHAR, operation_kind VARCHAR, market_code VARCHAR)""")
main.execute("""CREATE TABLE v_par_fund_check_para (fund_code VARCHAR, venture_type VARCHAR, is_venture VARCHAR,
                inure_begin_date VARCHAR, inure_end_date VARCHAR)""")
main.execute("""CREATE TABLE v_par_bond (security_id VARCHAR, bond_kind VARCHAR,
                inure_begin_date VARCHAR, inure_end_date VARCHAR)""")

# ── 2. 样例数据 ──
cols = [r[0] for r in main.execute(
    "SELECT column_name FROM information_schema.columns WHERE table_name='mid_yjqs_detail' "
    "ORDER BY ordinal_position").fetchall()]
notnull = {r[0] for r in main.execute(
    "SELECT column_name FROM information_schema.columns WHERE table_name='mid_yjqs_detail' "
    "AND is_nullable='NO'").fetchall()}
def row(**kw):
    r = {c: None for c in cols}
    r.update(bcrq=P_I_DATE, scdm="001", is_yysg="0", ywlb="00")
    for c in notnull:                      # NOT NULL 列默认 0（数值），可被 kw 覆盖
        if r[c] is None: r[c] = 0
    r.update(kw); return r
rows = [
    row(fund_code="", xwdm="X001", main_xwdm="", security_id="600000", bs="1", trade_type="T1",
        cjsl=100, cjje=1000.00, yhs=1.0, jsf=2.0, jiesf=0, zgf=1.0, ghf=0.5, ghf_dealer=0.2,
        fxj=10.0, accrual=1.0, accrual2=0.5, yj01=5.0, pay_type="0", stock_kind="0100",
        check_type="CT", date_source="DS", cjbz="CB", zqdm="600000", gddm="G001", sub_gddm="G001"),
    row(fund_code="", xwdm="X002", main_xwdm="", security_id="510300", bs="-1", trade_type="T2",
        cjsl=200, cjje=2000.00, yhs=2.0, jsf=1.0, jiesf=0, zgf=0.5, ghf=0.3, ghf_dealer=0.1,
        fxj=20.0, accrual=2.0, accrual2=1.0, yj02=8.0, pay_type="0", stock_kind="3500",
        check_type="CT", date_source="DS", cjbz="CB", zqdm="510300", gddm="G002", sub_gddm="G002"),
    row(fund_code="", xwdm="X003", main_xwdm="", security_id="609500", bs="1", trade_type="T3",
        cjsl=50, cjje=500.00, yhs=0.5, jsf=9.9, jiesf=0, zgf=0.1, ghf=0.1, ghf_dealer=0.0,
        fxj=5.0, accrual=0.2, accrual2=0.1, yj01=1.0, pay_type="0", stock_kind="0100",
        check_type="CT", date_source="DS", cjbz="CB", zqdm="609500", gddm="G003", sub_gddm="G003"),
]
main.executemany(f"INSERT INTO mid_yjqs_detail VALUES ({','.join(['?']*len(cols))})",
                 [tuple(r[c] for c in cols) for r in rows])
main.execute("INSERT INTO par_oper_venture VALUES ('stock_kind in (''0100'')','OP1','001')")
main.execute("""INSERT INTO v_par_fund_check_para VALUES
  ('F0001','1','1','20200101','20991231'), ('F0002','0','0','20200101','20991231')""")
main.execute("INSERT INTO v_par_bond VALUES ('600000','0100','20200101','20991231'),"
             "('009999','0200','20200101','20991231')")  # 609500 → NO_DATA_FOUND 分支；600000 走 INSERT 路径
main.execute(f"INSERT INTO jyfy_temp (zqdm, gddm, data_date, bs, jydy, jyfy) "
             f"VALUES ('609500','G003','{P_I_DATE}','1','X003',3.3)")

# ── 3. M6.5：PL 函数 → scalar UDF（函数体内 SQL 走第二连接）──
main.execute("SET threads=1")   # 实证：UDF 重入需单线程（见设计修订 R-M6.5）
def fnc_com_get_fund_code(gddm, sub_gddm, dt, xwdm):
    return {"G001": "F0001", "G002": "F0002"}.get(gddm, "")
def fnc_com_get_main_xwdm(fund, xwdm, dt):
    return xwdm
def fnc_get_fund_data_mode(bcrq, gddm, sub_gddm, xwdm):
    return udfconn.execute("SELECT '1'").fetchone()[0]   # 第二连接演示
def fnc_get_rate_info2(fund, dt, kind, stock_kind):
    return "0" if fund == "F0001" else "1"
main.create_function("fnc_com_get_fund_code", fnc_com_get_fund_code, [VARCHAR]*4, VARCHAR)
main.create_function("fnc_com_get_main_xwdm", fnc_com_get_main_xwdm, [VARCHAR]*3, VARCHAR)
main.create_function("fnc_get_fund_data_mode", fnc_get_fund_data_mode, [VARCHAR]*4, VARCHAR)
main.create_function("fnc_get_rate_info2", fnc_get_rate_info2, [VARCHAR]*4, VARCHAR)
main.create_function("fnc_com_get_seat_fee_type", lambda bcrq, scdm, fund, xwdm, kind: "1", [VARCHAR]*5, VARCHAR)

# ── 4. 步骤2：DELETE（延迟事务） + UPDATE SET 内 UDF（SET 别名前缀剥离）──
frame = {"p_o_succeed": "0"}
print("== 步骤2a: DELETE FROM dat_fund_cjqs（COMMIT 延迟到步骤边界）==")
main.execute("DELETE FROM dat_fund_cjqs WHERE bcrq = ? AND scdm = '001'", [P_I_DATE])

print("== 步骤2b: UPDATE SET = PL函数（UDF 桥接；原文 'SET t.fund_code' 需剥离别名前缀）==")
main.execute("""UPDATE mid_yjqs_detail SET fund_code = fnc_com_get_fund_code(gddm, sub_gddm, ?, xwdm)
                WHERE bcrq = ? AND scdm = '001'""", [P_I_DATE, P_I_DATE])
print("   已回填 fund_code:",
      main.execute("SELECT count(*) FROM mid_yjqs_detail WHERE fund_code <> ''").fetchone()[0])

# ── 5. 步骤3：游标 c_get_data（AST parsed_query → 改写 → 绑定 → 流式 FETCH）──
print("== 步骤3: 游标 c_get_data 打开（占位符绑定 p_i_date）==")
ast = json.load(open("/tmp/ast3sh.json"))
proc = [i["Procedure"] for i in
        [s["CreatePackageBody"] for s in ast["statements"] if "CreatePackageBody" in s][0]["items"]
        if i["Procedure"]["name"] == ["prc_trd_hz_byfund"]][0]
cur = [x["Cursor"] for x in proc["block"]["declarations"] if "Cursor" in x]
cur_q = [c for c in cur if c["name"] == "c_get_data"][0]["query"]
assert bool(cur_q) and [c for c in cur if c["name"] == "c_get_data"][0]["parsed_query"] is not None
# 改写规则：R2 to_number → CAST；R3 PL 变量 → 占位符（真实实现：AST Visitor，此处模拟）
q = cur_q.replace("to_number(a.bs)", "CAST(a.bs AS BIGINT)")
q = re.sub(r"\bp_i_date\b", "?", q)
q = q.replace("nvl(", "coalesce(")   # R4：实测 DuckDB 1.4 无 nvl → coalesce
cur_state = {"schema": [], "rows": [], "pos": -1, "found": None, "rowcount": 0}
nph = q.count("?")
res = main.execute(q, [P_I_DATE] * nph)              # OPEN：绑定 + 执行（每处占位符一个参数）
cur_state["schema"] = [d[0] for d in res.description]    # OPEN 时锁定 schema
cur_state["rows"] = res.fetchall()
print(f"   OPEN 完成：schema {len(cur_state['schema'])} 列，行流就绪（快照语义）")

def fetch_into(targets):
    """FETCH INTO：按 schema 对位写入帧；流尾不覆盖目标槽位，置 %NOTFOUND"""
    if cur_state["pos"] + 1 >= len(cur_state["rows"]):
        cur_state["found"] = False; return False
    cur_state["pos"] += 1
    cur_state["rowcount"] += 1
    cur_state["found"] = True
    rec = dict(zip(cur_state["schema"], cur_state["rows"][cur_state["pos"]]))
    for t in targets: frame[t] = rec
    return True

print("== 主循环：逐行模拟解释器 ==")
n_row = 0
while fetch_into(["r_get_data"]) and cur_state["found"]:
    r = frame["r_get_data"]
    n_row += 1
    # SELECT CASE..INTO FROM sys_dummy → 解释器内折叠（纯 PL 表达式，不下发 SQL）
    v_commision_type = "11" if r["security_id"] == "600000" else None
    # CASE..INTO..FROM sys_dummy 折叠：纯 PL 表达式（记录字段引用结果列别名）在解释器内求值
    yj_field = {"11": "yj11"}.get(v_commision_type)
    v_yj = r.get(yj_field, 0) if yj_field else 0
    # SELECT INTO v_par_bond：no_data_found 检测（fetchmany(2) 探测）
    got = main.execute("SELECT bond_kind FROM v_par_bond WHERE security_id=? AND ? BETWEEN inure_begin_date AND inure_end_date",
                       [r["security_id"], P_I_DATE]).fetchmany(2)
    if len(got) == 0:
        frame["p_o_succeed"] = f"在债券信息表中查询内码为{r['security_id']}的债券类型不存在…"   # WHEN no_data_found
        continue
    v_temp_stock_kind = got[0][0]
    # INSERT：decode→CASE；to_number→CAST；UDF 内嵌（fnc_get_rate_info2）
    dv = lambda x, k, a, b=0: a if x == k else b
    cjsl = r["cjsl"]; cjje = r["cjje"]; bs_n = int(r["bs"]) if r["bs"] else 0   # CAST(bs AS BIGINT)
    main.execute("""INSERT INTO dat_fund_cjqs (fund_code,xwdm,main_xwdm,security_id,bs,trade_type,bcrq,
        cjsl,cjje,yhs,jsf,jiesf,zgf,ghf,ghf_dealer,yj,fxj,accrual,pay_type,scdm,stock_kind,mrcb,
        accural_tax,gddm,gddmzm,check_type,gh_type,cjbz) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        [r["fund_code"], r["xwdm"], r["main_xwdm"], r["security_id"], r["bs"], r["trade_type"], P_I_DATE,
         cjsl,
         cjje + dv(r["fffs"], "0", r["accrual"]),                           # decode→CASE（别名 fffs=pay_type）
         r["yhs"], r["jsf"], r["jiesf"], r["zgf"], r["ghf"], r["ghf_dealer"],
         v_yj,
         dv(r["scdm"], "001", dv(frame.get("v_is_venture", "0"), "0", 0, r["fxj"]), r["fxj"]),
         dv(main.execute("SELECT fnc_get_rate_info2(?,?,?,?)",
                         [r["fund_code"], P_I_DATE, "3", v_temp_stock_kind]).fetchone()[0],
            "0", r["accrual2"], r["accrual"]),                              # INSERT VALUES 内 UDF
         r["fffs"], r["scdm"], r["stock_kind"],
         r["mrcb"] + dv(r["fffs"], "0", r["accrual"]) + bs_n * v_yj,
         abs((r["accrual"] or 0) - (r["accrual2"] or 0)),
         r["gddm"], r["gddmzm"], r["check_type"], r["date_source"], r["cjbz"]])
print(f"   处理 {n_row} 行；p_o_succeed={frame['p_o_succeed']!r}（第3行债券参数缺失 → 命名异常分支生效）")

# ── 6. 步骤7：REF CURSOR 动态 OPEN FOR ──
print("== 步骤7: REF CURSOR：运行期拼接 SQL → 运行期解析通道 → 执行 ==")
mk = main.execute("SELECT stock_kind_str, market_code FROM par_oper_venture").fetchone()
v_sql_txt = ("SELECT SUM(FXJ),ROUND(NVL(SUM(fxj2),0),2),FUND_CODE,XWDM,TRADE_TYPE,VENTURE_TYPE FROM ("
    "SELECT SUM(NVL(ROUND(fxj,2),0)) FXJ,NVL(SUM(fxj),0) FXJ2,fund_code,bs,xwdm,trade_type,venture_type FROM "
    "(select zqdm,fund_code,bs,xwdm,trade_type,SUM(NVL(fxj,0)) FXJ,"
    "fnc_com_get_seat_fee_type(BCRQ,SCDM,FUND_CODE,XWDM,STOCK_KIND) VENTURE_TYPE from mid_yjqs_detail "
    f"WHERE SCDM='{mk[1]}' AND BCRQ = '{P_I_DATE}' and {mk[0]}"
    "AND not (is_yysg='1' and ywlb in ('ES','77'))"
    "group by zqdm,bs,fund_code,xwdm,trade_type,"
    "fnc_com_get_seat_fee_type(BCRQ,SCDM,FUND_CODE,XWDM,STOCK_KIND)) "
    "GROUP BY fund_code, bs, XWDM,TRADE_TYPE,venture_type) "
    "GROUP BY fund_code, XWDM,TRADE_TYPE,venture_type")
# 运行期解析通道验证：ogsql 对动态 SQL 文本解析（ SpinDuck 运行期将做同样的事）
r = subprocess.run([str(OGSQL), "validate"], input=v_sql_txt, capture_output=True, text=True)
ok = (r.returncode == 0) and ("error" not in r.stdout.lower())
print("   ogsql 运行期解析:", "VALID ✓" if ok else r.stdout[:300] + r.stderr[:200])
v_sql_txt_rw = v_sql_txt.replace("NVL(", "coalesce(")   # 动态 SQL 同样过改写管线
# ogsql AST→DuckDB SQL：动态 SQL 经运行期解析+改写后下发
c_fxj = main.execute(v_sql_txt_rw).fetchall()
print("   REF CURSOR 结果行:", c_fxj)

# ── 7. 结果核对 ──
print("== 最终核对 ==")
print("dat_fund_cjqs（按基金汇总落表）:")
for row_ in main.execute("SELECT fund_code, security_id, cjsl, jsf, yj FROM dat_fund_cjqs ORDER BY security_id").fetchall():
    print("  ", row_)
print("SIMULATION OK")
