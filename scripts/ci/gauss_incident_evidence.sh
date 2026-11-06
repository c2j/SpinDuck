#!/usr/bin/env bash
# 职责：CI golden job 的事后取证脚本。参照实例（openGauss 容器 PID 1）在测试窗口内
# 消失时，客户端只剩一句 `[ENV-0020] connection closed`；本脚本把"服务端到底怎么死的"
# 一次落全：PID1 权威 rlimit、宿主内核记录（segfault / OOM）、systemd-coredump 栈、
# 容器生命周期事件、pg_log 增量窗口、崩溃恢复判定。
#
# 用法：gauss_incident_evidence.sh <容器名> <窗口起点 RFC3339> [pg_log 偏移文件] [PID1 基线文件]
#
# 纪律：
#   - 取证脚本绝不能反过来成为新的红因——全程 set +e，末尾一律 exit 0。
#   - 不进容器命名空间采样：宿主侧能拿到的（/proc、dmesg、coredumpctl、Engine API）
#     一律走宿主；只有读 pg_log 才 docker exec（日志在容器卷里，宿主不可见）。
#   - 不打印口令。容器内口令只由容器自身环境变量提供，本脚本不接触。
set +e

container="${1:?用法: $0 <容器名> <窗口起点 RFC3339> [pg_log 偏移文件] [PID1 基线文件]}"
since="${2:?用法: $0 <容器名> <窗口起点 RFC3339> [pg_log 偏移文件] [PID1 基线文件]}"
offsets="${3:-}"
baseline="${4:-/tmp/og_pid1_baseline.txt}"
until_iso=$(date -u +%Y-%m-%dT%H:%M:%S)
# systemd 的 --since 接受 "YYYY-MM-DD HH:MM:SS"；只做字符串变形，不调 date -d（macOS 无此选项）
since_systemd="${since/T/ }"

SUDO=""
command -v sudo >/dev/null 2>&1 && SUDO="sudo"

cid=$(docker inspect -f '{{.Id}}' "$container" 2>/dev/null)
cpid=$(docker inspect -f '{{.State.Pid}}' "$container" 2>/dev/null)

echo "::group::容器 / PID1 状态（取证窗口 $since → $until_iso）"
[ -f "$baseline" ] && cat "$baseline"
docker inspect \
  -f 'status={{.State.Status}} health={{.State.Health.Status}} pid1_oom_killed={{.State.OOMKilled}} restart_count={{.RestartCount}} exit_code={{.State.ExitCode}} started={{.State.StartedAt}} finished={{.State.FinishedAt}} pid={{.State.Pid}} id={{.Id}}' \
  "$container" 2>/dev/null
echo "::endgroup::"

echo "::group::PID1 权威 rlimit 与落 core 条件"
# `docker exec ... ulimit -c` 读的是**新 exec 出来的 shell** 的限额，不是 PID 1 的；
# 判"崩溃能不能落 core"必须看宿主侧 /proc/<pid>/limits（内核维护，进程改不了）。
echo "PID1 pid=${cpid:-?}（宿主 core_pattern: $($SUDO cat /proc/sys/kernel/core_pattern 2>/dev/null)）"
$SUDO cat "/proc/${cpid:-0}/limits" 2>/dev/null | grep -iE 'core file size|address space|locked memory|Max processes'
echo "PID1 cgroup: $($SUDO cat "/proc/${cpid:-0}/cgroup" 2>/dev/null | tr '\n' ' ')"
echo "::endgroup::"

echo "::group::容器生命周期事件（die 的 exitCode / 信号是判读的关键）"
# 不用 {{.TimeRFC3339}}：不同 docker CLI 版本对该模板键的支持不一致，取证脚本
# 不该引入新的失败模式；unix 秒足够对齐窗口。
docker events --since "$since" --until "$until_iso" \
  --filter "container=$container" \
  --filter event=die --filter event=oom --filter event=kill --filter event=restart \
  --format '{{.Time}} {{.Action}} {{.Attributes}}' 2>/dev/null \
  | head -40
echo "（以上为空 = 窗口内无容器生命周期事件）"
echo "::endgroup::"

echo "::group::宿主内核记录（segfault / OOM / traps）"
# postmaster 猝死最常见的外证就在宿主 dmesg：`gaussdb[123]: segfault at ... error 4`
# 或 cgroup OOM 的 `Killed process ... (gaussdb)`。这两类都不进 openGauss 自己的日志。
$SUDO dmesg -T 2>&1 \
  | grep -iE 'segfault|general protection|vmalloc|oom-kill|out of memory|killed process|traps|core dumped|signal 1[13]' \
  | tail -40
