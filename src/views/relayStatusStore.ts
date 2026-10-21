// 中转链路状态的前端副本：**后端**每 60s 轮询一次并经 `relay://status` 推送，
// 前端启动时读一次后端缓存快照并订阅事件。切到中转页只读这里，不再发 SSH。
//
// 为什么不再由页面驱动查询：一张中转页上同一隧道的端口行会有很多条，挂载时
// 各查一轮就是十几个 SSH 往返，查回来的还是同一组握手/字节数。状态归后端管，
// 页面只负责显示最近一次推送。
import { useSyncExternalStore } from 'react'
import type { listen } from '@tauri-apps/api/event'
import { relayStatuses } from '../api'
import type { LinkStatus } from '../api/types'

type StatusMap = Record<number, LinkStatus>

let store: StatusMap = {}
const listeners = new Set<() => void>()

function emit(): void {
  for (const fn of listeners) fn()
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn)
  return () => {
    listeners.delete(fn)
  }
}

function getSnapshot(): StatusMap {
  return store
}

export function useRelayStatuses(): StatusMap {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
}

/** 整份替换（后端一次轮询的完整快照）。 */
export function applyRelayStatuses(statuses: StatusMap): void {
  store = statuses
  emit()
}

/** 单条更新（手动「刷新状态」的返回值）。 */
export function applyOneRelayStatus(ruleId: number, status: LinkStatus): void {
  store = { ...store, [ruleId]: status }
  emit()
}

/** 链路被拆/删除后立刻丢弃该行状态；后端下一轮也会把它从快照里拿掉。 */
export function forgetRelayStatus(ruleId: number): void {
  if (!(ruleId in store)) return
  const next = { ...store }
  delete next[ruleId]
  store = next
  emit()
}

/** 测试用：读当前快照。 */
export function getRelayStatusSnapshot(): StatusMap {
  return store
}

let installed = false

/**
 * 全进程只装一次：先读后端缓存播种（不发 SSH），再订阅推送。
 * 生产入口在 App 的 useEffect；失败允许下次重试。
 */
export async function installRelayStatusListener(
  listenFn: typeof listen,
): Promise<void> {
  if (installed) return
  installed = true
  try {
    applyRelayStatuses(await relayStatuses())
    await listenFn('relay://status', (e) => {
      applyRelayStatuses(e.payload as StatusMap)
    })
  } catch {
    // 读不到快照不该拦住页面：状态先显示「未知」，下一轮推送会补上。
    installed = false
  }
}

/** 测试专用：清空 store 与安装标记。 */
export function __resetRelayStatuses(): void {
  store = {}
  installed = false
  emit()
}
