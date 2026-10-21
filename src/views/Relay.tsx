// 中转链路视图：链路列表（读库持久）+ 手动状态刷新 + 拆链/删除 + 建链向导入口。
// 状态由后端定时轮询并推送（stores 见 relayStatusStore），本页挂载不查询；
// 拆链为 link 类后台任务，状态与进度取自模块级注册表（jobs/registry，切页不丢、
// 可取消），终态后刷新列表并给出行内提示。行渲染在 components/LinkRow，向导在
// LinkWizard。
import { useCallback, useEffect, useState } from 'react'
import { cancelJob, deleteLinkRule, listRelayRules, listServers, readoptLink, refreshLinkStatus, teardownLink } from '../api'
import type { LinkStatus, RelayRule, Server } from '../api/types'
import { bindJob, isTargetBusy, jobForTarget, progressView, release, reserveJob, useJobTerminal, useTrackedJobs } from '../jobs/registry'
import LinkRow from '../components/LinkRow'
import LinkWizard from '../components/LinkWizard'
import {
  applyOneRelayStatus,
  forgetRelayStatus,
  useRelayStatuses,
} from './relayStatusStore'

// 拆链终态文案在**提交那一刻**定格：取决于当时的动作（解除登记 / 拆除接口 / 拆链）
// 与链路当时的状态（imported 才有「接口」可拆），事后无从还原。
// 放模块级是因为终态事件可能在切页之后才到，那时视图状态已经重建过一次；
// 量级极小（每条链至多一条），终态取用后即删。
const teardownOutcome = new Map<number, string>()

export default function Relay() {
  const [rules, setRules] = useState<RelayRule[]>([])
  const [servers, setServers] = useState<Server[]>([])
  // 状态来自后端定时轮询（App 启动时读一次缓存并订阅推送）：
  // 本页挂载/切页都不再发 SSH。
  const statuses = useRelayStatuses()
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

  // 手动「刷新状态」：唯一会从本页发起 SSH 的路径（用户明确点击）。
  // 结果同时写进 store 与后端缓存，切页回来仍是最新值。
  const doRefreshStatus = useCallback(async (ruleId: number) => {
    setRefreshing((prev) => new Set(prev).add(ruleId))
    try {
      const st = await refreshLinkStatus(ruleId)
      applyOneRelayStatus(ruleId, st)
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
      applyOneRelayStatus(ruleId, errorStatus)
    } finally {
      setRefreshing((prev) => {
        const next = new Set(prev)
        next.delete(ruleId)
        return next
      })
    }
  }, [])

  const refresh = useCallback(async (): Promise<RelayRule[]> => {
    try {
      const [rs, ss] = await Promise.all([listRelayRules(), listServers()])
      setRules(rs)
      setServers(ss)
      return rs
    } catch (e) {
      setError(String(e))
      return []
    }
  }, [])

  // 挂载后只装规则/服务器列表（本地读库）；状态由后端轮询推送，不在这里查。
  useEffect(() => {
    void refresh()
  }, [refresh])

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
    // 扩展端口行移除后整行会消失（refresh 之后），行内提示没机会被看到，
    // 改成顶部消息；其余动作的行还在，仍用行内提示。
    // 判据用提交时定格的文案而不是当前 rules：切页回来时该行已从库里读不到了。
    if (successText === '已移除扩展端口') {
      setSuccess(successText)
    } else {
      setRowSuccess((prev) => ({ ...prev, [rid]: successText ?? '拆链完成' }))
    }
    // 拆链后行变 not-built 不再查询状态：清掉 store 里的残留握手/传输，
    // 避免旧数据常驻误导。拆链失败不进这里（上面 error 分支已 return），
    // 那时链路还是好的，状态该留着。
    forgetRelayStatus(rid)
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

  const handleTeardown = async (
    ruleId: number,
    action: 'unregister' | 'full' | 'port',
  ) => {
    setError(null)
    setSuccess(null)
    clearRowError(ruleId)
    // 终态文案在此定格（动作 + 当时的链路状态）：unregister → 已解除登记；
    // port → 已移除扩展端口；full + imported → 已拆除接口；full + built → 已拆链
    const rule = rules.find((r) => r.id === ruleId)
    const wasImported = rule?.status === 'imported'
    teardownOutcome.set(
      ruleId,
      action === 'unregister'
        ? '已解除登记'
        : action === 'port'
          ? '已移除扩展端口'
          : wasImported
            ? '已拆除接口'
            : '已拆链',
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
      // 等后端下一轮轮询（≤60s）推送新状态，本页不发查询
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
      // 行已删除：状态一并清掉（同拆链清理语义，防止残留数据泄漏）
      forgetRelayStatus(ruleId)
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

  // 建链/接管完成：关闭向导并重载列表；新链路状态等后端下一轮推送
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
        {rules.map((r) => {
          // 同一对中转/落地机上的扩展端口挂在同一条隧道上；锚点链路要先清完它们
          // 才能整条拆（后端同样拒绝，这里把按钮先禁掉并说明原因）。
          const extendedSiblings = rules.filter(
            (x) =>
              x.forward_mode === 'extended' &&
              x.relay_server_id === r.relay_server_id &&
              x.landing_server_id === r.landing_server_id,
          ).length
          return (
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
              extendedSiblings={extendedSiblings}
              rowSuccess={rowSuccess[r.id]}
              rowError={rowError[r.id]}
              onRefresh={() => handleRefreshStatus(r.id)}
              onReadopt={() => void handleReadopt(r.id)}
              onTeardown={(action) => void handleTeardown(r.id, action)}
              onCancelTeardown={() => void handleCancelTeardown(r.id)}
              onDelete={() => void handleDelete(r.id, r.name)}
              onToggleDevices={() => setExpandedRuleId(expandedRuleId === r.id ? null : r.id)}
            />
          )
        })}
        {rules.length === 0 && <p className="text-sm text-neutral-500">还没有链路，点击「新建链路」开始。</p>}
      </ul>

      {wizardOpen && <LinkWizard servers={servers} onClose={() => setWizardOpen(false)} onDone={() => void handleWizardDone()} />}
    </div>
  )
}
