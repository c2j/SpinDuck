# Dear User-Defined Functions, Inlining isn't working out so great for us. Let's try batching (CIDR 2024)

## 中文标题
致UDF：内联对我们并不奏效，让我们尝试批处理（CIDR 2024）

## 核心思想摘要
颠覆性发现：对于复杂UDF，批处理（Batching，将外层输入打包为批次表一次性处理）最高比内联快93.4倍，因为极大降低了优化器去相关难度。

## 对 SpinDuck 引擎及集合化架构的启示与映射
奠定SpinDuck FnSite批量元组下沉、WriteBuffer（Appender列式直写）与同构UPDATE攒批的核心理论。

---

## 论文前两页原文提取预览
```text
Dear User-Defined Functions,
Inlining isn’t working out so great for us.
Let’s try batching to make our relationship work.
Sincerely, SQL
Kai Franz
 , Samuel Arch
 , Denis Hirn
 , Torsten Grust
 , Todd C. Mowry
 , Andrew Pavlo
Carnegie Mellon University,
 University of Tübingen
udfs-cidr24@cs.cmu.edu
Abstract
SQL’s user-defined functions (UDFs) allow developers to express
complex computation using procedural logic. But UDFs have been
the bane of database management systems (DBMSs) for decades
because they inhibit optimization opportunities, potentially slow-
ing down queries significantly. In response, batching andinlining
techniques have been proposed to enable effective query optimiza-
tion of UDF calls within SQL. Inlining is now available in a major
commercial DBMS. But the trade-offs between both approaches on
modern DBMSs remain unclear.
We evaluate and compare UDF batching and inlining on enter-
prise and open-source DBMSs using a state-of-the-art UDF-centric
workload. We observe the surprising result that although inlin-
ing is better on simple UDFs, batching outperforms inlining by up
to93.4×for more complex UDFs because it makes it easier for a
DBMS’s query optimizer to decorrelate subqueries. We propose a
hybrid approach that chooses batching or inlining to achieve the
best performance.
1 SQL AND UDFs: A DIFFICULT AFFAIR
Nearly 50 years since the invention of SQL, it remains a challenge to
express complex logic in a language that lacks procedural constructs
such as sequential and branching control flow.
To address this problem, all the major DBMSs extend SQL with
support for user-defined functions (UDFs) [ 19] written in proce-
dural programming languages. UDFs were first introduced by IN-
GRES in the 1970s to allow users to add operations on user-defined
types [ 17]. Since SQL:1999, the SQL standard has included PL/PSM
for defining UDFs. DBMSs such as Oracle, IBM DB2, PostgreSQL,
and SQL Server support UDFs, particularly the kind that allow
users to embed SQL queries inside UDFs (collectively referred to as
PL/SQL UDFs in what follows). Such UDFs are now commonplace
and are executed billions of times per day on Azure SQL Server
alone [ 8,19]. By encapsulating query logic inside UDFs and mak-
ing nested UDF calls, UDFs allow for modularity, code reuse, and
simplicity compared to pure SQL.
Despite their software engineering advantages, PL/SQL UDFs
are notoriously difficult to optimize. The main challenge comes
from the SQL queries that are intermixed with the UDF’s procedural
This paper is published under the Creative Commons Attribution 4.0 International
(CC-BY 4.0) license. Authors reserve their rights to disseminate the work on their
personal and corporate Web sites with the appropriate attribution, provided that you
attribute the original work to the authors and CIDR 2024. 14th Annual Conference on
Innovative Data Systems Research (CIDR ’24). TODO, Chaminade, USAstatements. Combining these two different programming paradigms
makes i
```
