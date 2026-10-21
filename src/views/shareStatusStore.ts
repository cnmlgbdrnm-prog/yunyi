// 分享状态的前端副本：**后端**定时逐台采集分享列表与证书状态并经
// `share://status` 推送，前端启动时读一次后端缓存并订阅事件。切到分享页只读
// 这里，不再在挂载/切页时触发全量 SSH（列举一次实测 ~14 秒）。
//
// 手动「刷新」仍由页面调用 `listShares()`：后端即时采集、写回缓存并返回快照，
// 页面把它写进本 store。
import { useSyncExternalStore } from 'react'
import type { listen } from '@tauri-apps/api/event'
import { shareStatus } from '../api'
import type { ShareList, ShareTlsStatus } from '../api/types'

let store: ShareList | null = null
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

function getSnapshot(): ShareList | null {
  return store
}

export function useShareStatus(): ShareList | null {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
}

/** 整份替换（后端每轮推送的完整快照 / 手动刷新结果）。 */
export function applyShareStatus(snapshot: ShareList): void {
  store = snapshot
  emit()
}

/** 单台证书状态更新（向导打开或配置 HTTPS 后立即生效，不等下一轮轮询）。 */
export function applyShareTls(serverId: number, status: ShareTlsStatus): void {
  store = {
    shares: store?.shares ?? [],
    unreachable: store?.unreachable ?? [],
    tls: { ...(store?.tls ?? {}), [serverId]: status },
    checked_at: store?.checked_at ?? null,
  }
  emit()
}

/** 测试用：读当前快照。 */
export function getShareStatusSnapshot(): ShareList | null {
  return store
}

let installed = false

/**
 * 全进程只装一次：先读后端缓存播种（不发 SSH），再订阅推送。
 * 生产入口在 App 的 useEffect；失败允许下次重试。
 */
export async function installShareStatusListener(
  listenFn: typeof listen,
): Promise<void> {
  if (installed) return
  installed = true
  try {
    applyShareStatus(await shareStatus())
    await listenFn('share://status', (e) => {
      applyShareStatus(e.payload as ShareList)
    })
  } catch {
    // 读不到快照不该拦住页面：先显示「读取中」，下一轮推送会补上。
    installed = false
  }
}

/** 测试专用：清空 store 与安装标记。 */
export function __resetShareStatuses(): void {
  store = null
  installed = false
  emit()
}
