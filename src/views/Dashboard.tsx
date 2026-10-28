// 仪表盘视图：全景状态概览——服务器、中转链路、最近对测、运行中任务、节点、订阅
// 六张状态卡（点击跳对应页）；启动重建失败等启动配置错误经 settings 持久化后在此顶部告警。
// 数据经 src/api 类型化封装并发拉取（Promise.all），全部只读 IPC，无 SSH、无副作用。
import { useCallback, useEffect, useState } from 'react'
import {
  getSettings,
  getSystemProxy,
  getSubServerStatus,
  listJobs,
  listNodes,
  listPrivacies,
  listRelayRules,
  listServers,
  listSubscriptions,
  listThroughput,
  netCurrent,
  netRepairProxy,
  netSelect,
  netSetTun,
  netStart,
  netStatus,
  netStop,
} from '../api'
import type {
  Job,
  NetStatus,
  Node,
  Privacy,
  RelayRule,
  Server,
  Subscription,
  ThroughputTest,
} from '../api/types'
import type { ViewKey } from '../App'
import NetworkCard, { AUTO_OPTION } from '../components/NetworkCard'
import ServerHealthCard from './dashboard/ServerHealthCard'
import { useMonitorSummary } from '../monitor/registry'

/** bits_per_second → Mbps 文本；空值/非有限值显示「—」（防 NaN 进 UI） */
function mbpsText(bps: number | null): string {
  if (bps === null || !Number.isFinite(bps)) return '—'
  return (bps / 1_000_000).toFixed(1)
}

