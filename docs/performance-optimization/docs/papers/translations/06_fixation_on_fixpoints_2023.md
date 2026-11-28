# A Fix for the Fixation on Fixpoints & DuckDB USING KEY (CIDR 2023 / SIGMOD 2025)

## 中文标题
修正对不动点的执念与DuckDB的USING KEY机制

## 核心思想摘要
在WITH RECURSIVE中引入USING KEY，用聚合哈希表原地覆写迭代状态，彻底消除传统单调不动点递归CTE的历史行膨胀与内存爆炸。

## 对 SpinDuck 引擎及集合化架构的启示与映射
SpinDuck基于DuckDB 1.5.5，原生具备USING KEY能力，可用于带状态累加的金融试算游标循环。

---

## 论文前两页原文提取预览
```text
A Fix for the Fixation on Fixpoints
Denis Hirn Torsten Grust
University of Tübingen
Tübingen, Germany
[denis.hirn,torsten.grust]@uni-tuebingen.de
ABSTRACT
We derive new iterative CTE variants from the simple loop-based,
operational semantics of SQL:1999’s WITH RECURSIVE . In the ab-
sence of fixpoint-based semantics and monotonicity restrictions,
these CTE variants (1) can hold onto as well as forget the results of
earlier loop iterations or (2) maintain iteration results in a keyed
table, enabling a SQL authoring style that mimics imperative algo-
rithms. We exercise the new variants using a series of examples to
demonstrate that this fresh look at CTEs has a beneficial impact on
the readability and performance of iterative SQL queries.
1 ITERATE LIKE IT’S 1999?
SQL:1999 [7,20] introduced WITH RECURSIVE —or recursive common
table expressions (CTEs)—a true game changer which turned the
WITH RECURSIVE
T(···)AS(q1
UNION [ALL]
q 𝛿(T)
)
TABLE T;query language into a Turing-complete pro-
gramming language with, admittedly, a very
distinctive flavor. In a nutshell, the recursive
CTE on the left iterates the evaluation of query
q𝛿(read: “ qloop”) which can refer to table Tto
access the rows produced in an earlier iteration.
The first iteration of q𝛿processes the rows produced by q1. Iteration
stops once the least fixpoint T=q1UNION [ALL] q𝛿(T)has been
reached, returning table Tas the overall result.
WITH RECURSIVE is powerful and versatile but proved to be no-
toriously hard to grasp and master. Indeed, only simple applications
like the computation of transitive closures or bills of material in hier-
achical assemblies prevail in practice. Among the many challenges,
let us highlight the following:
(1)The existence and uniqueness of the fixpoint is guaranteed
only if q𝛿is monotonic [2, 8]. This leads to significant syntac-
tic restrictions onq𝛿that rule out uses of negation ( e.g.,NOT
EXISTS ),INTERSECT /EXCEPT , outer joins, duplicate row elimi-
nation via DISTINCT , or grouping and aggregation. Working
around such limitations can turn simple, idiomatic SQL queries
into syntactic atrocities.
(2)Monotonicity enables the semi-naive evaluation of q𝛿over only
the rows produced in the immediately preceding iteration [1].
While this aids the efficient execution of recursive CTEs, q𝛿now
exhibits “short-term memory” in which prior results (or the
history of the computation, if you will) are inaccessible. This
led query authors to adopt idioms that collect result rows in
array-like structures to be carried from iteration to iteration [17,
§ 7.8.2.2, CYCLE ]. Not only does this clutter the query code,
This paper is published under the Creative Commons Attribution 4.0 International
(CC-BY 4.0) license. Authors reserve their rights to disseminate the work on their
personal and corporate Web sites with the appropriate attribution, provided that you
attribute the original work to the authors and CIDR 2023. 13th Annual Conference on
Innovative Data Systems Research (C
```
