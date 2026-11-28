-- D1' —— 改写自 SQL-ProcBench 的 Scalar UDFs/sudf_8_maxReturnClass.sql（逐字版见同目录）。
-- 唯一改动：游标查询里的列别名 cnt → n_cnt。原因：别名与函数局部变量 cnt 同名，引擎会把
--   别名位置替换成变量的值（生成 `count(i_class) AS CAST(NULL AS DECIMAL)`），项目态编译报
--   KERNEL-0011 Parser Error；Oracle 语义下 SELECT 别名是 SQL 标识符，不会被 PL 变量替换。
--   属名字解析缺口，另见 REPORT.md。函数体其余部分与基准逐字一致。
create or replace function maxreturnclass_r
return char is   --i_class is char(50) data type
    cnt number;
    cls char(50);
    maxClass char(50);
    maxReturn number;
    cursor c1 is
        select i_class, count(i_class) as n_cnt
        from catalog_returns, item
        where i_item_sk = cr_item_sk
        group by i_class;
begin
	maxReturn := 0;
    open c1;
    loop
        fetch c1 into cls, cnt;
        exit when c1%NOTFOUND;
        if cnt>maxReturn then
            maxReturn := cnt;
            maxclass := cls;
        end if;
    end loop;
    close c1;
	return maxClass;
end;
