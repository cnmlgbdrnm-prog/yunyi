// 中转链路视图：链路列表（读库持久）+ 状态刷新 + 拆链/删除 + 建链向导入口。
// 状态从 refreshLinkStatus 结果派生，缺失时显示「状态未知」兜底；拆链为 link
// 类后台任务，状态与进度取自模块级注册表（jobs/registry，切页不丢、可取消），
// 终态后刷新列表并给出行内提示。行渲染在 components/LinkRow，向导在 LinkWizard。
import { useCallback, useEffect, useRef, useState } from 'react'
import { cancelJob, deleteLinkRule, listRelayRules, listServers, readoptLink, refreshLinkStatus, teardownLink } from '../api'
import type { LinkStatus, RelayRule, Server } from '../api/types'
import { bindJob, isTargetBusy, jobForTarget, progressView, release, reserveJob, useJobTerminal, useTrackedJobs } from '../jobs/registry'
import LinkRow from '../components/LinkRow'
import LinkWizard from '../components/LinkWizard'
import { deleteCache, readCache, seedStatuses, STATUS_POLL_MS, writeCache } from './relayStatusCache'

// 拆链终态文案在**提交那一刻**定格：取决于当时的动作（解除登记 / 拆除接口 / 拆链）
// 与链路当时的状态（imported 才有「接口」可拆），事后无从还原。
// 放模块级是因为终态事件可能在切页之后才到，那时视图状态已经重建过一次；
// 量级极小（每条链至多一条），终态取用后即删。与 relayStatusCache 同一手法。
const teardownOutcome = new Map<number, string>()

