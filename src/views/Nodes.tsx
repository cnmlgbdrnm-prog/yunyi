// 节点管理视图：节点表格、名称筛选、启用开关、删除与测速（单发/批量）。
// 输入：api/listNodes/setNodeEnabled/deleteNode/speedTest/cancelJob；
// 输出：表格 UI 与 IPC 联动。任务状态一律取自模块级注册表（jobs/registry）：测速行级
// 结果按 job 分桶（targetResult，node_id 即目标 id），隐私任务按 node_id 认忙碌态；
// 终态才 refresh() 拉持久化结果，失败原因在终态 error 中展示。带宽为字节/秒，
// 按 MB/s 两位小数展示；上次测速时间为本地时区短格式（src/utils/checkedAtFormat）。
import { useCallback, useEffect, useState } from 'react'
import {
  cancelJob,
  deleteNode,
  disablePrivacy,
  enablePrivacy,
  listNodes,
  listPrivacies,
  listRelayRules,
  listServers,
  renameNode,
  setNodeEnabled,
  speedTest,
  toggleNodeRelay,
} from '../api'
import type { Node, Privacy, RelayRule, Server } from '../api/types'
import {
  bindJob,
  cancelableJobIds,
  isTargetBusy,
  release,
  reserveJob,
  targetResult,
  useJobTerminal,
  useTrackedJobs,
} from '../jobs/registry'
import LinkWizard from '../components/LinkWizard'
import NodeRow from './nodes/NodeRow'

const CONCURRENCY_OPTIONS = [1, 3, 5]

