-- D2' —— 改写自 SQL-ProcBench 的 Stored Procedures/proc21_activatePromoCat.sql。
-- 形态：游标循环 + 按主键逐行 UPDATE（OLTP 存过里最常见的「游标更新」写法）。
-- 唯一改动：游标源由表值函数 bestPromosCatalog() 换成普通表 best_promos_catalog。
--   原因：基准的 PL/SQL 版表值函数（TVF4b_bestPromoCatalog.sql）依赖 BULK COLLECT，
--   引擎报 ENG-0071 不支持；换表后循环体与控制流逐字保留。
create or replace procedure activatePromoCat
is
	promo_sk int;
	cursor c1 is select promo_sk from best_promos_catalog;
begin
	open c1;
	loop
        fetch c1 into promo_sk;

        exit when c1%NOTFOUND;

        update promotion set p_end_date_sk = 10000000, p_discount_active='Y'
		where p_promo_sk = promo_sk;
	end	LOOP;
    close c1;
end;
