// 服务器管理视图：服务器列表、新建/编辑表单、连通测试、一键免密、删除与部署入口，
// 以及 P5 服务器测速（单发/批量/并发/取消）、BBR 惰性检测与一键开启、iperf3 吞吐对测。
// 任务状态一律取自模块级注册表（jobs/registry）：测速与对测是两个 kind，各自订阅
// useTrackedJobs；行内结果按 job 分桶（targetResult），一个任务终结只带走自己那一桶。
// BBR 惰性触发：页面加载/刷新不调 check_bbr，仅行内按钮点击发起。吞吐对测默认取
// 第一条 built/imported 链路的中转机→落地机（listRelayRules 派生），最近结果展示
// listThroughput 首行。已部署协议徽章读后端 deployed_protocols_cmd（不再靠本地镜像）。
import { useCallback, useEffect, useRef, useState, type Dispatch, type SetStateAction } from 'react'
import {
  cancelJob,
  checkBbr,
  deleteServer,
  enableBbr,
  installIperf3,
  listDeployedProtocolDetails,
  listRelayRules,
  listServers,
  listThroughput,
  serverSpeed,
  serverThroughput,
  setupPasswordless,
  testServerConnection,
} from '../api'
import type { ConnectionTest, DeployedProtocolDetail, RelayRule, Server, ThroughputTest } from '../api/types'
import {
  bindJob,
  cancelableJobIds,
  firstJob,
  isTargetBusy,
  release,
  reserveJob,
  useJobTerminal,
  useTrackedJobs,
} from '../jobs/registry'
import ServerFormPanel, { EMPTY_SERVER_FORM, type ServerFormState } from '../components/ServerFormPanel'
import ServerList from './servers/ServerList'
import ServerTools from './servers/ServerTools'
import { useAddrFamilySwitch } from './servers/useAddrFamilySwitch'
import { useServerSave } from './servers/useServerSave'

/** Set<number> 型开关状态：新增/移除一个 id。展开态有三处（更多/通道/上游出口），
 *  原先各写一份七行；一份就够，行为也只有一个定义。 */
const toggleSet = (set: Dispatch<SetStateAction<Set<number>>>, id: number) => {
  set((prev) => {
    const next = new Set(prev)
    if (next.has(id)) next.delete(id)
    else next.add(id)
    return next
  })
}

/** 从 Set 中移除一个 id；本来就不存在时返回原集合，避免无意义重渲染。 */
const withoutId = (set: Set<number>, id: number) => {
  if (!set.has(id)) return set
  const next = new Set(set)
  next.delete(id)
  return next
}

