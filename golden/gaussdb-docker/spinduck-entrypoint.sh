#!/usr/bin/env bash
# SpinDuck 黄金对比用 GaussDB 容器入口。
# 改编自 dota-adapter 交付脚本（lib/fastaas/prototype-1.7/dota-scheduler/
# dota-adapter/docker/dota-entrypoint.sh）：
#   保留：等待应用角色可登录 → 校验 datcompatibility=A → exec 镜像原生入口
#   去除：DOTA step3 schema 自动应用（SpinDuck 测试自建全部对象，见 tests/golden_*）
#
# 关键约束（openGauss）：角色名与 schema 名共享命名空间，绝不能在 USER gaussdb
# 存在前创建 SCHEMA gaussdb；因此以"gaussdb 能登录"作为唯一就绪信号。
set -Eeo pipefail

export GAUSSHOME="${GAUSSHOME:-/usr/local/opengauss}"
export PATH="$GAUSSHOME/bin:$PATH"
export LD_LIBRARY_PATH="$GAUSSHOME/lib:${LD_LIBRARY_PATH:-}"

GS_PASSWORD="${GS_PASSWORD:?GS_PASSWORD is required}"

_ready() {
  gsql -v ON_ERROR_STOP=1 --username gaussdb \
    --password "$GS_PASSWORD" --dbname postgres -c "SELECT 1" >/dev/null 2>&1
}

_oracle_a_mode() {
  [ "$(gsql -U gaussdb -W "$GS_PASSWORD" -d postgres -t -A -c \
    "SELECT datcompatibility FROM pg_database WHERE datname = current_database()" \
    2>/dev/null | head -1 | tr -d '[:space:]')" = "A" ]
}

guard_loop() {
  local tries=0
  while [ "$tries" -lt 90 ]; do
    if _ready; then
      if ! _oracle_a_mode; then
        echo "[spinduck-init] ERROR: postgres is not Oracle A-mode (datcompatibility != A)." >&2
        echo "[spinduck-init] 兼容模式在 initdb 固化，须重建卷：" >&2
        echo "[spinduck-init]   docker compose down -v && docker compose up -d" >&2
        return 1
      fi
      echo "[spinduck-init] gaussdb ready (Oracle A-mode, md5)"
      return 0
    fi
    tries=$((tries + 1))
    sleep 2
  done
  echo "[spinduck-init] WARNING: readiness wait timed out" >&2
  return 1
}

( guard_loop ) &

exec /var/lib/opengauss/entrypoint.sh "$@"
