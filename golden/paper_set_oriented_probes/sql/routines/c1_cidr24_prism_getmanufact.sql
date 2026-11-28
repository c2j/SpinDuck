-- 被调函数：改写自 SQL-ProcBench 的 Scalar UDFs/Sudf_20b_GetManufactComplex.sql
--   （逐字版见 procbench/Sudf_20b_GetManufactComplex.sql；d_year=2003，论文行文里写的是 2023）。
--   唯一改动：参数名 item → p_item。原函数的参数名与表名 item 同名，引擎会把 `from item` 里的
--   表名也替换成参数（生成 `FROM $v_item`）；Oracle 语义下 FROM 子句里的表名不会被 PL 变量遮蔽。
--   属名字解析缺口，另见 REPORT.md。函数体其余部分与基准逐字一致。
--   与驱动过程放在同一文件、函数在前：SQL 语句里的裸名函数调用对注册顺序敏感（见 a0_xfile_call.sql）。
create or replace function getmanufact_complex_r(p_item int)
return char
is
    man char(50);
	cnt1 int; cnt2 int;
begin
	man  := '';
	-- was this item sold in this year through store or catalog?
	select count(*) into cnt1 from store_sales_history, date_dim where ss_item_sk=p_item and d_date_sk=ss_sold_date_sk and d_year=2003;
	select count(*) into cnt2 from catalog_sales_history, date_dim where cs_item_sk=p_item and d_date_sk=cs_sold_date_sk and d_year=2003;
	if(cnt1>0 and cnt2>0) then
		select i_manufact into man from item where i_item_sk = p_item;
	else
		man := 'outdated item'; --implies that this item is not sold in a recent year at all and is probably outdated
	end if;
    return man;
end;
/

-- 探针 C1 —— CIDR 2024「Inlining isn't working out … let's try batching」图 2 的 getManufact，
--   与 PRISM（PVLDB 18(1)）图 1 的 getManufact：两篇论文用的是同一个 UDF。
-- 驱动过程在前文所列被调函数之后定义。
-- 形态：驱动游标循环里逐行调用「两次相关 COUNT 子查询 + IF/ELSE + RETURN」的标量 UDF。
-- 论文对照：Froid 式内联把它变成两层 LATERAL；batching 把它变成 state 表上的两条 UPDATE；
--   PRISM 把两个 COUNT 留在函数里（outline）、只内联 item 查找。
CREATE OR REPLACE PROCEDURE prc_cidr24_c1 IS
BEGIN
  DELETE FROM result_manufact;
  COMMIT;
  FOR r IN (SELECT ws_item_sk
              FROM (SELECT ws_item_sk, COUNT(*) cnt
                      FROM web_sales
                     GROUP BY ws_item_sk
                     ORDER BY COUNT(*), ws_item_sk) t1) LOOP
    INSERT INTO result_manufact(ws_item_sk, manufact)
    VALUES (r.ws_item_sk, getmanufact_complex_r(r.ws_item_sk));
  END LOOP;
  COMMIT;
END;
/
