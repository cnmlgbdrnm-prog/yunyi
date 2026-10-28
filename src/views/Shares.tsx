// 分享视图：列出服务器上的分享链接、新建（选服务器 + 勾节点）、作废。
// 分享的真实状态在服务器上——后端定时逐台采集并经 `share://status` 推送，
// 前端挂载/切页只读 store，不再触发全量 SSH（列举一次实测 ~14 秒）。
// 手动「刷新」与增删改之后仍会即时采集（用户动作），结果写回 store 与后端缓存。
// 某台连不上时只列不出那几条，并把服务器名摆在明面上（见 unreachable 提示）。
import { Fragment, useCallback, useEffect, useState } from 'react'
import { deleteShare, listServers, listShares } from '../api'
import ConfirmButton from '../components/ConfirmButton'
import ShareEditPanel from '../components/ShareEditPanel'
import ShareWizard from '../components/ShareWizard'
import { copyText } from '../utils/clipboard'
import { formatShareBytes as fmtBytes } from '../utils/shareTraffic'
import {
  dismissShareCreation,
  useShareCreation,
} from './shareCreationStore'
import { applyShareStatus, useShareStatus } from './shareStatusStore'
import type { Server, ShareInfo } from '../api/types'

/** 单向配额条：0 = 不限，不画进度（但保留「不限」字样）。 */
function QuotaBar({
  label,
  used,
  quota,
  ariaLabel,
}: {
  label: string
  used: number
  quota: number
  ariaLabel: string
}) {
  const pct = quota > 0 ? Math.min(100, Math.round((used / quota) * 100)) : 0
  return (
    <div className="space-y-0.5">
      <div className="text-xs text-neutral-400">
        {label} {fmtBytes(used)} / {quota > 0 ? fmtBytes(quota) : '不限'}
      </div>
      <div
        role="progressbar"
        aria-label={ariaLabel}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct}
        className="h-1.5 w-full rounded bg-neutral-800"
      >
        <div
          style={{ width: `${pct}%` }}
          className={
            pct >= 100
              ? 'h-1.5 rounded bg-amber-500'
              : 'h-1.5 rounded bg-emerald-500'
          }
        />
      </div>
    </div>
  )
}

function statusView(s: ShareInfo): { label: string; className: string } {
  switch (s.status) {
    case 'active':
      return { label: '运行中', className: 'text-emerald-400' }
    case 'blocked_quota':
      return { label: '已超额', className: 'text-amber-400' }
    case 'blocked_expired':
      return { label: '已到期', className: 'text-amber-400' }
    case 'degraded':
      return { label: '异常', className: 'text-red-400' }
    case 'legacy':
      return { label: '旧版·无管控', className: 'text-neutral-400' }
  }
}

/** 会话级快照已移入后端轮询；这里只格式化「更新于」的时刻。 */
function formatClock(at: string): string | null {
  const d = new Date(at)
  if (Number.isNaN(d.getTime())) return null
  return d.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })
}

