#!/usr/bin/env python3
"""
SpinDuck 设计验证 #2：模拟运行 PKG_SPLIT_TRADE_STEP3_SH.prc_trd_deal_purchase()。
重点验证：分流策略三类（宏/解释器直调/内联）、日期运算改写、rownum→LIMIT、
GOTO error_row=continue、inst_log 存根过程调用。
"""
import duckdb, json, re, pathlib
try:
    from duckdb.sqltypes import VARCHAR      # DuckDB ≥1.5
except ImportError:
    from duckdb.typing import VARCHAR        # DuckDB ≤1.4 兼容

CORPUS = pathlib.Path(__file__).parent.parent / "lib/fastaas/exam/清算拆分优化考题/基线代码"
P_I_DATE = "20240105"

db = duckdb.connect(":memory:")
main = db.cursor()

# ── 1. 建表（R1：剥离 ::character varying）──
def load_table(name):
    sql = (CORPUS / "1_表结构" / f"{name}.sql").read_text()
    main.execute(re.sub(r"::character varying", "", sql))

for t in ["mid_yjqs_detail", "dat_trd_repurchase", "par_fund_partner", "par_fund_seat",
          "par_fund_seat_para", "par_sys_purchase", "swh_all_kind"]:
    load_table(t)

def cols_of(t):
    return [r[0] for r in main.execute(
        "SELECT column_name FROM information_schema.columns WHERE table_name=? "
        "ORDER BY ordinal_position", [t]).fetchall()]

def insert(t, **kw):
    cols = cols_of(t)
    r = {c: None for c in cols}; r.update(kw)
    nn = {x[0] for x in main.execute(
        "SELECT column_name FROM information_schema.columns WHERE table_name=? AND is_nullable='NO'",
        [t]).fetchall()}
    for c in nn:                       # NOT NULL 默认 0，可被 kw 覆盖
        if r[c] is None: r[c] = 0
    main.execute(f"INSERT INTO {t} ({','.join(r)}) VALUES ({','.join(['?']*len(r))})",
                 list(r.values()))

# ── 2. 参数数据 ──
insert("par_fund_partner", fund_code="F0001", partner_code="G001", sub_partner_code="G001",
       market_code="001", inure_begin_date="20200101", inure_end_date="20991231")
insert("par_fund_seat", fund_code="F0001", seat_code="X001", seat_id="ID001", market_code="001",
       inure_begin_date="20200101", inure_end_date="20991231")
insert("par_sys_purchase", security_id="S001", purchase_days=7,
       inure_begin_date="20200101", inure_end_date="20991231")
insert("par_fund_seat_para", fund_code="F0001", market_code="001", seat_code="X001",
       stock_kind="0000", commision_fee_type="21",
       inure_begin_date="20200101", inure_end_date="20991231")
insert("swh_all_kind", operation_kind="COMMISSION_SWITCH", kind_id="0")

cols = cols_of("mid_yjqs_detail")
nn = {r[0] for r in main.execute(
    "SELECT column_name FROM information_schema.columns WHERE table_name='mid_yjqs_detail' "
    "AND is_nullable='NO'").fetchall()}
def qsrow(**kw):
    r = {c: None for c in cols}
    r.update(bcrq=P_I_DATE, scdm="001", is_yysg="0", ywlb="00")
    for c in nn:
        if r[c] is None: r[c] = 0
    r.update(kw); return r
qsrows = [
    # 正常回购：参数齐全 → INSERT 成功
    qsrow(fund_code="F0001", xwdm="X001", main_xwdm="X001", security_id="S001", cjbh=123001, bs="1",
          trade_type="T1", cjsl=100, cjje=1000.0, jsf=2.0, fxj=1.0, yj06=6.0,
          stock_kind="0501", ghcjje=1100.0, ghcjje2=1105.0, gddm="G001", sub_gddm="G001",
          zqdm="S001", pay_type="0", accrual=0.0),
    # 股东未维护 par_fund_partner → OTHERS + inst_log + GOTO continue
    qsrow(fund_code="", xwdm="X002", main_xwdm="", security_id="S002", cjbh=123002, bs="1",
          trade_type="T2", cjsl=50, cjje=500.0, jsf=1.0, stock_kind="0502",
          ghcjje=550.0, ghcjje2=552.0, gddm="G099", sub_gddm="G099",
          zqdm="S002", pay_type="0"),
    # 证券无 par_sys_purchase → OTHERS 分支
    qsrow(fund_code="F0001", xwdm="X003", main_xwdm="", security_id="S099", cjbh=123003, bs="1",
          trade_type="T3", cjsl=10, cjje=100.0, jsf=0.5, stock_kind="0503",
          ghcjje=101.0, ghcjje2=101.5, gddm="G001", sub_gddm="G001",
          zqdm="S099", pay_type="0"),
    # stock_kind 0509 → 游标 WHERE 排除；0100 → substr<>05 排除
    qsrow(fund_code="F0001", xwdm="X004", security_id="S004", cjbh=123004, bs="1", trade_type="T4",
          cjsl=1, cjje=1.0, stock_kind="0509", gddm="G001", sub_gddm="G001", zqdm="S004", pay_type="0"),
    qsrow(fund_code="F0001", xwdm="X005", security_id="S005", cjbh=123005, bs="1", trade_type="T5",
          cjsl=1, cjje=1.0, stock_kind="0100", gddm="G001", sub_gddm="G001", zqdm="S005", pay_type="0"),
]
main.executemany(f"INSERT INTO mid_yjqs_detail VALUES ({','.join(['?']*len(cols))})",
                 [tuple(r[c] for c in cols) for r in qsrows])