export default function Servers() {
  const [servers, setServers] = useState<Server[]>([])
  const [rules, setRules] = useState<RelayRule[]>([])
  const [form, setForm] = useState<ServerFormState | null>(null)
  const [editingId, setEditingId] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [tests, setTests] = useState<Record<number, ConnectionTest>>({})
  const [testing, setTesting] = useState<Set<number>>(new Set())
  const [pwBusy, setPwBusy] = useState<Set<number>>(new Set())
  const [pwDone, setPwDone] = useState<Record<number, boolean>>({})
  // 就地展开：同一时间最多一张卡片展开（部署向导或编辑表单），互斥由单状态模型保证
  const [expand, setExpand] = useState<{ type: 'deploy' | 'edit'; id: number } | null>(null)
  // 已部署协议徽章：**后端节点行为准**（含完整变体与端口，不再读最新部署摘要）。
  // 原先只由部署完成回调写本地 state，切页即消失——徽章与后端事实重复（评审 #17）。
  const [deployedProtos, setDeployedProtos] = useState<Record<number, DeployedProtocolDetail[]>>({})
  // 测速与对测任务态取自注册表：多 job 并行跟踪，切页不丢（见 jobs/registry 文件头）。
  // 后端两个任务共用一个 kind（server_speed），前端分成两个桶——对测终态要另刷吞吐列表。
  const [concurrency, setConcurrency] = useState(3)
  const speedJobs = useTrackedJobs('server_speed')
  const throughputJobs = useTrackedJobs('server_throughput')
  // 部署任务（含保存出口族后自动排的重部署）：向导展开时由向导自己显示，
  // 其余情况由行级进度显示（ServerList）。
  const deployJobs = useTrackedJobs('deploy')
  // 对测同一时间只允许一个（运行中按钮即禁用），取唯一那个即可
  const throughputJob = firstJob(throughputJobs)
  // BBR：惰性检测（行内按钮触发）；algo=当前算法，msg=成功/失败结果
  const [bbrAlgo, setBbrAlgo] = useState<Record<number, string>>({})
  const [bbrChecking, setBbrChecking] = useState<Set<number>>(new Set())
  const [bbrEnabling, setBbrEnabling] = useState<Set<number>>(new Set())
  const [bbrMsg, setBbrMsg] = useState<Record<number, { ok: boolean; text: string }>>({})
  // 吞吐对测：源/目标/秒数、最近结果（任务态与阶段文案在注册表里）
  const [fromId, setFromId] = useState<number | null>(null)
  const [toId, setToId] = useState<number | null>(null)
  const [seconds, setSeconds] = useState(15)
  const [throughputResults, setThroughputResults] = useState<ThroughputTest[]>([])
  const [installBusy, setInstallBusy] = useState(false)
  const [installMsg, setInstallMsg] = useState<string | null>(null)
  // 通道面板：按服务器 id 记录展开状态
  const [channelsOpen, setChannelsOpen] = useState<Set<number>>(new Set())
  const [upstreamsOpen, setUpstreamsOpen] = useState<Set<number>>(new Set())
  // 工具折叠 + 卡片内「更多」展开（纯 UI 状态，不影响数据流）
  const [toolsOpen, setToolsOpen] = useState(true)
  const [moreOpen, setMoreOpen] = useState<Set<number>>(new Set())

  const refresh = useCallback(async () => {
    try {
      const [ss, rs] = await Promise.all([listServers(), listRelayRules()])
      setServers(ss)
      setRules(rs)
    } catch (e) {
      setError(String(e))
    }
  }, [])

  const loadThroughput = useCallback(async () => {
    try {
      setThroughputResults(await listThroughput())
    } catch (e) {
      setError(String(e))
    }
  }, [])

  // 已部署协议：逐台读后端节点行详情。读失败**不打掉页面**（沿用 Nodes.tsx:68 对隐私开关的做法：
  // 这只是个徽章，没读到就不显示），但也不写空值覆盖上一次结果——免得一次瞬时失败
  // 让徽章闪没。
  const loadDeployed = useCallback(async (ids: number[]) => {
    const pairs = await Promise.all(
      ids.map(async (id) => [id, await listDeployedProtocolDetails(id).catch(() => null)] as const),
    )
    setDeployedProtos((prev) => {
      const next = { ...prev }
      for (const [id, protos] of pairs) {
        if (protos != null) next[id] = protos
      }
      return next
    })
  }, [])

  // 地址族切换是「服务器 + 全部自建直连节点」的事务改动，切换期间锁住该行按钮。
  const { busy: addrBusy, switchTo: handleAddrFamily } = useAddrFamilySwitch(setError, refresh, loadDeployed)

  // 行内链路摘要：从规则数据派生（仅显示已建/接管中的链路，not-built 不算）
  const linkSummary = (s: Server): string | null => {
    const rule = rules.find((r) => (r.relay_server_id === s.id || r.landing_server_id === s.id) && r.status !== 'not-built')
    if (!rule) return null
    if (s.kind === 'landing') {
      const relay = servers.find((x) => x.id === rule.relay_server_id)
      return `${relay ? relay.name : `#${rule.relay_server_id}`} → ${s.name}`
    }
    const landing = servers.find((x) => x.id === rule.landing_server_id)
    return `被 ${landing ? landing.name : `#${rule.landing_server_id}`} 中转`
  }

  useEffect(() => {
    void refresh()
  }, [refresh])

  useEffect(() => {
    void loadThroughput()
  }, [loadThroughput])

  // 服务器列表一变（增删/刷新）就重读已部署协议：徽章永远对得上后端
  useEffect(() => {
    void loadDeployed(servers.map((s) => s.id))
  }, [servers, loadDeployed])

  // 对测默认源/目标：首次出现 built/imported 链路时取中转机→落地机（用户改过后不再覆盖）
  const defaultsPicked = useRef(false)
  useEffect(() => {
    if (defaultsPicked.current || (fromId != null && toId != null)) return
    const rule = rules.find((r) => r.status === 'built' || r.status === 'imported')
    if (rule) {
      defaultsPicked.current = true
      setFromId(rule.relay_server_id)
      setToId(rule.landing_server_id)
    }
  }, [rules, fromId, toId])

  // 保存编排（新建 / 编辑 + 出口变化时的自动重部署）收在 hook 里，见 useServerSave。
  // 编辑保存后表单关闭，重部署进度因此落在行级（ServerList 的 deployJobs）。
  const closeForm = useCallback(() => {
    setForm(null)
    setEditingId(null)
    setExpand(null)
  }, [])
  const handleSave = useServerSave(servers, editingId, form, refresh, setError, closeForm)

  // 顶部「更多/收起」在用户眼里就是这张卡的折叠键：收起副操作行时，展开区
  // （部署向导 / 编辑表单）与通道、上游出口三个子面板必须一起收，否则会停在
  // 「上面收起了、下面还挂着一块」的半收起状态（2026-10-19 实测）。
  // 只处理收起方向；展开「更多」不影响其它面板。
  const handleToggleMore = (id: number) => {
    const closing = moreOpen.has(id)
    toggleSet(setMoreOpen, id)
    if (!closing) return
    setExpand((cur) => (cur?.id === id ? null : cur))
    setChannelsOpen((prev) => withoutId(prev, id))
    setUpstreamsOpen((prev) => withoutId(prev, id))
  }

  const handleTest = async (id: number) => {
    setTesting((prev) => new Set(prev).add(id))
    setError(null)
    try {
      const result = await testServerConnection(id)
      setTests((prev) => ({ ...prev, [id]: result }))
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setTesting((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const handlePasswordless = async (id: number) => {
    setError(null)
    setPwBusy((prev) => new Set(prev).add(id))
    try {
      await setupPasswordless(id)
      setPwDone((prev) => ({ ...prev, [id]: true }))
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setPwBusy((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const handleDelete = async (id: number) => {
    setError(null)
    try {
      await deleteServer(id)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  const startSpeedTest = async (ids: number[]) => {
    setError(null)
    // 已被在跟踪任务覆盖的目标跳过：占位在 await **之前**提交，连点两次的第二下
    // 在这里就被挡下（评审 #6：原先忙碌态要等 IPC 回来才生效，那段时间是空的）
    const pending = ids.filter((id) => !isTargetBusy(speedJobs, String(id)))
    if (pending.length === 0) return
    const ticket = reserveJob('server_speed', pending.map(String))
    try {
      bindJob(ticket, await serverSpeed(pending, concurrency))
    } catch (e) {
      release(ticket)
      setError(String(e))
    }
  }

  const handleSpeedTestOne = (id: number) => {
    void startSpeedTest([id])
  }

  const handleBatchSpeedTest = () => {
    const ids = servers.map((s) => s.id)
    if (ids.length === 0) {
      setError('没有服务器，无法批量测速')
      return
    }
    void startSpeedTest(ids)
  }

  const handleCancelSpeed = async () => {
    // 快照后再删：下面的 release 会改注册表。
    // 本地一并复位（后端终态事件到达时任务已不在表里，自然忽略）；占位（IPC 还没返回的
    // 负数 id）后端不认，只能本地撤。
    const jobs = [...speedJobs.values()]
    for (const id of cancelableJobIds(speedJobs)) {
      try {
        await cancelJob(id)
      } catch (e) {
        setError(String(e))
      }
    }
    for (const job of jobs) release(job.id)
    void refresh()
  }

  const handleCheckBbr = async (id: number) => {
    setError(null)
    setBbrChecking((prev) => new Set(prev).add(id))
    try {
      const algo = await checkBbr(id)
      setBbrAlgo((prev) => ({ ...prev, [id]: algo }))
      setBbrMsg((prev) => {
        const next = { ...prev }
        delete next[id]
        return next
      })
    } catch (e) {
      setBbrMsg((prev) => ({ ...prev, [id]: { ok: false, text: String(e) } }))
    } finally {
      setBbrChecking((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const handleEnableBbr = async (id: number) => {
    setError(null)
    setBbrEnabling((prev) => new Set(prev).add(id))
    try {
      const msg = await enableBbr(id)
      setBbrMsg((prev) => ({ ...prev, [id]: { ok: true, text: msg } }))
      setBbrAlgo((prev) => ({ ...prev, [id]: 'bbr' }))
    } catch (e) {
      setBbrMsg((prev) => ({ ...prev, [id]: { ok: false, text: String(e) } }))
    } finally {
      setBbrEnabling((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const handleThroughputStart = async () => {
    if (fromId == null || toId == null) return
    setError(null)
    const ticket = reserveJob('server_throughput', [String(fromId), String(toId)])
    try {
      bindJob(ticket, await serverThroughput(fromId, toId, seconds))
    } catch (e) {
      release(ticket)
      setError(String(e))
    }
  }

  const handleCancelThroughput = async () => {
    const job = firstJob(throughputJobs)
    if (job != null) {
      if (job.id >= 0) {
        try {
          await cancelJob(job.id)
        } catch (e) {
          setError(String(e))
        }
      }
      release(job.id)
    }
    void refresh()
    void loadThroughput()
  }

  const handleInstallIperf3 = async () => {
    if (toId == null) return
    setError(null)
    setInstallBusy(true)
    setInstallMsg(null)
    try {
      setInstallMsg(await installIperf3(toId))
    } catch (e) {
      setError(String(e))
    } finally {
      setInstallBusy(false)
    }
  }

  const startEdit = (s: Server) => {
    setEditingId(s.id)
    setForm({
      name: s.name,
      host: s.host,
      hostV6: s.host_v6,
      port: s.port,
      user: s.user,
      authKind: s.auth_kind,
      kind: s.kind,
      notes: s.notes,
      credential: '',
      egressFamily: s.egress_family,
      shareDomain: s.share_domain,
    })
    setExpand({ type: 'edit', id: s.id })
  }

  // 终态处置：测速终结刷新服务器列表（持久化结果上屏），对测终结另刷吞吐列表。
  // 事件丢失由注册表那个唯一的 5s 对账兜底，这里不再各起一个定时器。
  // 部署任务也在这里收：向导可能已经收起或用户已经切页，卡片上的「已部署协议」
  // 仍要立刻对上后端（目标首位就是被部署的服务器 id）。
  useJobTerminal('deploy', (job) => {
    // 自动重部署没有向导兜着，失败原因要上屏，否则只在行内闪一下就没了
    if (job.error) setError(job.error)
    const id = Number(job.targets[0])
    if (Number.isNaN(id)) return
    void loadDeployed([id])
    void refresh()
  })
  useJobTerminal('server_speed', (job) => {
    if (job.error) setError(job.error)
    void refresh()
  })
  useJobTerminal('server_throughput', (job) => {
    if (job.error) setError(job.error)
    void refresh()
    void loadThroughput()
  })

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold">服务器管理</h2>
        <button type="button" onClick={() => { setEditingId(null); setForm(EMPTY_SERVER_FORM); setExpand(null) }} className="rounded bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500">
          新建服务器
        </button>
      </div>

      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      <ServerTools
        open={toolsOpen}
        onToggle={() => setToolsOpen((v) => !v)}
        concurrency={concurrency}
        onConcurrencyChange={setConcurrency}
        serverCount={servers.length}
        runningCount={speedJobs.size}
        onBatch={handleBatchSpeedTest}
        onCancelSpeed={() => void handleCancelSpeed()}
        servers={servers}
        fromId={fromId}
        toId={toId}
        seconds={seconds}
        onFromChange={setFromId}
        onToChange={setToId}
        onSecondsChange={setSeconds}
        throughputRunning={throughputJob != null}
        progressMessage={throughputJob?.message ?? ''}
        onThroughputStart={handleThroughputStart}
        onThroughputCancel={() => void handleCancelThroughput()}
        installBusy={installBusy}
        installMsg={installMsg}
        onInstall={() => void handleInstallIperf3()}
        results={throughputResults}
      />

      {form && editingId == null && (
        <div className="space-y-2 rounded-lg border border-neutral-800 bg-neutral-900/50 p-4">
          <ServerFormPanel
            form={form}
            editing={false}
            onChange={setForm}
            onSave={handleSave}
            onCancel={() => setForm(null)}
          />
        </div>
      )}

      <ServerList
        servers={servers}
        speedJobs={speedJobs}
        deployJobs={deployJobs}
        testing={testing}
        addrBusy={addrBusy}
        pwBusy={pwBusy}
        pwDone={pwDone}
        bbrAlgo={bbrAlgo}
        bbrChecking={bbrChecking}
        bbrEnabling={bbrEnabling}
        bbrMsg={bbrMsg}
        deployedProtos={deployedProtos}
        tests={tests}
        linkSummary={linkSummary}
        moreOpen={moreOpen}
        channelsOpen={channelsOpen}
        upstreamsOpen={upstreamsOpen}
        expand={expand}
        form={form}
        setForm={setForm}
        onToggleMore={handleToggleMore}
        onToggleChannels={(id) => toggleSet(setChannelsOpen, id)}
        onToggleUpstreams={(id) => toggleSet(setUpstreamsOpen, id)}
        onSpeedTest={handleSpeedTestOne}
        onTest={(id) => void handleTest(id)}
        onAddrFamily={(id, family) => void handleAddrFamily(id, family)}
        onPasswordless={(id) => void handlePasswordless(id)}
        onBbrEnable={(id) => void handleEnableBbr(id)}
        onBbrCheck={(id) => void handleCheckBbr(id)}
        onDeploy={(id) => setExpand({ type: 'deploy', id })}
        onEdit={startEdit}
        onDelete={(id) => void handleDelete(id)}
        onClose={() => setExpand(null)}
        onSave={handleSave}
        onCancelEdit={closeForm}
        onDeployDone={(s) => {
          // 部署终态：重读后端摘要（徽章不再由回调携带的协议清单镜像）
          setExpand(null)
          void refresh()
          void loadDeployed([s.id])
        }}
      />
    </div>
  )
}