echo "（以上为空 = 宿主内核没有相关记录或 dmesg 不可读）"
echo "::endgroup::"

echo "::group::systemd-coredump（runner 的 core_pattern 是管道，core 不落目录而进 coredumpctl）"
$SUDO coredumpctl list --no-pager --since "$since_systemd" 2>&1 | tail -20
$SUDO coredumpctl info --no-pager gaussdb 2>&1 \
  | grep -iE '^ *(PID|signal|timestamp|executable|unit|package)' -A0 \
  | head -20
echo "--- 崩溃栈顶（coredumpctl info 的 Stack trace 段）---"
$SUDO coredumpctl info --no-pager gaussdb 2>&1 \
  | grep -A45 -m1 'Stack trace of thread' \
  | head -50
echo "::endgroup::"

echo "::group::crash-recovery 判定"
echo -n "pg_log 中出现 'database system was not properly shut down' 的文件数（>0 = 上一次实例异常终止，走恢复）: "
docker exec "$container" bash -c \
  "grep -lE 'database system was not properly shut down' /var/lib/opengauss/data/pg_log/*.log 2>/dev/null | wc -l" 2>/dev/null
echo -n "pg_log 中出现 'received ... shutdown request' 的行数（>0 = 收到过关停信号，属正常关停路径）: "
docker exec "$container" bash -c \
  "grep -hE 'received .*shutdown request' /var/lib/opengauss/data/pg_log/*.log 2>/dev/null | wc -l" 2>/dev/null
echo "::endgroup::"

if [ -n "$offsets" ]; then
  echo "::group::pg_log 增量（取证：只输出窗口起点之后写入的字节）"
  docker exec "$container" bash -c "cd /var/lib/opengauss/data/pg_log || exit 0
  for f in *.log; do
    off=\$(sed -n \"s|^\$f ||p\" $offsets)
    off=\${off:-0}
    now=\$(wc -c < \"\$f\")
    [ \"\$now\" -gt \"\$off\" ] || continue
    echo \"===== \$f 增量 \$((now-off)) 字节 =====\"
    tail -c +\$((off+1)) \"\$f\" > /tmp/ci_delta.log
    echo '--- 关键行 ---'
    grep -inE 'FATAL|PANIC|ERROR|out of memory|exceed|terminat|signal|kill|crash|backtrace|segfault' /tmp/ci_delta.log | tail -80
    echo '--- 增量尾部 120 行 ---'
    tail -120 /tmp/ci_delta.log
  done" 2>/dev/null
  echo "::endgroup::"
fi

echo "::group::内存口径（峰值 / 当前 / 限额）"
cgrouprel=$($SUDO awk -F'::' '$1=="0"{print $2}' "/proc/${cpid:-0}/cgroup" 2>/dev/null | head -1)
peak=""
[ -n "$cgrouprel" ] && [ "$cgrouprel" != "/" ] && peak=$($SUDO cat "/sys/fs/cgroup$cgrouprel/memory.peak" 2>/dev/null)
cur=""
[ -n "$cgrouprel" ] && [ "$cgrouprel" != "/" ] && cur=$($SUDO cat "/sys/fs/cgroup$cgrouprel/memory.current" 2>/dev/null)
lim=""
[ -n "$cgrouprel" ] && [ "$cgrouprel" != "/" ] && lim=$($SUDO cat "/sys/fs/cgroup$cgrouprel/memory.max" 2>/dev/null)
echo "宿主 cgroup（$cgrouprel）: peak=${peak:-?} current=${cur:-?} max=${lim:-?} 字节"
echo "Engine API 限额: $(docker inspect -f '{{.HostConfig.Memory}}' "$container" 2>/dev/null) 字节"
[ -z "$peak" ] && echo "docker stats（当前值≠峰值）: $(docker stats --no-stream --format '{{.MemUsage}}' "$container" 2>/dev/null)"
echo "--- 容器内 sysctl（口径是否真的生效）---"
docker exec "$container" bash -c 'grep -E "^(max_process_memory|shared_buffers|cstore_buffers|dynamic_memory_limit|effective_io_concurrency)" /var/lib/opengauss/data/postgresql.conf' 2>/dev/null
echo "--- 宿主可用内存 ---"
free -m 2>/dev/null | head -3
echo "::endgroup::"

echo "::group::容器 stdout（窗口起点之后，尾部 250 行）"
docker logs --since "$since" "$container" 2>&1 | tail -250
echo "::endgroup::"

# 取证脚本永不影响门禁：任何一步失败都不代表被测行为失败。
exit 0