export default function Relay() {
  const [rules, setRules] = useState<RelayRule[]>([])
  const [servers, setServers] = useState<Server[]>([])
  // 初始状态播种自模块级缓存：30s 内切回本页零查询直接展示
  const [statuses, setStatuses] = useState<Record<number, LinkStatus>>(() => seedStatuses())
  const [refreshing, setRefreshing] = useState<Set<number>>(new Set())
  const [error, setError] = useState<string | null>(null)
  const [success, setSuccess] = useState<string | null>(null)
  const [wizardOpen, setWizardOpen] = useState(false)
  const [expandedRuleId, setExpandedRuleId] = useState<number | null>(null)
  const [deleting, setDeleting] = useState<Set<number>>(new Set())
  const [readopting, setReadopting] = useState<Set<number>>(new Set())
  // 拆链任务态：目标即 rule id，状态与分步进度都在注册表里（切页不丢）
  const teardownJobs = useTrackedJobs('link')
  const [rowSuccess, setRowSuccess] = useState<Record<number, string>>({})
  const [rowError, setRowError] = useState<Record<number, string>>({})

  const serverById = useCallback((id: number): Server | undefined => servers.find((s) => s.id === id), [servers])

  // 对 built/imported 链路查询一轮状态；失败也写入错误状态（展示「状态未知」）。
  // 结果回写模块级缓存：切页回来 30s 内零查询。
  const doRefreshStatus = useCallback(async (ruleId: number) => {
    setRefreshing((prev) => new Set(prev).add(ruleId))
    try {
      const st = await refreshLinkStatus(ruleId)
      writeCache(ruleId, st)
      setStatuses((prev) => ({ ...prev, [ruleId]: st }))
    } catch (e) {
      const errorStatus: LinkStatus = {
        ok: false,
        relay_handshake_secs_ago: null,
        landing_handshake_secs_ago: null,
        relay_rx_bytes: 0,
        relay_tx_bytes: 0,
        landing_rx_bytes: 0,
        landing_tx_bytes: 0,
        checked_at: '',
        error: String(e),
      }
      writeCache(ruleId, errorStatus)
      setStatuses((prev) => ({ ...prev, [ruleId]: errorStatus }))
    } finally {
      setRefreshing((prev) => {
        const next = new Set(prev)
        next.delete(ruleId)
        return next
      })
    }
  }, [])

  /**
   * 一轮状态补查：只查缓存过期/缺失的 built|imported 链路，新鲜的一律跳过（零查询）。
   *
   * 链路清单**显式传入**而不是读 rulesRef：调用方都刚拿到一份确切的清单，
   * 传参省掉一次「ref 是否已经跟上」的时序推理。
   */
  const sweepStale = useCallback(
    (list: RelayRule[]) => {
      const staleIds = list
        .filter((r) => r.status === 'built' || r.status === 'imported')
        .filter((r) => readCache(r.id, STATUS_POLL_MS) == null)
        .map((r) => r.id)
      if (staleIds.length === 0) return
      void Promise.allSettled(staleIds.map((id) => doRefreshStatus(id)))
    },
    [doRefreshStatus],
  )

  const refresh = useCallback(async (): Promise<RelayRule[]> => {
    try {
      const [rs, ss] = await Promise.all([listRelayRules(), listServers()])
      setRules(rs)
      setServers(ss)
      // 链路集合变了就地补一轮：新建/接管的链路没有缓存，不补要等下一个 30s 轮询
      // 才出现状态，那段「状态未知」是假的。放在这里、而不是靠「effect 依赖 rules」
      // 触发——后者每次刷新都会把定时器拆掉重建（见下面轮询 effect 的注释）。
      sweepStale(rs)
      return rs
    } catch (e) {
      setError(String(e))
      return []
    }
  }, [sweepStale])

  // 挂载后装载规则/服务器列表（本地读库，不走 SSH）
  useEffect(() => {
    void refresh()
  }, [refresh])

  // 状态轮询：每 STATUS_POLL_MS 补查一轮缓存过期的链路，视图切走即卸载、定时器随之清除。
  // 手动「刷新状态」按钮即时可用，与轮询互不阻塞。
  //
  // **依赖里不放 rules**：body 读的是 rulesRef.current，且 rules 每次 refresh() 都是新
  // 数组——放进去等于每次列表刷新（拆链完成、删除、重新接管…）都把定时器拆掉重建，
  // 下一轮轮询随之顺延，最坏白等一个周期。「新链路要立刻补查」由 refresh() 里的
  // sweepStale 负责，与定时器解耦。
  const rulesRef = useRef<RelayRule[]>(rules)
  rulesRef.current = rules
  useEffect(() => {
    const timer = setInterval(() => sweepStale(rulesRef.current), STATUS_POLL_MS)
    return () => clearInterval(timer)
  }, [sweepStale])

  // 拆链终态：失败标红给出原因，成功按提交时定格的文案提示并清掉该行残留状态，
  // 两种情况都刷新列表。事件丢失由注册表那个唯一的 5s 对账兜底。
  useJobTerminal('link', (job) => {
    const rid = Number(job.targets[0])
    if (Number.isNaN(rid)) return
    const successText = teardownOutcome.get(rid)
    teardownOutcome.delete(rid)
    if (job.error) {
      setRowError((prev) => ({ ...prev, [rid]: job.error ?? '拆链失败' }))
      return
    }
    setRowSuccess((prev) => ({ ...prev, [rid]: successText ?? '拆链完成' }))
    // 拆链后行变 not-built 不再查询状态：清掉残留握手/传输缓存，避免旧数据常驻误导。
    // **模块级缓存也要清**：只清视图状态的话，条目会一直留到 30s 过期，切页重挂时
    // seedStatuses 又把它播种回来——「刚拆掉的链路显示着拆之前的握手」。
    // 拆链失败不进这里（上面 error 分支已 return），那时链路还是好的，缓存该留着。
    deleteCache(rid)
    setStatuses((prev) => {
      const next = { ...prev }
      delete next[rid]
      return next
    })
    void refresh()
  })

  const clearRowError = useCallback((id: number) => {
    setRowError((prev) => {
      const next = { ...prev }
      delete next[id]
      return next
    })
  }, [])

  const handleRefreshStatus = useCallback(
    (ruleId: number) => {
      setRowSuccess((prev) => {
        const next = { ...prev }
        delete next[ruleId]
        return next
      })
      void doRefreshStatus(ruleId)
    },
    [doRefreshStatus],
  )

  const handleTeardown = async (ruleId: number, action: 'unregister' | 'full') => {
    setError(null)
    clearRowError(ruleId)
    // 终态文案在此定格（动作 + 当时的链路状态）：unregister → 已解除登记；
    // full + imported → 已拆除接口；full + built → 已拆链
    const wasImported = rules.find((r) => r.id === ruleId)?.status === 'imported'
    teardownOutcome.set(
      ruleId,
      action === 'unregister' ? '已解除登记' : wasImported ? '已拆除接口' : '已拆链',
    )
    // 占位先行：IPC 回来之前这一行就进入「拆链中…」，连点堆不出一串任务
    const ticket = reserveJob('link', [String(ruleId)])
    try {
      bindJob(ticket, await teardownLink(ruleId, action))
    } catch (e) {
      release(ticket)
      teardownOutcome.delete(ruleId)
      setRowError((prev) => ({ ...prev, [ruleId]: String(e) }))
    }
  }

  const handleCancelTeardown = async (ruleId: number) => {
    const job = jobForTarget(teardownJobs, String(ruleId))
    if (job != null) {
      if (job.id >= 0) {
        try {
          await cancelJob(job.id)
        } catch (e) {
          setRowError((prev) => ({ ...prev, [ruleId]: String(e) }))
        }
      }
      release(job.id)
    }
    teardownOutcome.delete(ruleId)
    void refresh()
  }

  const handleReadopt = async (ruleId: number) => {
    setError(null)
    clearRowError(ruleId)
    setReadopting((prev) => new Set(prev).add(ruleId))
    try {
      await readoptLink(ruleId)
      setRowSuccess((prev) => ({ ...prev, [ruleId]: '已重新接管' }))
      // 状态补查由 refresh() 就地扫一轮（见 sweepStale 的注释）
      void refresh()
    } catch (e) {
      setRowError((prev) => ({ ...prev, [ruleId]: String(e) }))
    } finally {
      setReadopting((prev) => {
        const next = new Set(prev)
        next.delete(ruleId)
        return next
      })
    }
  }

  const handleDelete = async (ruleId: number, name: string) => {
    setError(null)
    setDeleting((prev) => new Set(prev).add(ruleId))
    try {
      await deleteLinkRule(ruleId)
      setSuccess(`链路「${name}」已删除`)
      // 行已删除：状态缓存一并清除（同拆链清理语义，防止残留数据泄漏）
      deleteCache(ruleId)
      setStatuses((prev) => {
        const next = { ...prev }
        delete next[ruleId]
        return next
      })
      void refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setDeleting((prev) => {
        const next = new Set(prev)
        next.delete(ruleId)
        return next
      })
    }
  }

  // 建链/接管完成：关闭向导并重载列表；新链路的状态由 refresh() 就地补查一轮
  const handleWizardDone = () => {
    setWizardOpen(false)
    void refresh()
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold">中转链路</h2>
        <button type="button" onClick={() => setWizardOpen(true)} className="rounded bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500">
          新建链路
        </button>
      </div>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}
      {success && <p className="text-sm text-emerald-400">{success}</p>}

      <ul className="space-y-2">
        {rules.map((r) => (
          <LinkRow
            key={r.id}
            rule={r}
            relay={serverById(r.relay_server_id)}
            landing={serverById(r.landing_server_id)}
            status={statuses[r.id]}
            refreshing={refreshing.has(r.id)}
            teardownActive={isTargetBusy(teardownJobs, String(r.id))}
            teardown={progressView(jobForTarget(teardownJobs, String(r.id)))}
            deleting={deleting.has(r.id)}
            readopting={readopting.has(r.id)}
            expanded={expandedRuleId === r.id}
            rowSuccess={rowSuccess[r.id]}
            rowError={rowError[r.id]}
            onRefresh={() => handleRefreshStatus(r.id)}
            onReadopt={() => void handleReadopt(r.id)}
            onTeardown={(action) => void handleTeardown(r.id, action)}
            onCancelTeardown={() => void handleCancelTeardown(r.id)}
            onDelete={() => void handleDelete(r.id, r.name)}
            onToggleDevices={() => setExpandedRuleId(expandedRuleId === r.id ? null : r.id)}
          />
        ))}
        {rules.length === 0 && <p className="text-sm text-neutral-500">还没有链路，点击「新建链路」开始。</p>}
      </ul>

      {wizardOpen && <LinkWizard servers={servers} onClose={() => setWizardOpen(false)} onDone={() => void handleWizardDone()} />}
    </div>
  )
}