export default function Dashboard({
  onNavigate,
}: {
  onNavigate: (view: ViewKey) => void
}) {
  const { summary: monitor } = useMonitorSummary()
  const [servers, setServers] = useState<Server[]>([])
  const [rules, setRules] = useState<RelayRule[]>([])
  const [throughputs, setThroughputs] = useState<ThroughputTest[]>([])
  const [jobs, setJobs] = useState<Job[]>([])
  const [nodes, setNodes] = useState<Node[]>([])
  const [privacies, setPrivacies] = useState<Privacy[]>([])
  const [subs, setSubs] = useState<Subscription[]>([])
  const [subServerRunning, setSubServerRunning] = useState<boolean | null>(null)
  const [startupError, setStartupError] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 本机上网客户端：状态（null=查询失败降级）、忙碌（开关动作期间）、错误说明
  const [net, setNet] = useState<NetStatus | null>(null)
  const [netBusy, setNetBusy] = useState(false)
  const [netSelectBusy, setNetSelectBusy] = useState(false)
  const [systemProxy, setSystemProxy] = useState<string | null | undefined>(undefined)
  const [proxyRepairBusy, setProxyRepairBusy] = useState(false)
  const [netTunBusy, setNetTunBusy] = useState(false)
  const [netError, setNetError] = useState<string | null>(null)
  // 当前出口节点（手动选择组回显；null=未运行/未知）
  const [selectedNode, setSelectedNode] = useState<string | null>(null)
  // 出口模式。**必须单独记**：自动模式在服务端也是把「手动选择」组写成具体节点名，
  // 只回显 selectedNode 的话，用户选的「自动（测速最快）」下一秒就显示成某个节点名，
  // 看着像应用替他改成了手动。
  const [exitMode, setExitMode] = useState<'auto' | 'manual'>('auto')

  const refresh = useCallback(async () => {
    try {
      const [sv, rl, tp, jb, nd, sb] = await Promise.all([
        listServers(),
        listRelayRules(),
        listThroughput(),
        listJobs(),
        listNodes(),
        listSubscriptions(),
      ])
      setServers(sv)
      setRules(rl)
      setThroughputs(tp)
      setJobs(jb)
      setNodes(nd)
      setSubs(sb)
    } catch (e) {
      setError(String(e))
    }
    // 订阅服务状态失败不拖垮整屏：单独降级为未知（null 显示「订阅服务未知」）
    getSubServerStatus()
      .then(setSubServerRunning)
      .catch(() => setSubServerRunning(null))
    // 上网状态同理：失败降级为未知（null 禁用开关，防误操作）
    netStatus()
      .then((s) => {
        setNet(s)
        // 运行时回显当前出口；查询失败只重置回显，不拖垮状态卡
        if (s.running) {
          netCurrent().then(setSelectedNode).catch(() => setSelectedNode(null))
        } else {
          setSelectedNode(null)
        }
      })
      .catch(() => setNet(null))
    getSystemProxy().then(setSystemProxy).catch(() => setSystemProxy(undefined))
    listPrivacies().then(setPrivacies).catch(() => setPrivacies([]))
  }, [])

  /** 一键开关：开启走冲突扫描（冲突即拒，提醒不抢占）；关闭还原系统代理 */
  const handleNetToggle = async () => {
    if (!net) return
    setNetError(null)
    setNetBusy(true)
    try {
      setNet(net.running ? await netStop() : await netStart())
      getSystemProxy().then(setSystemProxy).catch(() => setSystemProxy(undefined))
      if (!net.running) setSelectedNode(null)
    } catch (e) {
      // 冲突/启动失败：说明展示原因，状态回查一次（可能半启动失败已清理）
      setNetError(String(e))
      try {
        setNet(await netStatus())
      } catch {
        /* 保持原状态 */
      }
    } finally {
      setNetBusy(false)
    }
  }

  /**
   * 开关 TUN 全局模式。
   *
   * 状态一律取后端返回的那份（它是从服务回读的），不做本地乐观翻转——TUN 没有
   * 降级路径，失败时必须让开关**回弹**并展示服务的原话，否则用户以为自己没点到。
   */
  const handleTunToggle = async (enable: boolean) => {
    if (!net) return
    setNetError(null)
    setNetTunBusy(true)
    try {
      setNet(await netSetTun(enable))
    } catch (e) {
      setNetError(String(e))
      try {
        setNet(await netStatus())
      } catch {
        /* 保持原状态 */
      }
    } finally {
      setNetTunBusy(false)
    }
  }

  /** 隐私出口要先核验 HTTPS；成功前保持旧出口回显，避免界面先报已切换。 */
  const handleNetSelect = async (name: string) => {
    setNetError(null)
    const mode = name === AUTO_OPTION ? 'auto' : 'manual'
    setNetSelectBusy(true)
    try {
      await netSelect(name)
      setSelectedNode(name)
      setExitMode(mode)
    } catch (e) {
      setNetError(String(e))
      netCurrent().then(setSelectedNode).catch(() => setSelectedNode(null))
    } finally {
      setNetSelectBusy(false)
    }
  }

  const handleRepairProxy = async () => {
    setNetError(null)
    setProxyRepairBusy(true)
    try {
      await netRepairProxy()
      setSystemProxy(await getSystemProxy())
    } catch (e) {
      setNetError(String(e))
    } finally {
      setProxyRepairBusy(false)
    }
  }

  // 下拉项：启用节点（客户端 config 只含启用+非 ssr）+ 自动选择（默认成员可改回）
  const clientNodes = nodes.filter((n) => n.enabled && n.protocol !== 'ssr')
  const privateNames = new Map(
    privacies.filter((p) => p.onion_host).map((p) => [p.node_id, `${p.name} [隐私]`]),
  )

  useEffect(() => {
    void refresh()
    // 启动重建失败只进 stderr 在打包后不可见，读取持久化告警在此展示
    getSettings(['startup_config_error', 'exit_mode'])
      .then((s) => {
        const v = s.startup_config_error
        setStartupError(v && v.trim() ? v : null)
        // 缺省算自动：新配置里「手动选择」默认就指向「自动选择」
        setExitMode(s.exit_mode === 'manual' ? 'manual' : 'auto')
      })
      .catch((e) => setError(String(e)))
  }, [refresh])

  // Tor 在后台 bootstrap；网络已开启时定期读状态，避免面板一直停在启动瞬间的 off。
  //
  // **未知（net === null）时同样要轮询**，只有「明确问到了：没在跑」才停。停早了的
  // 代价是再没人把状态问回来——界面会一直停在那一刻的读数，只能靠切页重挂载恢复
  // （2026-09-28 实测：应用挂久了显示「网络未开启」并给出开启按钮，而网络其实在跑）。
  // 用派生布尔而不是 `net?.running` 做依赖：每次轮询回来都是新对象，直接依赖 net
  // 会每 2 秒重建一次定时器；这个布尔只在「该不该轮询」翻转时才变。
  const netPolling = net === null || net.running
  useEffect(() => {
    if (!netPolling) return
    let active = true
    const timer = window.setInterval(() => {
      netStatus().then((status) => {
        if (active) setNet(status)
      }).catch(() => {})
      getSystemProxy().then((proxy) => {
        if (active) setSystemProxy(proxy)
      }).catch(() => {
        if (active) setSystemProxy(undefined)
      })
    }, 2000)
    return () => {
      active = false
      window.clearInterval(timer)
    }
  }, [netPolling])

  const serverErrors = servers.filter((s) => s.last_check_error).length
  const builtRules = rules.filter((r) => r.status === 'built' || r.status === 'imported').length
  const failedRules = rules.filter((r) => r.status === 'failed').length
  const runningJobs = jobs.filter((j) => j.status === 'running' || j.status === 'queued').length
  const enabledNodes = nodes.filter((n) => n.enabled).length
  const errorSubs = subs.filter((s) => s.last_sync_error).length
  const expectedProxy = net?.port != null ? `http://127.0.0.1:${net.port}` : null
  const proxyDetached = Boolean(net?.running && !net.tun && systemProxy !== undefined && systemProxy !== expectedProxy)
  const latestTp = throughputs
    .filter((t) => t.bits_per_second !== null)
    .sort((a, b) => b.checked_at.localeCompare(a.checked_at))[0]
  const latestTpError = throughputs
    .filter((t) => t.error)
    .sort((a, b) => b.checked_at.localeCompare(a.checked_at))[0]

  return (
    <div className="space-y-4">
      <h2 className="text-xl font-semibold">仪表盘</h2>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}
      {startupError && (
        <div
          role="alert"
          className="rounded border border-amber-700 bg-amber-950 px-3 py-2 text-sm text-amber-200"
        >
          {startupError}
        </div>
      )}
      {/* 去掉 max-w-4xl：卡片**跟着窗口变宽**，而不是钉死在 896px 让右边空一片
          （本机 2560×1440@150%，逻辑 1707 → 主区可用 ~1467px）。
          列数**不跟着涨**：要的是每张卡变宽（放大），不是拆出更多更小的卡。 */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <StatCard
          label="服务器"
          value={String(servers.length)}
          sub={serverErrors > 0 ? `${serverErrors} 台连通异常` : '连通正常'}
          alert={serverErrors > 0}
          onClick={() => onNavigate('servers')}
        />
        <StatCard
          label="中转链路"
          value={String(builtRules)}
          sub={failedRules > 0 ? `${failedRules} 条异常` : `共 ${rules.length} 条`}
          alert={failedRules > 0}
          onClick={() => onNavigate('relay')}
        />
        <StatCard
          label="最近对测"
          value={latestTp ? `${mbpsText(latestTp.bits_per_second)} Mbps` : '—'}
          sub={
            latestTpError && !latestTp
              ? '上次失败'
              : latestTp
                ? new Date(latestTp.checked_at).toLocaleString()
                : '暂无对测'
          }
          alert={Boolean(latestTpError && !latestTp)}
          onClick={() => onNavigate('servers')}
        />
        <StatCard
          label="运行中任务"
          value={runningJobs > 0 ? String(runningJobs) : '无'}
          sub={runningJobs > 0 ? '进行中' : '空闲'}
          onClick={() => onNavigate('servers')}
        />
        <StatCard
          label="节点"
          value={`${enabledNodes} / ${nodes.length}`}
          sub="启用 / 总数"
          onClick={() => onNavigate('nodes')}
        />
        <StatCard
          label="订阅"
          value={String(subs.length)}
          sub={
            errorSubs > 0
              ? `${errorSubs} 个同步异常`
              : subServerRunning === null
                ? '订阅服务未知'
                : subServerRunning
                  ? '订阅服务运行中'
                  : '订阅服务已停止'
          }
          alert={errorSubs > 0}
          onClick={() => onNavigate('subscriptions')}
        />
        <ServerHealthCard
          summary={monitor}
          onClick={() => onNavigate('monitor')}
        />
      </div>
      <NetworkCard
        net={net}
        netBusy={netBusy}
        netError={netError}
        netSelectBusy={netSelectBusy}
        netTunBusy={netTunBusy}
        proxyRepairBusy={proxyRepairBusy}
        proxyUnknown={systemProxy === undefined}
        exitMode={exitMode}
        selectedNode={selectedNode}
        clientNodes={clientNodes}
        privateNames={privateNames}
        proxyDetached={proxyDetached}
        onToggle={() => void handleNetToggle()}
        onSelect={(name) => void handleNetSelect(name)}
        onTunToggle={(enable) => void handleTunToggle(enable)}
        onRepairProxy={() => void handleRepairProxy()}
        onOpenSettings={() => onNavigate('settings')}
      />
    </div>
  )
}

function StatCard({
  label,
  value,
  sub,
  alert,
  onClick,
}: {
  label: string
  value: string
  sub: string
  alert?: boolean
  onClick: () => void
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`rounded border p-4 text-left transition-colors hover:border-neutral-600 ${
        alert ? 'border-red-900 bg-red-950' : 'border-neutral-800 bg-neutral-900'
      }`}
    >
      <p className="text-sm text-neutral-400">{label}</p>
      <p className="mt-1 text-2xl font-bold">{value}</p>
      <p className="mt-1 text-xs text-neutral-500">{sub}</p>
    </button>
  )
}
