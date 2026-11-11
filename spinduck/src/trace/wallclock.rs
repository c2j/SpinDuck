//! UTC 毫秒时间戳：epoch → §11.2 的 `ts` 文本（不引第三方日期库）。

/// epoch 毫秒 → `YYYY-MM-DDTHH:MM:SS.mmmZ`（UTC）。只覆盖 1970-01-01 及以后。
pub fn utc_millis(epoch_ms: i64) -> String {
    let days = epoch_ms.div_euclid(86_400_000);
    let tod = epoch_ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        tod / 3_600_000,
        tod % 3_600_000 / 60_000,
        tod % 60_000 / 1000,
        tod % 1000,
    )
}

/// 自 epoch 的天数 → (年, 月, 日)：Hinnant 的 civil_from_days，纯整数运算。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::utc_millis;

    #[test]
    fn should_render_epoch_millis_as_utc_iso8601() {
        // 期望值由独立实现（Python datetime）交叉核对，闰日/年末边界各锁一条
        assert_eq!(utc_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_millis(1794364267412), "2026-11-11T02:31:07.412Z");
        assert_eq!(utc_millis(1709210096789), "2024-02-29T12:34:56.789Z");
        assert_eq!(utc_millis(1767225599999), "2025-12-31T23:59:59.999Z");
        assert_eq!(utc_millis(951868800000), "2000-03-01T00:00:00.000Z");
        assert_eq!(utc_millis(946684799000), "1999-12-31T23:59:59.000Z");
    }
}