# 日志存根（pkg_split_trade_log.inst_log → ProcedureCall 模拟）
main.execute("CREATE TABLE inst_log_tbl (msg VARCHAR)")
log_buf = []
def inst_log(*args): log_buf.append(args[3]); main.execute("INSERT INTO inst_log_tbl VALUES (?)", [args[3]])

# ── 3. 分流策略验证 ──
print("== 分流③：表达式级函数 → DuckDB 宏 ==")
main.execute("CREATE OR REPLACE MACRO fnc_com_if_deal_date(market_code, dt) AS 0")  # 纯表达式
main.execute("""CREATE OR REPLACE MACRO fnc_com_getday(scdm, dt, off) AS
                strftime(strptime(dt, '%Y%m%d') + INTERVAL (off) DAY, '%Y%m%d')""")  # 日期运算改写
print("   宏注册成功；fnc_com_getday('001','20240105',1) =",
      main.execute("SELECT fnc_com_getday('001', '20240105', 1)").fetchone()[0])

print("== 分流①：过程级函数 FNC_GET_PURCHASE_JS_DAYS → 解释器直调（PL 上下文，无桥接）==")
frame = {}
def fnc_get_purchase_js_days(p_date, p_security_id):
    """解释器执行 PL 函数体：SELECT INTO + 嵌套函数调用 + 异常"""
    try:
        got = main.execute(
            "SELECT coalesce(purchase_days, 1) FROM par_sys_purchase WHERE security_id=? "
            "AND ? BETWEEN inure_begin_date AND inure_end_date", [p_security_id, p_date]).fetchmany(2)
        v_purchase_days = got[0][0] if len(got) == 1 else "0"
    except Exception:
        v_purchase_days = "0"
    if len(got) == 0: v_purchase_days = "0"
    v_repurchase_date = main.execute(  # 嵌套调用③宏路径的 getday
        "SELECT fnc_com_getday('001', ?, ?)", [p_date, int(v_purchase_days)]).fetchone()[0]
    # fnc_com_if_deal_date → ③宏（表达式级），返回 0
    v_flag = main.execute("SELECT fnc_com_if_deal_date('001', ?)", [v_repurchase_date]).fetchone()[0]
    if v_flag == 1:
        v_repurchase_date = main.execute(
            "SELECT fnc_com_getday('001', ?, 1)", [v_repurchase_date]).fetchone()[0]
    v_js = main.execute("SELECT fnc_com_getday('001', ?, 1)", [p_date]).fetchone()[0]
    return int(v_purchase_days), v_js

print("== 步骤1: DELETE（延迟事务）==")
main.execute("DELETE FROM dat_trd_repurchase WHERE purchase_date=? AND market_code='001'", [P_I_DATE])

print("== 步骤2: 游标 c_get_purchase 打开 ==")
ast = json.load(open("/tmp/ast3sh.json"))
proc = [i["Procedure"] for i in
        [s["CreatePackageBody"] for s in ast["statements"] if "CreatePackageBody" in s][0]["items"]
        if i["Procedure"]["name"] == ["prc_trd_deal_purchase"]][0]
cur = [x["Cursor"] for x in proc["block"]["declarations"] if "Cursor" in x][0]
q = cur["query"]
q = re.sub(r"\bp_i_date\b", "?", q)
q = q.replace("nvl(", "coalesce(")  # R3
res = main.execute(q, [P_I_DATE])
schema = [d[0] for d in res.description]
rows = res.fetchall()
print(f"   OPEN 完成：{len(schema)} 列（别名即 %ROWTYPE 字段：{schema[1]}…），候选行 {len(rows)}（预期 3：0509/0100 被排除）")
assert len(rows) == 3, f"游标过滤不符: {len(rows)}"

def fetch_into():
    if cur["_pos"] + 1 >= len(rows): cur["found"] = False; return False
    cur["_pos"] += 1; cur["found"] = True
    frame["r"] = dict(zip(schema, rows[cur["_pos"]]))
    return True

