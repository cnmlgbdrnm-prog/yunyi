// 监控全局 store：事件流是主路径，命令返回用于手动采集/设置保存后的即时更新。
import { useSyncExternalStore } from 'react'
import type { listen } from '@tauri-apps/api/event'
import { monitorSummary } from '../api'
import type { MonitorSummary } from '../api/types'

export interface MonitorStore {
  summary: MonitorSummary | null
  collecting: boolean
  error: string | null
}

let store: MonitorStore = { summary: null, collecting: false, error: null }
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

function getSnapshot(): MonitorStore {
  return store
}

export function getMonitorSnapshot(): MonitorStore {
  return store
}

export function useMonitorSummary(): MonitorStore {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
}

export function applySummary(s: MonitorSummary): void {
  store = { ...store, summary: s, collecting: s.collecting, error: null }
  emit()
}

export function setCollecting(v: boolean): void {
  store = { ...store, collecting: v }
  emit()
}

export function setMonitorError(e: string): void {
  store = { ...store, error: e }
  emit()
}

export async function refreshMonitor(): Promise<void> {
  try {
    applySummary(await monitorSummary())
  } catch (e) {
    setMonitorError(String(e))
  }
}

let installed = false

/**
 * 全进程只安装一次。生产入口在 App 的 useEffect；StrictMode 下 setup1 发起后
 * cleanup 先跑、setup2 再调用——用模块级布尔保证不会装第二份；监听随进程存活。
 */
export async function installMonitorListener(
  listenFn: typeof listen,
): Promise<void> {
  if (installed) return
  installed = true
  try {
    await listenFn('monitor://tick', (e) => {
      applySummary(e.payload as MonitorSummary)
    })
    await listenFn('monitor://collecting', (e) => {
      const p = e.payload as { collecting?: boolean }
      setCollecting(Boolean(p.collecting))
    })
  } catch (e) {
    // 安装失败时允许下次重试，否则监控页永远收不到事件。
    installed = false
    setMonitorError(`安装监控事件监听失败: ${String(e)}`)
  }
}

/** 测试专用：清空 store 与安装标记。 */
export function __resetMonitorStore(): void {
  store = { summary: null, collecting: false, error: null }
  installed = false
  emit()
}
