//! 切换出口后**应当断开**的连接判据。
//!
//! 为什么必须断：mihomo 换 selector **不重建既有连接**——已建立的连接继续走旧节点，
//! 直到自己自然关闭。而用户点「手动选择」的语义就是「现在就在这个节点上」，不切断旧
//! 连接等于没切。实测症状（2026-09-20）：切到某节点后浏览器当场不可用（keep-alive
//! 还钉在旧出口上，而旧出口可能正是不通的目标地区），过一会儿连接轮换完又自己好了；
//! **开一次 TUN 也会好，那不是 TUN 的功劳**——开关 TUN 会拆网卡与路由、把既有连接
//! 当场全断。
//!
//! 判据刻意不取「全部断开」：规则集里 `GEOSITE,cn`/`GEOIP,CN` 那批是国内直连，与选
//! 哪个节点无关，掐掉它们只会打断正在进行的国内下载，换不来任何收益。
//!
//! 新选中项是「自动选择」时，「不含新选中项」这条判据失效——连接链里同样含「自动
//! 选择」这个词，按名字比对会把全部连接都判成"已经在正确的节点上"而一条都不关。
//! 那时的意图是"让 url-test 重新选"，故放宽成"经手动选择的全部断开"。
//!
//! `providerChains` 不参与判定：本应用的配置来自 `client_yaml`，不含 provider。
use crate::config::{AUTO_GROUP, MANUAL_GROUP};
use crate::net::connlog::MihomoConn;

pub fn conns_to_close(conns: &[MihomoConn], new_selection: &str) -> Vec<String> {
    let pin_by_name = new_selection != AUTO_GROUP;
    conns
        .iter()
        .filter(|c| c.chains.iter().any(|s| s == MANUAL_GROUP))
        .filter(|c| !pin_by_name || !c.chains.iter().any(|s| s == new_selection))
        .map(|c| c.id.clone())
        .filter(|id| !id.is_empty())
        .collect()
}