print("== 主循环：逐行处理（含 GOTO error_row=continue、日期运算、rownum→LIMIT）==")
cur = {"_pos": -1, "found": None}
inserted, skipped = 0, 0
while fetch_into() and cur["found"]:
    r = frame["r"]
    # SELECT INTO fund（0 行 → OTHERS → inst_log + GOTO error_row）
    got = main.execute("""SELECT t.fund_code FROM par_fund_partner t
        WHERE ? BETWEEN t.inure_begin_date AND t.inure_end_date
          AND t.partner_code=? AND t.sub_partner_code=? AND t.market_code=?
          AND t.fund_code IN (SELECT p.fund_code FROM par_fund_seat p
              WHERE p.seat_code=? AND ? BETWEEN p.inure_begin_date AND p.inure_end_date)""",
        [P_I_DATE, r["partner_code"], r["sub_partner_code"], r["market_code"],
         r["seat_code"], P_I_DATE]).fetchmany(2)
    if len(got) != 1:  # 0 行或 2 行 → WHEN OTHERS
        inst_log(P_I_DATE, "99999999", "回购业务业务拆分",
                 f"股东代码为{r['gddm']},席位代码为{r['seat_code']}的信息没有在par_fund_parter表中维护",
                 "210", "1", "cf", "0")
        skipped += 1; continue          # ← GOTO error_row = continue（实证）
    v_fund_code = got[0][0]
    # SELECT INTO purchase_days（nvl→coalesce 已改写；S099 无参数 → OTHERS 分支）
    got = main.execute("""SELECT coalesce(purchase_days, 1) FROM par_sys_purchase
        WHERE ? BETWEEN inure_begin_date AND inure_end_date AND security_id=?""",
        [P_I_DATE, r["security_id"]]).fetchmany(2)
    if len(got) != 1:
        inst_log(P_I_DATE, "99999999", "回购业务业务拆分",
                 f"取{r['security_id']}回购类型时出错！", "210", "1", "cf", "0")
        skipped += 1; continue
    v_purchase_days = got[0][0]
    # v_purchase_js_days := FNC_GET_PURCHASE_JS_DAYS(...) → 解释器直调（含嵌套调用）
    v_purchase_days_js, _ = fnc_get_purchase_js_days(r["purchase_date"], r["security_id"])
    # 日期运算：to_char(to_date(d,'YYYYMMDD')+n,'YYYYMMDD') → 宏同款改写（strptime+INTERVAL）
    v_repurchase_date = main.execute(
        "SELECT strftime(strptime(?, '%Y%m%d') + INTERVAL (?) DAY, '%Y%m%d')",
        [r["purchase_date"], int(v_purchase_days)]).fetchone()[0]
    # rownum=1 → LIMIT 1（ORDER BY 优先级保留）
    got = main.execute("""SELECT a.commision_type FROM (
        SELECT s.commision_fee_type commision_type FROM par_fund_seat_para s
        WHERE s.fund_code=? AND s.market_code=? AND s.seat_code IN (?, '000000')
          AND s.stock_kind IN (?, substr(?, 1, 2) || '00', '0000')
          AND ? BETWEEN s.inure_begin_date AND s.inure_end_date
        ORDER BY seat_code DESC, stock_kind DESC) a LIMIT 1""",
        [v_fund_code, r["market_code"], r["seat_code"], r["stock_kind"],
         r["stock_kind"], P_I_DATE]).fetchmany(2)
    v_commision_fee_type = got[0][0] if len(got) == 1 else "0"
    # CASE..INTO..FROM sys_dummy → 解释器折叠
    yj_map = {"21": "purchase_yj04", "22": "purchase_yj03", "24": "purchase_yj11",
              "26": "purchase_yj17", "31": "purchase_yj06", "32": "purchase_yj05",
              "34": "purchase_yj12", "36": "purchase_yj18"}
    v_yj = r.get(yj_map.get(v_commision_fee_type, "purchase_yj05"), 0)
    # INSERT（round/abs 原生；COMMIT 延迟）
    main.execute("""INSERT INTO dat_trd_repurchase (fund_code,market_code,partner_code,security_id,
        purchase_no,purchase_fee,purchase_fxj,purchase_yj,purchase_money,purchase_quantity,
        purchase_date,buy_sale,seat_code,repurchase_date,repurchase_money,repurchase_money_js,
        accrual_daily,accrual_daily_js,purchase_days,purchase_days_js,purchase_kind,gddmzm)
        VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        [v_fund_code, r["market_code"], r["partner_code"], r["security_id"],
         r["purchase_date"] + str(r["cjbh"]),  # R-隐式转换：DECIMAL cjbh 拼接 → VARCHAR
         r["purchase_fee"], r["purchase_fxj"], v_yj,
         r["purchase_money"], abs(r["purchase_quantity"]), r["purchase_date"], r["buy_sale"],
         r["seat_code"], v_repurchase_date, r["repurchase_money"], r["repurchase_money_js"],
         round((r["repurchase_money"] - r["purchase_money"]) / v_purchase_days, 2),
         round((r["repurchase_money_js"] - r["purchase_money"]) / v_purchase_days_js, 2),
         v_purchase_days, v_purchase_days_js, r["purchase_kind"], r["sub_partner_code"]])
    inserted += 1

print(f"   结果：成功 INSERT {inserted} 行；GOTO 跳过 {skipped} 行；inst_log 记录 {len(log_buf)} 条")
print("== 最终核对 ==")
for row_ in main.execute("""SELECT fund_code, security_id, purchase_days, purchase_days_js,
                            repurchase_date, accrual_daily FROM dat_trd_repurchase""").fetchall():
    print("  ", row_)
print("日志:", log_buf)
print("SIMULATION #2 OK")
