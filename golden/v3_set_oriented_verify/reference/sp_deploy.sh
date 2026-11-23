#!/usr/bin/env bash
# openGauss 参照侧部署：新建一个独立库（默认 sdv3_<dataset>），把与 SpinDuck
# 侧同一份语料对象（表/视图/桩/包过程）部署进去，再灌同一份参数种子与
# 同一份驱动数据模板（sql/gen_<dataset>.sql 的 :N 替换）。
#
# 用法:  REF_DB=sdv3_purchase bash reference/sp_deploy.sh purchase 100000
#        CORPUS=/path/to/pr59/golden/case_corpus_purchase 必填（被测 head 工作树）
#
# 口径与安全：
#   - 只 CREATE 新库，绝不 DROP/ALTER 任何既有库（postgres/demo 等不触碰）。
#     重跑请用新的 REF_DB 名：mid_yjqs_detail 非空时本脚本会拒绝再灌
#     （二次灌数会把驱动行数翻倍），确需追加才设 ALLOW_APPEND=1。
#   - 口令只在容器内取 $GS_PASSWORD，不落文件、不进命令行。
#   - 参照库默认无索引：语料 DDL 不含二级索引，与 SpinDuck 侧同口径。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTAINER="${CONTAINER:-sd-og70}"
DATASET="${1:?用法: sp_deploy.sh <purchase|byfund> [N]}"
N="${2:-100000}"
CORPUS="${CORPUS:?需要设置 CORPUS 指向被测 head 的 golden/case_corpus_purchase}"
REF_DB="${REF_DB:-sdv3_${DATASET}}"
OUT_DIR="${OUT_DIR:-${TMPDIR:-/tmp}/v3-set-oriented-verify-ref}"
mkdir -p "$OUT_DIR"

gsql() {  # $1=db，SQL 走 stdin
  docker exec -i "$CONTAINER" bash -lc \
    'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:$LD_LIBRARY_PATH
     /usr/local/opengauss/bin/gsql -U gaussdb -W "$GS_PASSWORD" -d "$1" -p 5432 \
       -v ON_ERROR_STOP=1 -tA' sh "$1"
}

if ! docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null | grep -q true; then
  echo "[sp] 参照容器 $CONTAINER 未运行；需要 openGauss-lite 7.0.0-RC3 实例（口径见 reports/PROVENANCE.md）" >&2
  exit 1
fi

echo "[sp] 建库 $REF_DB（DBCOMPATIBILITY 'A'）"
if [ "$(gsql postgres <<< "SELECT 1 FROM pg_database WHERE datname='$REF_DB';")" = "1" ]; then
  echo "[sp] $REF_DB 已存在，复用其对象定义（数据是否会追加见下方 N 检查）"
else
  gsql postgres <<< "CREATE DATABASE $REF_DB DBCOMPATIBILITY 'A' ENCODING 'UTF8';" \
    > "$OUT_DIR/create.log" 2>&1 || { echo "[sp] 建库失败"; tail -5 "$OUT_DIR/create.log"; exit 1; }
fi
gsql "$REF_DB" <<< "SELECT datcompatibility FROM pg_database WHERE datname='$REF_DB';"

echo "[sp] 表 DDL"
gsql "$REF_DB" < "$CORPUS/sql/ddl/tables.sql" > "$OUT_DIR/tables.log" 2>&1 \
  || { echo "[sp] tables 失败"; tail -5 "$OUT_DIR/tables.log"; exit 1; }

echo "[sp] 视图 DDL"
for f in "$CORPUS"/sql/ddl/views/*.sql; do
  gsql "$REF_DB" < "$f" > "$OUT_DIR/views.log" 2>&1 \
    || { echo "[sp] view 失败: $f"; tail -3 "$OUT_DIR/views.log"; exit 1; }
done

echo "[sp] 桩包 + 包过程"
for f in "$CORPUS"/sql/stubs/*.sql "$CORPUS"/sql/routines/*.sql; do
  gsql "$REF_DB" < "$f" > "$OUT_DIR/routines.log" 2>&1 \
    || { echo "[sp] routine 失败: $f"; tail -5 "$OUT_DIR/routines.log"; exit 1; }
done

echo "[sp] 参数种子（剔除 mid_yjqs_detail）$([ "$DATASET" = byfund ] && echo '+ byfund 补件')"
python3 - "$DATASET" "$CORPUS" "$HERE" > "$OUT_DIR/params.sql" <<'PY'
import sys
dataset, corpus, here = sys.argv[1], sys.argv[2], sys.argv[3]
seed = f"{corpus}/cases/{'datasets_byfund' if dataset == 'byfund' else 'datasets'}/seed.sql"
paths = [seed, f"{here}/sql/byfund_params_extra.sql"] if dataset == "byfund" else [seed]
for path in paths:
    for s in open(path).read().split(";"):
        stmt = s.strip()
        if not stmt or "mid_yjqs_detail" in stmt:
            continue
        if "INSERT" in stmt.upper() or "UPDATE" in stmt.upper():
            sys.stdout.write(stmt + ";\n")
PY
gsql "$REF_DB" < "$OUT_DIR/params.sql" > "$OUT_DIR/params.log" 2>&1 \
  || { echo "[sp] 参数种子失败"; tail -5 "$OUT_DIR/params.log"; exit 1; }

echo "[sp] 驱动数据 N=$N（与 SpinDuck 侧同一模板）"
existing=$(gsql "$REF_DB" <<< "SELECT count(*) FROM mid_yjqs_detail;" 2>/dev/null)
if [ "${existing:-0}" != "0" ] && [ "${ALLOW_APPEND:-0}" != "1" ]; then
  echo "[sp] $REF_DB 已有 mid_yjqs_detail ${existing} 行：再灌会翻倍，两侧数字即失真。"
  echo "[sp] 换一个 REF_DB 重跑，或确认要追加时设 ALLOW_APPEND=1。" >&2
  exit 3
fi
sed "s/:N/${N}/g" "$HERE/sql/gen_${DATASET}.sql" > "$OUT_DIR/gen.sql"
gsql "$REF_DB" < "$OUT_DIR/gen.sql" > "$OUT_DIR/gen.log" 2>&1 \
  || { echo "[sp] 数据生成失败"; tail -5 "$OUT_DIR/gen.log"; exit 1; }

echo "[sp] 对象与数据计数"
gsql "$REF_DB" <<< "SELECT 'tables', count(*) FROM information_schema.tables WHERE table_schema='public'
UNION ALL SELECT ' routines', count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public'
UNION ALL SELECT ' detail_rows', count(*) FROM mid_yjqs_detail WHERE bcrq='20250715' AND scdm='001';"
echo "[sp] 索引计数（参照夹具口径：语料 DDL 无二级索引）"
gsql "$REF_DB" <<< "SELECT count(*) FROM pg_indexes WHERE schemaname='public';"
