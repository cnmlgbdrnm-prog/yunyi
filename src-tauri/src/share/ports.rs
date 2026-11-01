//! 分享端口区间：随机起点与回绕候选。
//!
//! 纯函数；SSH 探测与分配编排在 [`super::install`]。
//! 三个区间互不重叠（订阅 18080–18180 / 公网数据 18200–18499 / 回环 18500–18799），
//! 随机只在各自区间内发生，跨区间隔离由常量保证。

use rand::Rng;

/// 闭区间 `(lo, hi)`。
pub(crate) type PortRange = (u16, u16);

/// 区间内均匀随机起点。
pub(crate) fn random_start_in(range: PortRange, rng: &mut impl Rng) -> u16 {
    rng.gen_range(range.0..=range.1)
}

/// 从 `start` 起、回绕、覆盖整个区间；不重复。
pub(crate) fn range_candidates(range: PortRange, start: u16) -> impl Iterator<Item = u16> {
    let (lo, hi) = range;
    let len = u32::from(hi) - u32::from(lo) + 1;
    let offset = u32::from(start.saturating_sub(lo)) % len;
    (0..len).map(move |n| (u32::from(lo) + (offset + n) % len) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn random_start_stays_in_range() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..1000 {
            let p = random_start_in((18080, 18180), &mut rng);
            assert!((18080..=18180).contains(&p), "{p}");
        }
    }

    #[test]
    fn candidates_cover_range_once_and_wrap() {
        let v: Vec<u16> = range_candidates((18080, 18180), 18178).collect();
        assert_eq!(v.len(), 101);
        assert_eq!(v[0], 18178);
        assert_eq!(v[1], 18179);
        assert_eq!(v[2], 18180);
        assert_eq!(v[3], 18080, "段尾必须回绕到段首");
        let set: std::collections::HashSet<u16> = v.iter().copied().collect();
        assert_eq!(set.len(), 101, "候选不得重复");
    }

    #[test]
    fn start_outside_range_is_clamped_by_offset() {
        let v: Vec<u16> = range_candidates((18200, 18499), 18000).collect();
        assert_eq!(v[0], 18200);
        assert_eq!(v.last().copied(), Some(18499));
    }
}
