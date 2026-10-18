// 中转页状态缓存：视图切换（卸载/重挂）后 30s 内直接复用上次查询结果，
// 避免每次切页都 SSH 两端；轮询与手动刷新成功后回写。
// 模块级 Map 跨挂载存活，只在组件生命周期外作为查询结果的最近副本。

import type { LinkStatus } from '../api/types'

/** 轮询/缓存新鲜窗口：结果在此窗口内视为新鲜，切回页面零查询直接展示。 */
export const STATUS_POLL_MS = 30_000

export interface CachedStatus {
  at: number
  status: LinkStatus
}

const cache = new Map<number, CachedStatus>()

/** 读取新鲜缓存：不存在或超出新鲜窗口返回 undefined。 */
export function readCache(ruleId: number, maxAgeMs: number): CachedStatus | undefined {
  const entry = cache.get(ruleId)
  if (entry == null || Date.now() - entry.at >= maxAgeMs) return undefined
  return entry
}

/** 写入最近查询结果（成功或失败态都缓存——避免切页重放失败查询）。 */
export function writeCache(ruleId: number, status: LinkStatus): void {
  cache.set(ruleId, { at: Date.now(), status })
}

/**
 * 丢弃某条链路的状态缓存。
 *
 * **删除链路时必须调**：这个 Map 是模块级的，没有任何回收机制，而链路 id 是自增的、
 * 不会复用——被删掉的 id 再也不会成为这里的读者，条目就永久留着（一次会话删几十条
 * 就躺几十条）。
 *
 * 拆链（不删行）**不调**：id 之后还会复用，缓存会被下一次查询覆盖，不会增长；
 * 而「清视图状态却不清缓存」那点残留由 30s 新鲜窗口自行过期。
 */
export function deleteCache(ruleId: number): void {
  cache.delete(ruleId)
}

/** 挂载时播种：新鲜窗口内的结果直接作为初始状态，首屏零查询。 */
export function seedStatuses(): Record<number, LinkStatus> {
  const out: Record<number, LinkStatus> = {}
  const now = Date.now()
  for (const [id, entry] of cache) {
    if (now - entry.at < STATUS_POLL_MS) out[id] = entry.status
  }
  return out
}

/** 测试专用：清空缓存（beforeEach 隔离）。 */
export function clearStatusCache(): void {
  cache.clear()
}
