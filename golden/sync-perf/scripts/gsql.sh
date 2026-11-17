#!/bin/bash
# gsql 便捷封装：口令只经 stdin pipeline 传给 gsql -2（口令管理模式），
# 不进 argv、不落文件（stdin 脚本先落临时 SQL 文件，口令不落盘）。
# 用法：./gsql.sh            —— 从 stdin 读 SQL（多语句脚本）
#       ./gsql.sh -c "SQL"   —— 单语句（"$@" 原样透传）
# 环境变量：SPINDUCK_OG_HOST / SPINDUCK_OG_PORT / SPINDUCK_OG_USER /
#           SPINDUCK_OG_PASSWORD / SPINDUCK_OG_DB（与 spinduck CLI 同名）
#           SPINDUCK_GSQL_DOCKER=1 时在容器内执行 gsql
#           （默认容器 spinduck-gaussdb，可用 SPINDUCK_GSQL_CONTAINER 覆盖）
# 注：gsql 不识别 PGPASSWORD/.pgpass（实测），口令只能 -W(argv 泄漏) 或
# -2(stdin pipeline)——本脚本统一用后者。
set -euo pipefail

: "${SPINDUCK_OG_HOST:?需要 SPINDUCK_OG_HOST}"
: "${SPINDUCK_OG_PASSWORD:?需要 SPINDUCK_OG_PASSWORD（口令不入库）}"
PORT="${SPINDUCK_OG_PORT:-5432}"
USER="${SPINDUCK_OG_USER:-gaussdb}"
DB="${SPINDUCK_OG_DB:-postgres}"

if [ "${SPINDUCK_GSQL_DOCKER:-}" = "1" ]; then
  # 口令经 docker -e 注入容器环境（argv 只出现变量名 GSQL_PW）；
  # 容器内 gsql -2 从 stdin 读口令——docker exec / gsql 的 argv 均无口令值。
  GSQL_PW="$SPINDUCK_OG_PASSWORD" exec docker exec -i -e GSQL_PW \
    "${SPINDUCK_GSQL_CONTAINER:-spinduck-gaussdb}" bash -c '
      export LD_LIBRARY_PATH=/usr/local/opengauss/lib:/usr/local/opengauss/lib64:$LD_LIBRARY_PATH
      U="$1"; D="$2"; shift 2
      BIN=/usr/local/opengauss/bin/gsql
      # CONN 有意不加引号：按词分割为 -h/-p 两个参数
      CONN="-h 127.0.0.1 -p 5432"
      if [ $# -gt 0 ]; then
        printf "%s\n" "$GSQL_PW" | $BIN -2 $CONN -U "$U" -d "$D" -v ON_ERROR_STOP=1 "$@"
      else
        tmp=$(mktemp /tmp/.gsql-stdin.XXXXXX) || exit 1
        cat > "$tmp"
        printf "%s\n" "$GSQL_PW" | $BIN -2 $CONN -U "$U" -d "$D" -v ON_ERROR_STOP=1 -f "$tmp"
        rc=$?
        rm -f "$tmp"
        exit $rc
      fi
    ' _ "$USER" "$DB" "$@"
else
  # 本机直连：同一 -2 口令管道口径；stdin 脚本先落临时文件（只含 SQL）。
  if [ $# -gt 0 ]; then
    printf '%s\n' "$SPINDUCK_OG_PASSWORD" | gsql -2 \
      -h "$SPINDUCK_OG_HOST" -p "$PORT" -U "$USER" -d "$DB" \
      -v ON_ERROR_STOP=1 "$@"
  else
    tmp=$(mktemp /tmp/.gsql-stdin.XXXXXX) || exit 1
    cat > "$tmp"
    printf '%s\n' "$SPINDUCK_OG_PASSWORD" | gsql -2 \
      -h "$SPINDUCK_OG_HOST" -p "$PORT" -U "$USER" -d "$DB" \
      -v ON_ERROR_STOP=1 -f "$tmp"
    rc=$?
    rm -f "$tmp"
    exit $rc
  fi
fi
