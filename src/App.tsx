// 应用根组件：左侧导航 + 视图切换（暗色主题）
import { useEffect, useState } from 'react'
import { listen } from '@tauri-apps/api/event'
import { installJobListener } from './jobs/registry'
import { installRelayStatusListener } from './views/relayStatusStore'
import { installShareStatusListener } from './views/shareStatusStore'
import {
  installMonitorListener,
  refreshMonitor,
  useMonitorSummary,
} from './monitor/registry'
import NavItem from './components/NavItem'
import ConnLog from './views/ConnLog'
import Dashboard from './views/Dashboard'
import Subscriptions from './views/Subscriptions'
import Nodes from './views/Nodes'
import Servers from './views/Servers'
import Relay from './views/Relay'
import Shares from './views/Shares'
import Settings from './views/Settings'
import Monitor from './views/Monitor'

export type ViewKey =
  | 'dashboard'
  | 'connlog'
  | 'subscriptions'
  | 'nodes'
  | 'servers'
  | 'monitor'
  | 'relay'
  | 'shares'
  | 'settings'

export const NAV_ITEMS: { key: ViewKey; label: string }[] = [
  { key: 'dashboard', label: '仪表盘' },
  // 紧跟仪表盘：这是「网络开着但某个程序没走代理」时第一个要看的地方
  { key: 'connlog', label: '连接日志' },
  { key: 'subscriptions', label: '订阅' },
  { key: 'nodes', label: '节点' },
  { key: 'servers', label: '服务器' },
  { key: 'monitor', label: '监控' },
  { key: 'relay', label: '中转' },
  { key: 'shares', label: '分享' },
  { key: 'settings', label: '设置' },
]

export default function App() {
  const [view, setView] = useState<ViewKey>('dashboard')
  const { summary: monitorSummary } = useMonitorSummary()
  const unackedMonitor =
    monitorSummary?.active_alerts.filter((a) => a.acked_at == null).length ?? 0
  // 全进程只装一次 job://progress 订阅与其唯一的对账定时器。
  // 为什么放在根组件而不是各视图：视图是条件挂载的（下面 :73-80），每个视图各自订阅
  // 一份等于「任务进度只在当前那一页可见」，切走的那份订阅连同状态一起没了（评审 #5/#15）。
  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | null = null
    void installJobListener(listen).then((u) => {
      if (disposed) u()
      else unlisten = u
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [])
  // 监控事件监听同样全进程只装一次；首帧先拉一次摘要，不等第一轮事件。
  useEffect(() => {
    void installMonitorListener(listen)
    void refreshMonitor()
  }, [])
  // 中转链路状态：后端定时查询后推送；这里只订阅事件并读一次后端缓存，不发 SSH。
  useEffect(() => {
    void installRelayStatusListener(listen)
  }, [])
  // 分享状态同理：后端逐台采集（含证书）后推送，切页不再触发全量 SSH。
  useEffect(() => {
    void installShareStatusListener(listen)
  }, [])
  return (
    <div className="flex h-screen bg-neutral-950 text-neutral-100">
      <nav className="flex w-48 flex-col gap-1 border-r border-neutral-800 p-3">
        <h1 className="mb-4 px-2 text-lg font-bold">云驿</h1>
        {NAV_ITEMS.map((item) => (
          <NavItem
            key={item.key}
            label={item.label}
            active={view === item.key}
            badge={item.key === 'monitor' ? unackedMonitor : 0}
            onClick={() => setView(item.key)}
          />
        ))}
      </nav>
      <main className="flex-1 overflow-auto p-6">
        {view === 'dashboard' && <Dashboard onNavigate={setView} />}
        {view === 'connlog' && <ConnLog />}
        {view === 'subscriptions' && <Subscriptions />}
        {view === 'nodes' && <Nodes />}
        {view === 'servers' && <Servers />}
        {view === 'monitor' && <Monitor onNavigate={setView} />}
        {view === 'relay' && <Relay />}
        {view === 'shares' && <Shares />}
        {view === 'settings' && <Settings />}
      </main>
    </div>
  )
}
