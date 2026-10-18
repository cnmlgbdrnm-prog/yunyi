// DNAT 回读表 → 接管表单的端口归属。
//
// 一台落地机可能挂多条 DNAT（多个服务，如 ss 与 xray），中转机上也可能同时
// 挂着别的链路。先认「目标指向本链路落地机 WG 地址」的行，再用入口端口定位
// 到具体那条——落地端口（表单的「落地端口」，挂中转按它匹配节点）随之确定。
import type { DnatPair } from '../api/types'

/** 本链路落地机名下的 DNAT 行；落地地址未知（探测没读到接口地址）时不做 IP 筛。 */
function pairsForLanding(pairs: DnatPair[], landingWgIp: string | null): DnatPair[] {
  const own = landingWgIp ? pairs.filter((p) => p.target_ip === landingWgIp) : pairs
  return own.length > 0 ? own : pairs
}

/**
 * 探测预填：入口端口命中就用那条，否则给该落地机的首条。
 * 宁可给一个可改的默认值（回读的现有配置），也不留空逼用户手填。
 */
export function pickDnatTarget(
  pairs: DnatPair[],
  landingWgIp: string | null,
  forwardPort: number | null,
): DnatPair | null {
  const pool = pairsForLanding(pairs, landingWgIp)
  return pool.find((p) => p.forward_port === forwardPort) ?? pool[0] ?? null
}

/**
 * 用户改「转发端口」时的跟随：**表里确有这一条**才动落地端口。
 * 不能用上面的兜底版——手打 8443 的过程中前几位都不匹配，
 * 落地端口会被反复拽回首条的目标端口，等于用户在跟自己输入的数字打架。
 */
export function dnatTargetForPort(
  pairs: DnatPair[],
  landingWgIp: string | null,
  forwardPort: number | null,
): DnatPair | null {
  return (
    pairsForLanding(pairs, landingWgIp).find((p) => p.forward_port === forwardPort) ?? null
  )
}
