#!/bin/bash
# upsert 服务端分解扫描：隔离 UPDATE / INSERT NOT EXISTS 的耗时，
# 扫描 staging 规模 S 与命中率 f（稀疏→稠密）。
#
# 用法：./sweep_upsert.sh <target_table> <base_id>
#   target_table  目标表（须有覆盖 key 的唯一索引）
#   base_id       本轮插入行的 id 基址（每轮递增，避免与既有行冲突；
#                 脚本结束会 DELETE id >= base_id 清理）
#
# 每个组合输出：UPDATE/INSERT 的命令标签与 Time: 毫秒（\timing on）。
# staging 经 INSERT...SELECT generate_series 灌入临时表——测的是 DML 本体；
# COPY FROM STDIN 灌入耗时与密度无关，单独实测约 0.35s/1M 行（见 results.md）。
set -euo pipefail
cd "$(dirname "$0")"

TARGET="${1:?用法: sweep_upsert.sh <target_table> <base_id>}"
BASE="${2:?用法: sweep_upsert.sh <target_table> <base_id>}"

run() { # S=staging 行数 f=命中率百分数
  local S=$1 f=$2 M INS
  M=$(( S * f / 100 ))
  INS=$(( S - M ))
  {
    echo "CREATE TEMP TABLE sd_sync_tmp AS SELECT id, v, w FROM $TARGET LIMIT 0;"
    [ "$M" -gt 0 ] && echo "INSERT INTO sd_sync_tmp SELECT g, g, 'm' FROM generate_series(1,$M) g;"
    [ "$INS" -gt 0 ] && echo "INSERT INTO sd_sync_tmp SELECT $((BASE))+g, g, 'i' FROM generate_series(1,$INS) g;"
    echo "\\timing on"
    echo "UPDATE $TARGET AS tgt SET v = tmp.v, w = tmp.w FROM sd_sync_tmp tmp WHERE tmp.id = tgt.id;"
    echo "INSERT INTO $TARGET (id,v,w) SELECT tmp.id, tmp.v, tmp.w FROM sd_sync_tmp tmp WHERE NOT EXISTS (SELECT 1 FROM $TARGET x WHERE x.id = tmp.id);"
    echo "DELETE FROM $TARGET WHERE id >= $BASE;"
  } | ./gsql.sh
}

for spec in "10000 50" "100000 0" "100000 10" "100000 50" "100000 100" \
            "1000000 0" "1000000 10" "1000000 50" "1000000 100"; do
  set -- $spec
  echo "##### S=$1 f=$2%"
  BASE=$((BASE + 2000000))
  run "$1" "$2"
done