export default function Nodes() {
  const [nodes, setNodes] = useState<Node[]>([])
  const [filter, setFilter] = useState('')
  const [error, setError] = useState<string | null>(null)
  // 并发数（1/3/5，默认 3）：单发与批量共用
  const [concurrency, setConcurrency] = useState(3)
  // 进行中的测速任务：多 job 并行跟踪，状态在注册表里（切页不丢；见 jobs/registry 头注释）。
  // 行级结果按 job 分桶，一个任务终结只带走自己那一桶。
  const speedJobs = useTrackedJobs('speed')
  // 可用中转规则（已建成 imported/built）：无规则时隐藏「中转」列与开关
  const [relayRules, setRelayRules] = useState<RelayRule[]>([])
  // 服务器列表（只为「补全中转」：按节点地址找回它的落地机，再决定预填哪对机器）
  const [servers, setServers] = useState<Server[]>([])
  // 「补全中转」入口选中的落地机（非空则展示建链向导，预填整机转发）
  const [completing, setCompleting] = useState<{ landingId: number; relayId: number | null } | null>(
    null,
  )
  // 多选子集测速：勾选任意节点 → 「测速选中 (N)」按并发窗口并行（后台 chunks(concurrency)）
  const [selected, setSelected] = useState<Set<string>>(new Set())
  // 就地改名：同一时间只允许一行处于编辑态
  const [editingName, setEditingName] = useState<{ id: string; draft: string } | null>(null)
  const [savingName, setSavingName] = useState(false)
  // 隐私保护开关（Tor 入口）：一条记录 = 某条线的开关。**含未生效的**
  // （onion_host 为空 + last_check_error）——那正是「开着但没起来」，要显示成红字
  // 而不是变回「没开」，否则用户以为链路是干净的。
  const [privacies, setPrivacies] = useState<Privacy[]>([])
  // 进行中的隐私任务（部署要装 Tor、等 onion 地址，是慢活）：目标即 node id，状态在注册表
  const privacyJobs = useTrackedJobs('privacy')

  const refresh = useCallback(async () => {
    try {
      const [ns, ps] = await Promise.all([
        listNodes(),
        // 隐私开关读失败不该把节点列表一起打掉：没读到就按「都没开」显示
        listPrivacies().catch(() => [] as Privacy[]),
      ])
      setNodes(ns)
      setPrivacies(ps)
    } catch (e) {
      setError(String(e))
    }
  }, [])

  useEffect(() => {
    void refresh()
    void listRelayRules()
      .then(setRelayRules)
      .catch(() => setRelayRules([]))
    void listServers()
      .then(setServers)
      .catch(() => setServers([]))
  }, [refresh])

  // 节点直连/中转开关（含挂载时对规则可用性的校验由后端做）
  const relayAvailable = relayRules.some(
    (r) => r.converged && (r.status === 'imported' || r.status === 'built'),
  )

  // 这条规则能不能给「落地机 + 端口」用。判据与后端 relay::attach_deployed_nodes 一致：
  // 建成/接管 + 收敛，且端口对得上（整机转发则整机所有端口都算对得上）。
  const ruleCovers = (r: RelayRule, landingId: number, port: number) =>
    r.landing_server_id === landingId &&
    r.converged &&
    (r.status === 'imported' || r.status === 'built') &&
    (r.forward_mode === 'same_port' || r.landing_port === port)

  // 节点地址找回落地机（自建节点的 addr 就是落地机 host）
  const landingOf = (n: Node): Server | undefined =>
    servers.find((s) => s.kind === 'landing' && s.host === n.addr)

  // 该节点「没挂中转、且这台落地机根本没有可用链路」→ 需要一键补全。
  // 只是没开中转开关（链路在）不算，那种情况点「中转」即可。
  const needsCompletion = (n: Node): boolean => {
    if (n.relay_rule_id != null) return false
    const landing = landingOf(n)
    if (!landing) return false
    return !relayRules.some((r) => ruleCovers(r, landing.id, n.port))
  }

  // 「中转」列的显示条件不能只看 relayAvailable：正是没链路时才需要这个入口
  const showRelayCol = relayAvailable || nodes.some(needsCompletion)
  const handleToggleRelay = async (id: string) => {
    setError(null)
    try {
      await toggleNodeRelay(id)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  // 这条线的隐私开关记录（没有就是没开）。
  const privacyOf = (n: Node): Privacy | undefined => privacies.find((p) => p.node_id === n.id)

  const handleTogglePrivacy = async (n: Node) => {
    setError(null)
    const p = privacyOf(n)
    // 占位先行：IPC 回来之前这一行就显示「处理中…」，连点堆不出一串任务
    const ticket = reserveJob('privacy', [n.id])
    try {
      if (p?.onion_host && !p.last_check_error) {
        // 已有 onion 地址时，点击是关闭；清理远端完成后才会删记录。
        const jobId = await disablePrivacy(p.id)
        if (jobId == null) {
          release(ticket)
          return
        }
        bindJob(ticket, jobId)
      } else {
        // 地址为空或本机同步失败时再次点击应重试，不能把有效的远端 Onion 当作关闭。
        // 隐藏服务要开在**这条线的入口**那台机器上：直连线是落地机自己，
        // 挂了中转就是中转机——由后端按 effective_node 判定，前端只报这台落地机
        const landing = landingOf(n)
        if (!landing) {
          release(ticket)
          setError(`找不到「${n.name}」所在的服务器，无法开启隐私保护`)
          return
        }
        bindJob(ticket, await enablePrivacy(landing.id, n.id))
      }
      await refresh()
    } catch (e) {
      release(ticket)
      setError(String(e))
    }
  }

  const filtered = nodes.filter((n) => n.name.includes(filter))
  const enabledCount = nodes.filter((n) => n.enabled).length

  const handleToggle = async (id: string, enabled: boolean) => {
    setError(null)
    try {
      await setNodeEnabled(id, enabled)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  const handleDelete = async (id: string) => {
    setError(null)
    try {
      await deleteNode(id)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  const handleRenameSave = async (id: string) => {
    if (!editingName || savingName) return
    const name = editingName.draft.trim()
    if (!name) return
    setError(null)
    setSavingName(true)
    try {
      // 后端会剔除零宽字符并再次校验；报错（撞名/空名）落顶部错误条
      await renameNode(id, name)
      await refresh()
      setEditingName(null)
    } catch (e) {
      setError(String(e))
    } finally {
      setSavingName(false)
    }
  }

  const startSpeedTest = async (ids: string[]) => {
    setError(null)
    // 已被在跟踪任务覆盖的节点跳过：占位在 await **之前**提交，连点两次的第二下
    // 在这里就被挡下（评审 #6：原先忙碌态要等 IPC 回来才生效，那段时间是空的）
    const pending = ids.filter((id) => !isTargetBusy(speedJobs, id))
    if (pending.length === 0) return
    const ticket = reserveJob('speed', pending)
    try {
      bindJob(ticket, await speedTest(pending, concurrency))
    } catch (e) {
      release(ticket)
      setError(String(e))
    }
  }

  // 指定节点是否在任一被跟踪测速任务中（行内按钮忙态与禁用判定）
  const isSpeedBusy = (id: string) => isTargetBusy(speedJobs, id)

  const handleSpeedTestOne = (id: string) => {
    void startSpeedTest([id])
  }

  const toggleSelect = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  // 表头全选/与已选数量同步
  const allSelected = filtered.length > 0 && filtered.every((n) => selected.has(n.id))
  const toggleSelectAll = () => {
    setSelected(() => {
      if (allSelected) return new Set()
      return new Set(filtered.map((n) => n.id))
    })
  }

  const handleSelectedSpeedTest = () => {
    if (selected.size === 0) {
      setError('请先勾选要测速的节点')
      return
    }
    void startSpeedTest([...selected])
  }

  const handleBatchSpeedTest = () => {
    const ids = nodes.filter((n) => n.enabled).map((n) => n.id)
    if (ids.length === 0) {
      setError('没有启用节点，无法批量测速')
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

  // 终态处置：测速终结刷新节点列表（持久化结果上屏）。
  // 隐私任务只在终态刷新——中间步骤（装 Tor、等 onion、回连核验）都不该让列表跳动。
  // 事件丢失由注册表那个唯一的 5s 对账兜底，这里不再各起一个定时器。
  useJobTerminal('speed', (job) => {
    if (job.error) setError(job.error)
    void refresh()
  })
  useJobTerminal('privacy', (job) => {
    if (job.error) setError(job.error)
    void refresh()
  })

  const completeRelay = (n: Node) => {
    const landing = landingOf(n)
    if (!landing) return
    // 中转机预填：这台落地机已有链路用的那台；没有就用第一台
    const known = relayRules.find((r) => r.landing_server_id === landing.id)
    setCompleting({
      landingId: landing.id,
      relayId:
        known?.relay_server_id ?? servers.find((s) => s.kind === 'relay')?.id ?? null,
    })
  }

  return (
    <div className="space-y-4">
      <h2 className="text-xl font-semibold">节点管理</h2>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}
      {completing && (
        <LinkWizard
          servers={servers}
          initialRelayId={completing.relayId}
          initialLandingId={completing.landingId}
          initialForwardMode="same_port"
          onClose={() => setCompleting(null)}
          onDone={() => {
            // 建成后链路与节点挂载都会变：刷新两边，让「补全中转」原地变回「中转✓」
            setCompleting(null)
            void refresh()
            void listRelayRules()
              .then(setRelayRules)
              .catch(() => setRelayRules([]))
          }}
        />
      )}
      <div className="flex flex-wrap items-center gap-3">
        <input
          placeholder="按名称筛选"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="w-full max-w-xs rounded border border-neutral-700 bg-neutral-900 px-3 py-1.5 text-sm"
        />
        <select
          aria-label="并发数"
          value={concurrency}
          onChange={(e) => setConcurrency(Number(e.target.value))}
          className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1.5 text-sm"
        >
          {CONCURRENCY_OPTIONS.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
        <button
          type="button"
          onClick={handleBatchSpeedTest}
          disabled={enabledCount === 0}
          className="rounded bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          批量测速
        </button>
        {selected.size > 0 && (
          <button
            type="button"
            onClick={handleSelectedSpeedTest}
            className="rounded bg-indigo-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-indigo-500"
          >
            测速选中 ({selected.size})
          </button>
        )}
        {speedJobs.size > 0 && (
          <button
            type="button"
            onClick={() => void handleCancelSpeed()}
            className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-700"
          >
            取消测速
          </button>
        )}
      </div>
      <div className="overflow-x-auto rounded border border-neutral-800">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-neutral-800 text-left text-neutral-400">
              <th className="px-3 py-2">
                <input
                  type="checkbox"
                  aria-label="全选"
                  checked={allSelected}
                  onChange={toggleSelectAll}
                  className="accent-indigo-600"
                />
              </th>
              <th className="px-3 py-2">名称</th>
              <th className="px-3 py-2">协议</th>
              <th className="px-3 py-2">地址</th>
              <th className="px-3 py-2">分组</th>
              <th className="px-3 py-2">延迟</th>
              <th className="px-3 py-2">带宽</th>
              <th className="px-3 py-2">上次测速</th>
              <th className="px-3 py-2">失败原因</th>
              <th className="px-3 py-2">启用</th>
              <th className="px-3 py-2">隐私保护</th>
              {showRelayCol && <th className="px-3 py-2">中转</th>}
              <th className="px-3 py-2">操作</th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((n) => {
              const landing = landingOf(n)
              return (
                <NodeRow
                  key={n.id}
                  node={n}
                  selected={selected.has(n.id)}
                  onToggleSelect={() => toggleSelect(n.id)}
                  editingDraft={editingName?.id === n.id ? editingName.draft : null}
                  onEditingDraftChange={(draft) => setEditingName({ id: n.id, draft })}
                  onRenameSave={() => void handleRenameSave(n.id)}
                  onRenameCancel={() => setEditingName(null)}
                  savingName={savingName}
                  live={targetResult(speedJobs, n.id)}
                  privacy={privacyOf(n)}
                  privacyBusy={isTargetBusy(privacyJobs, n.id)}
                  onTogglePrivacy={() => void handleTogglePrivacy(n)}
                  showRelayCol={showRelayCol}
                  needsCompletion={needsCompletion(n)}
                  completionTitle={`${landing?.name ?? '该落地机'} 上还没有可用的转发链路，点这里建一条覆盖整机端口的`}
                  onCompleteRelay={() => completeRelay(n)}
                  relayOn={n.relay_rule_id != null}
                  onToggleRelay={() => void handleToggleRelay(n.id)}
                  speedBusy={isSpeedBusy(n.id)}
                  onSpeedTest={() => handleSpeedTestOne(n.id)}
                  onRenameStart={() => setEditingName({ id: n.id, draft: n.name })}
                  onDelete={() => void handleDelete(n.id)}
                  onToggleEnabled={() => handleToggle(n.id, !n.enabled)}
                />
              )
            })}
          </tbody>
        </table>
        {filtered.length === 0 && <p className="p-4 text-sm text-neutral-500">没有匹配的节点。</p>}
      </div>
    </div>
  )
}
