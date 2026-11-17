#!/bin/bash
# gsql 便捷封装：口令只走环境变量（本机 PGPASSWORD / 容器内 GS_PASSWORD），
# 不进 argv、不落文件。
# 用法：./gsql.sh            —— 从 stdin 读 SQL（多语句脚本）
#       ./gsql.sh -c "SQL"   —— 单语句（"$@" 原样透传）
# 环境变量：SPINDUCK_OG_HOST / SPINDUCK_OG_PORT / SPINDUCK_OG_USER /
#           SPINDUCK_OG_PASSWORD / SPINDUCK_OG_DB（与 spinduck CLI 同名）
#           SPINDUCK_GSQL_DOCKER=1 时在容器内执行 gsql
#           （默认容器 spinduck-gaussdb，可用 SPINDUCK_GSQL_CONTAINER 覆盖）
set -euo pipefail

: "${SPINDUCK_OG_HOST:?需要 SPINDUCK_OG_HOST}"
: "${SPINDUCK_OG_PASSWORD:?需要 SPINDUCK_OG_PASSWORD（口令不入库）}"
PORT="${SPINDUCK_OG_PORT:-5432}"
USER="${SPINDUCK_OG_USER:-gaussdb}"
DB="${SPINDUCK_OG_DB:-postgres}"

if [ "${SPINDUCK_GSQL_DOCKER:-}" = "1" ]; then
  # 口令经 docker -e 注入容器环境（argv 只出现变量名），容器内 gsql -W 读取；
  # bash -c 的位置参数先取走 USER/DB，shift 后 "$@" 透传给 gsql。
  GS_PASSWORD="$SPINDUCK_OG_PASSWORD" exec docker exec -i -e GS_PASSWORD \
    "${SPINDUCK_GSQL_CONTAINER:-spinduck-gaussdb}" bash -c \
    'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:/usr/local/opengauss/lib64:$LD_LIBRARY_PATH; \
     U="$1"; D="$2"; shift 2; \
     exec /usr/local/opengauss/bin/gsql -U "$U" -W "$GS_PASSWORD" -d "$D" \
       -v ON_ERROR_STOP=1 "$@"' \
    _ "$USER" "$DB" "$@"
else
  # 本机直连：PGPASSWORD 环境变量认证，stdin 留给 SQL 脚本。
  PGPASSWORD="$SPINDUCK_OG_PASSWORD" exec gsql \
    -h "$SPINDUCK_OG_HOST" -p "$PORT" -U "$USER" -d "$DB" \
    -v ON_ERROR_STOP=1 "$@"
fi