export default function Shares() {
  // 状态来自后端定时采集（App 启动时读一次缓存并订阅 share://status）：
  // 本页挂载/切页都不再发 SSH。
  const status = useShareStatus()
  const shares = status?.shares ?? []
  const unreachable = status?.unreachable ?? []
  const [servers, setServers] = useState<Server[]>([])
  const [error, setError] = useState<string | null>(null)
  const [refreshing, setRefreshing] = useState(false)

  // 新建向导与编辑面板
  const [wizardOpen, setWizardOpen] = useState(false)
  const [editing, setEditing] = useState<ShareInfo | null>(null)
  // 创建任务在模块级 store 里：切页卸载不会丢，成功结果也不依赖本组件还活着。
  const creation = useShareCreation()

  // 手动刷新：唯一会从本页触发全量 SSH 的路径（用户点击/增删改之后）。
  const refresh = useCallback(async () => {
    setError(null)
    setRefreshing(true)
    try {
      applyShareStatus(await listShares())
    } catch (e) {
      setError(String(e))
    } finally {
      setRefreshing(false)
    }
  }, [])

  // 后端尚未完成第一轮（checked_at=null）时不能渲染「还没有分享」：那不是在说
  // 事实，而是在说「还没读到」。已完成后空列表才表示真的没有分享。
  const loading = status == null || status.checked_at === null
  const updatedAt = status?.checked_at ? formatClock(status.checked_at) : null

  useEffect(() => {
    // 只读本地库（服务器清单）；分享状态由后端推送。切页可能在返回前发生：
    // 卸载后再 setState 只会在测试里留下 act 警告，对界面毫无意义。
    let alive = true
    void listServers()
      .then((list) => {
        if (alive) setServers(list)
      })
      .catch((e) => {
        if (alive) setError(String(e))
      })
    return () => {
      alive = false
    }
  }, [])

  // 复制失败必须说出来：剪贴板 API 的拒绝是静默的，不说用户就只会看到「点了没反应」
  const handleCopy = async (url: string) => {
    setError(null)
    if (await copyText(url)) return
    setError('复制失败：剪贴板不可用。请手动选中链接复制')
  }

  const handleDelete = async (s: ShareInfo) => {
    setError(null)
    try {
      await deleteShare(s.server_id, s.id)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <h2 className="text-xl font-semibold">分享</h2>
        <button
          type="button"
          disabled={refreshing}
          onClick={() => void refresh()}
          className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-sky-300 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {refreshing ? '刷新中…' : '刷新'}
        </button>
        {updatedAt && (
          <span className="text-xs text-neutral-500">更新于 {updatedAt}</span>
        )}
      </div>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      {/* 说明分享凭证、订阅快照、格式和流量单位。 */}
      <div className="space-y-1 text-xs text-neutral-500">
        <p>新分享强制 HTTPS；链接即凭证，泄露即可使用这些节点。</p>
        <p>快照：内容固定在生成那一刻，改节点或重新部署后需重新生成。</p>
        <p>同时提供 mihomo（YAML）与 sing-box（JSON）；旧版分享仍为明文 HTTP。</p>
        <p>1 GB = 10 亿字节。只按公网双向总额度停用；上行、下行只展示用量明细，不再单独限额。</p>
      </div>

      {unreachable.length > 0 && (
        <p className="rounded border border-neutral-700 bg-neutral-900 px-3 py-2 text-xs text-neutral-400">
          读不到这些服务器上的分享，列表可能不全：{unreachable.join('、')}
        </p>
      )}

      <button
        type="button"
        onClick={() => setWizardOpen(true)}
        className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500"
      >
        新建分享
      </button>

      {wizardOpen && (
        <ShareWizard
          servers={servers}
          onClose={() => setWizardOpen(false)}
          onCreated={() => setWizardOpen(false)}
        />
      )}

      {creation?.status === 'running' && (
        <p
          role="status"
          className="rounded border border-sky-800 bg-sky-950/40 px-3 py-2 text-sm text-sky-300"
        >
          正在生成分享（{creation.serverName}）… 切换到其他页面不会中断。
        </p>
      )}

      {creation?.status === 'done' && creation.result && (
        <div className="flex items-start gap-2 rounded border border-emerald-800 bg-emerald-950/40 px-3 py-2 text-sm text-emerald-300">
          <p className="min-w-0 flex-1">
            已生成：<code className="font-mono">{creation.result.url}</code>
            {creation.result.url_singbox && (
              <>
                {' · '}
                <code className="font-mono">{creation.result.url_singbox}</code>
              </>
            )}
          </p>
          <button
            type="button"
            onClick={dismissShareCreation}
            className="rounded bg-emerald-900/60 px-2 py-0.5 text-xs text-emerald-200 hover:bg-emerald-900"
          >
            关闭
          </button>
        </div>
      )}

      {creation?.status === 'error' && (
        <div
          role="alert"
          className="flex items-start gap-2 rounded border border-red-800 bg-red-950/40 px-3 py-2 text-sm text-red-300"
        >
          <p className="min-w-0 flex-1">生成分享失败：{creation.error}</p>
          <button
            type="button"
            onClick={dismissShareCreation}
            className="rounded bg-red-900/60 px-2 py-0.5 text-xs text-red-200 hover:bg-red-900"
          >
            关闭
          </button>
        </div>
      )}

      <div className="overflow-x-auto rounded border border-neutral-800">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-neutral-800 text-left text-neutral-400">
              <th className="px-3 py-2">链接</th>
              <th className="px-3 py-2">节点</th>
              <th className="px-3 py-2">服务器</th>
              <th className="px-3 py-2">已用 / 配额</th>
              <th className="px-3 py-2">速率</th>
              <th className="px-3 py-2">到期</th>
              <th className="px-3 py-2">状态</th>
              <th className="px-3 py-2">操作</th>
            </tr>
          </thead>
          <tbody>
            {shares.map((s) => {
              const view = statusView(s)
              const pct =
                s.quota_bytes > 0
                  ? Math.min(100, Math.round((s.used_bytes / s.quota_bytes) * 100))
                  : 0
              return (
                <Fragment key={`${s.server_id}-${s.id}`}>
                  <tr className="border-b border-neutral-900">
                  <td className="px-3 py-2 align-top">
                    <div className="space-y-1">
                      <div className="break-all font-mono text-xs">{s.url}</div>
                      <div className="flex flex-wrap gap-1">
                        <button
                          type="button"
                          onClick={() => void handleCopy(s.url)}
                          className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700"
                        >
                          复制链接
                        </button>
                        {!s.legacy && s.url_singbox && (
                          <button
                            type="button"
                            onClick={() => void handleCopy(s.url_singbox)}
                            className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700"
                          >
                            复制 sing-box
                          </button>
                        )}
                      </div>
                    </div>
                  </td>
                  <td className="px-3 py-2 align-top text-neutral-400">
                    {s.node_names.join('、')}
                  </td>
                  <td className="px-3 py-2 align-top text-neutral-400">{s.server_name}</td>
                  <td className="px-3 py-2 align-top">
                    {s.quota_total_bytes != null && (
                      <div className="mb-3 w-56 space-y-1 border-b border-neutral-700 pb-2">
                        {s.total_usage?.total_used_bytes != null ? (
                          <QuotaBar label="总额度" used={s.total_usage.total_used_bytes}
                            quota={s.quota_total_bytes} ariaLabel="总额度已用配额" />
                        ) : <p className="text-xs text-amber-400">总用量待核实，不能按 0 计算</p>}
                        {s.total_usage && (
                          <>
                            <p className="text-xs text-neutral-500">历史折算：{fmtBytes(s.total_usage.history_charge_bytes)}（原上下行合计 ×2）</p>
                            <p className="text-xs text-neutral-500">新增公网：入站 {fmtBytes(s.total_usage.public_rx_bytes)} · 出站 {fmtBytes(s.total_usage.public_tx_bytes)}</p>
                          </>
                        )}
                        {s.total_usage?.state === 'error' && <p className="text-xs text-red-400">{s.total_usage.detail ?? '公网计量异常，已停止分享'}</p>}
                      </div>
                    )}
                    {s.legacy ? (
                      <span className="text-neutral-500">—</span>
                    ) : s.split_quota ? (
                      <div className="w-44 space-y-2">
                        <QuotaBar
                          label="↑ 上行"
                          used={s.used_up_bytes}
                          quota={s.quota_up_bytes}
                          ariaLabel="上行已用配额"
                        />
                        <QuotaBar
                          label="↓ 下行"
                          used={s.used_down_bytes}
                          quota={s.quota_down_bytes}
                          ariaLabel="下行已用配额"
                        />
                      </div>
                    ) : (
                      <div className="w-32 space-y-1">
                        <div className="text-xs text-neutral-400">
                          {fmtBytes(s.used_bytes)} / {fmtBytes(s.quota_bytes)}
                        </div>
                        <div
                          role="progressbar"
                          aria-label="已用配额"
                          aria-valuemin={0}
                          aria-valuemax={100}
                          aria-valuenow={pct}
                          className="h-1.5 w-full rounded bg-neutral-800"
                        >
                          <div
                            style={{ width: `${pct}%` }}
                            className={
                              s.status === 'blocked_quota' || s.status === 'blocked_expired'
                                ? 'h-1.5 rounded bg-amber-500'
                                : 'h-1.5 rounded bg-emerald-500'
                            }
                          />
                        </div>
                      </div>
                    )}
                  </td>
                  <td className="px-3 py-2 align-top text-xs text-neutral-400">
                    {s.legacy
                      ? '—'
                      : `↑${s.rate_up_mbps || '不限'} ↓${s.rate_down_mbps || '不限'} Mbps`}
                  </td>
                  <td className="px-3 py-2 align-top text-xs text-neutral-400">
                    {s.legacy || !s.expires_at ? '—' : s.expires_at.slice(0, 10)}
                  </td>
                  <td className="px-3 py-2 align-top">
                    <span className={view.className} title={s.status_detail ?? undefined}>
                      {view.label}
                    </span>
                    {s.last_access_at && (
                      <div className="text-xs text-neutral-500" title={s.last_access_ip ?? ''}>
                        最近取用 {s.last_access_at.slice(0, 10)}
                      </div>
                    )}
                  </td>
                  <td className="px-3 py-2 align-top">
                    <div className="flex items-center gap-2">
                      {!s.legacy && (
                        <button
                          type="button"
                          onClick={() => setEditing(s)}
                          className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700"
                        >
                          编辑
                        </button>
                      )}
                      <ConfirmButton
                        label="作废"
                        confirmLabel="确认作废"
                        busyLabel="作废中…"
                        busy={false}
                        onConfirm={() => void handleDelete(s)}
                      />
                    </div>
                  </td>
                  </tr>
                  {/* 编辑框必须紧跟被编辑的这一行：放到表格上方会让用户来回找，
                      尤其列表滚动后根本不知道改的是哪条。 */}
                  {editing &&
                    editing.server_id === s.server_id &&
                    editing.id === s.id && (
                      <tr className="border-b border-neutral-900 bg-neutral-950/40">
                        <td colSpan={8} className="px-3 py-3">
                          <ShareEditPanel
                            share={editing}
                            onClose={() => setEditing(null)}
                            onSaved={() => {
                              setEditing(null)
                              void refresh()
                            }}
                          />
                        </td>
                      </tr>
                    )}
                </Fragment>
              )
            })}
          </tbody>
        </table>
        {shares.length === 0 && (
          <p className="p-4 text-sm text-neutral-500">
            {loading ? '读取中…' : error ? '读取失败，列表未知。' : '还没有分享。'}
          </p>
        )}
      </div>
    </div>
  )
}
