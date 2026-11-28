# Compiling PL/SQL Away (CIDR 2020)

## 中文标题
编译消除PL/SQL

## 核心思想摘要
通过编译技术将PL/SQL函数及内部循环整体翻译为纯递归SQL，消除解释器与SQL引擎之间数万次上下文切换。

## 对 SpinDuck 引擎及集合化架构的启示与映射
为SpinDuck提供不依赖外部机器码JIT、全量下沉DuckDB的理论原型。

---

## 论文前两页原文提取预览
```text
Compiling PL/SQL Away
Christian Duta Denis Hirn Torsten Grust
Universität Tübingen
Tübingen, Germany
[ christian.duta, denis.hirn, torsten.grust ]@uni-tuebingen.de
ABSTRACT
“PL/SQL functions are slow,” is common developer wisdom
that derives from the tension between set-oriented SQL eval-
uation and statement-by-statement PL/SQL interpretation.
We pursue the radical approach of compiling PL/SQL away,
turning interpreted functions into regular subqueries that
can then be eﬃciently evaluated together with their em-
bracing SQL query, avoiding any PL/SQL ↔SQL context
switches. Input PL/SQL functions may exhibit arbitrary
control ﬂow. Iteration, in particular, is compiled into SQL-
level recursion. RDBMSs across the board reward this com-
pilation eﬀort with signiﬁcant run time savings that render
established developer lore questionable.
1. NOW IS NOT A GOOD TIME
TO INTERRUPT ME
Frequent changes|The required|of context|context switch-
ing eﬀort|can turn|may even outweigh|otherwise tractable
tasks|the cost|into real challenges.|of the tasks themselves.
If you have found those two sentences hard to comprehend,
you were struggling with the context switches —occurring at
every | bar—needed to process a piece of one sentence before
immediately turning focus back to the other.
SQL evaluation in relational DBMSs can face such frequent
context switches, in particular if bits of the query logic are
implemented using PL/SQL,1thein-database scripting lan-
guage. Whenever a SQL query Qinvokes a PL/SQL function,
sayf,
•the DBMS switches from set-oriented plan evaluation to
statement-by-statement PL/SQL interpretation mode (re-
ferred to as switch Qfin the sequel).
•Execution of f’s statements then switches query evalua-
tionbackto planmode—possibly multiple times—toeval-
uate the SQL queries Qiembedded in f(switch fQi).
1We refer to the language as PL/SQL as coined by Oracle.
Our discussion extends to its variants known as PL/pgSQL
in PostgreSQL or T-SQLin Microsoft SQL Server.Context switches will be abundant. If f’s call site is lo-
cated inside a SELECT-FROM-WHEREblock of Q, each row pro-
cessed by the block will invoke f. Likewise, if fembeds
multiple queries or employs iteration, e.g., in terms of FOR
orWHILEloops,weobserverepeatedplanevaluationforthe Qi.
Unfortunately, both kinds of context switches are costly.
Eachswitch QfincursoverheadforPL/SQLinterpreterinvo-
cation or resumption. A switch fQileads to overhead due
to (1) plan generation and caching on the ﬁrst evaluation
ofQior (2) plan cache lookup, plan instantiation, and plan
teardown for each subsequent evaluation of Qi. Iteration in
both, Qand f, multiplies the toll.
Let us make the conundrum concrete with PL/pgSQL func-
tion walk()of Figure 3. The function simulates the walk of
a robot
 on a grid whose cells hold rewards (see Figures 1a
and 2a). On cell (x,y)the robot follows a prescribed policy
(e.g., move down if on cell (3,0), see Figures 1b and 2b).
ThispolicyhasbeenprecomputedbyaMarkovde
```
