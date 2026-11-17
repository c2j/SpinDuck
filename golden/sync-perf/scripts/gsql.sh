#!/bin/bash
# gsql 便捷封装：口令只走环境变量，不进 argv/文件。
# 用法：./gsql.sh            —— 从 stdin 读 SQL（多语句脚本）
#       ./gsql.sh -c "SQL"   —— 单语句
# 环境变量：SPINDUCK_OG_HOST / SPINDUCK_OG_PORT / SPINDUCK_OG_USER /
#           SPINDUCK_OG_PASSWORD / SPINDUCK_OG_DB（与 spinduck CLI 同名）
set -euo pipefail

: "${SPINDUCK_OG_HOST:?需要 SPINDUCK_OG_HOST}"
: "${SPINDUCK_OG_PASSWORD:?需要 SPINDUCK_OG_PASSWORD（口令不入库）}"
PORT="${SPINDUCK_OG_PORT:-5432}"
USER="${SPINDUCK_OG_USER:-gaussdb}"
DB="${SPINDUCK_OG_DB:-postgres}"

# 容器场景：gsql 在容器内、LD_LIBRARY_PATH 需手工给；直连场景用本机 gsql。
if [ "${SPINDUCK_GSQL_DOCKER:-}" = "1" ]; then
  exec docker exec -i "${SPINDUCK_GSQL_CONTAINER:-spinduck-gaussdb}" bash -c \
    'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:/usr/local/opengauss/lib64:$LD_LIBRARY_PATH; \
     /usr/local/opengauss/bin/gsql -U "$1" -W "$2" -d "$3" -v ON_ERROR_STOP=1' \
    _ "$USER" "$SPINDUCK_OG_PASSWORD" "$DB" "$@"
else
  exec gsql -h "$SPINDUCK_OG_HOST" -p "$PORT" -U "$USER" -d "$DB" \
    -v ON_ERROR_STOP=1 "$@"
fi
