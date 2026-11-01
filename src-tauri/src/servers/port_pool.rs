//! 主协议随机端口池：10000–65535，排除分享端口段 18080–18799。
//!
//! 纯函数模块：不建连、不探测；SSH 探测与分配编排在 `deploy`。
//! 池按「逻辑索引」分成两段连续端口，随机起点与回绕都在逻辑索引上做，
//! 这样排除段不会引入重试或空号。

use crate::share::naming::{INTERNAL_PORT_END, PORT_RANGE_START};
use rand::Rng;

/// 主协议端口池下界（避开 1–9999 的系统/面板常用端口）。
pub const MAIN_PORT_MIN: u16 = 10000;
/// 主协议端口池上界。
pub const MAIN_PORT_MAX: u16 = 65535;
/// 单次分配最多探测的候选数。池内占用率极低，64 个足够；
/// 有上限才能保证 SSH 往返次数可控。
pub const MAX_PROBES: usize = 64;

/// 分享端口段（订阅/公网数据/内部三段合并），主协议整体避开。
pub const SHARE_PORT_MIN: u16 = PORT_RANGE_START;
pub const SHARE_PORT_MAX: u16 = INTERNAL_PORT_END;

/// 段 1 的长度：10000..=18079。
const SEG1_LEN: u32 = (SHARE_PORT_MIN - MAIN_PORT_MIN) as u32; // 8080
/// 逻辑池总长度：8080 + 46736 = 54816。
const TOTAL: u32 = SEG1_LEN + (MAIN_PORT_MAX - SHARE_PORT_MAX) as u32;

/// 逻辑索引 → 实际端口；`index` 必须 < TOTAL。
fn port_at(index: u32) -> u16 {
    if index < SEG1_LEN {
        MAIN_PORT_MIN + index as u16
    } else {
        SHARE_PORT_MAX + 1 + (index - SEG1_LEN) as u16
    }
}

/// 池内随机起点（均匀覆盖 54816 个端口）。
pub fn random_pool_start(rng: &mut impl Rng) -> u16 {
    port_at(rng.gen_range(0..TOTAL))
}

/// 从 `start` 起、回绕、最多 `limit` 个候选；永不返回分享段端口。
///
/// `start` 必须是池内端口（生产只有 [`random_pool_start`] 或池内端口传入）。
/// 无效起点直接 panic：release 下静默算出错误的 `start_index` 会落到池外端口，
/// 分配结果看起来正常、实际不可用，比尽早失败更糟。
pub fn pool_candidates(start: u16, limit: usize) -> Vec<u16> {
    assert!(
        (MAIN_PORT_MIN..=MAIN_PORT_MAX).contains(&start)
            && !(SHARE_PORT_MIN..=SHARE_PORT_MAX).contains(&start),
        "起点必须在主协议端口池内: {start}"
    );
    let start_index = if start < SHARE_PORT_MIN {
        u32::from(start - MAIN_PORT_MIN)
    } else {
        SEG1_LEN + u32::from(start - (SHARE_PORT_MAX + 1))
    };
    // 先夹到池长再转 u32：`limit as u32` 会截断超大 limit，`start_index + n`
    // 也可能因此溢出。
    let n = limit.min(TOTAL as usize);
    (0..n as u32)
        .map(|n| port_at((start_index + n) % TOTAL))
        .collect()
}

/// 过滤掉已分配/已占用的端口；保持候选顺序。
pub fn skip_taken(candidates: Vec<u16>, taken: &std::collections::HashSet<u16>) -> Vec<u16> {
    candidates
        .into_iter()
        .filter(|p| !taken.contains(p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashSet;

    #[test]
    fn pool_size_matches_segments() {
        assert_eq!(SEG1_LEN, 8080);
        assert_eq!(TOTAL, 54816);
    }

    #[test]
    fn random_start_never_lands_in_share_range() {
        let mut rng = StdRng::seed_from_u64(42);
        let mut distinct: HashSet<u16> = HashSet::new();
        for _ in 0..10_000 {
            let p = random_pool_start(&mut rng);
            assert!((MAIN_PORT_MIN..=MAIN_PORT_MAX).contains(&p), "{p}");
            assert!(!(SHARE_PORT_MIN..=SHARE_PORT_MAX).contains(&p), "{p}");
            distinct.insert(p);
        }
        // 常量桩（恒返回 10000）只有 1 个不同值；真实实现约 9000 个。
        assert!(
            distinct.len() > 1000,
            "随机起点差异过少: {}",
            distinct.len()
        );
    }

    #[test]
    fn candidates_wrap_and_skip_share_range() {
        let v = pool_candidates(65500, 64);
        assert_eq!(v.len(), 64);
        assert_eq!(v[0], 65500);
        assert_eq!(v[35], 65535);
        assert_eq!(v[36], MAIN_PORT_MIN, "段尾必须回绕到池首");
        assert!(
            v.iter()
                .all(|p| !(SHARE_PORT_MIN..=SHARE_PORT_MAX).contains(p)),
            "候选不得落入分享段: {v:?}"
        );
        let set: HashSet<u16> = v.iter().copied().collect();
        assert_eq!(set.len(), 64, "候选不得重复");
    }

    #[test]
    fn start_at_share_boundary_maps_to_next_segment() {
        assert_eq!(pool_candidates(18079, 1), vec![18079]);
        assert_eq!(pool_candidates(18800, 1), vec![18800]);
    }

    #[test]
    fn skip_taken_keeps_order() {
        let taken: HashSet<u16> = [10000, 10002].into_iter().collect();
        assert_eq!(
            skip_taken(vec![10000, 10001, 10002, 10003], &taken),
            vec![10001, 10003]
        );
    }
}
