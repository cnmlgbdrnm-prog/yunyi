// 分享视图：列出服务器上的分享链接、新建（选服务器 + 勾节点）、作废。
// 分享的真实状态在服务器上——本地不存记录，每次进页面都向服务器重新拉；
// 但列表要跨视图切换保留一份**会话级内存快照**：读一次要 SSH 逐台服务器
// （实测 ~14 秒），每次切回来都空白「读取中…」既慢又像数据丢了。快照只活在
// 当前进程内存里（不落盘：链接本身就是凭证），进页面先显示旧列表，同时后台
// 刷新，服务器始终是唯一事实源。某台连不上时只列不出那几条，并把服务器名
// 摆在明面上（见 unreachable 提示）。
import { Fragment, useCallback, useEffect, useState } from 'react'
import { deleteShare, listServers, listShares } from '../api'
import ConfirmButton from '../components/ConfirmButton'
import ShareEditPanel from '../components/ShareEditPanel'
import ShareWizard from '../components/ShareWizard'
import { copyText } from '../utils/clipboard'
import type { Server, ShareInfo } from '../api/types'

const GB = 1024 ** 3

/** 用量展示：GB 保留一位小数；旧版分享没有配额，显示「—」。 */
function fmtBytes(n: number): string {
  if (n >= GB) return `${(n / GB).toFixed(1)} GB`
  if (n >= 1024 ** 2) return `${(n / 1024 ** 2).toFixed(1)} MB`
  if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${n} B`
}

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

/** 会话级快照：模块作用域，视图卸载后仍在；应用退出即丢，不落盘。 */
type ShareSnapshot = { shares: ShareInfo[]; unreachable: string[] }
let lastSnapshot: ShareSnapshot | null = null

/** 仅供测试清空快照，避免用例之间互相污染（生产代码不应调用）。 */
export function clearShareSnapshot() {
  lastSnapshot = null
}

export default function Shares() {
  // 有快照就先渲染旧列表，避免每次切回来都空白「读取中…」；
  // 真实数据仍然由下面的后台刷新拿服务器结果覆盖。
  const [shares, setShares] = useState<ShareInfo[]>(() => lastSnapshot?.shares ?? [])
  const [unreachable, setUnreachable] = useState<string[]>(
    () => lastSnapshot?.unreachable ?? [],
  )
  const [servers, setServers] = useState<Server[]>([])
  const [error, setError] = useState<string | null>(null)
  const [refreshing, setRefreshing] = useState(false)

  // 新建向导与编辑面板
  const [wizardOpen, setWizardOpen] = useState(false)
  const [editing, setEditing] = useState<ShareInfo | null>(null)
  const [created, setCreated] = useState<ShareInfo | null>(null)

  const refresh = useCallback(async () => {
    setRefreshing(true)
    try {
      const list = await listShares()
      // 只有成功结果才更新快照；失败时保留旧列表（服务器不可达不代表分享没了）
      lastSnapshot = { shares: list.shares, unreachable: list.unreachable }
      setShares(list.shares)
      setUnreachable(list.unreachable)
    } finally {
      setRefreshing(false)
    }
  }, [])

  // 首次取数期间**不能**渲染「还没有分享」：分享在服务器上，这台要 SSH 逐台读，
  // 实测一次 ~14 秒。那 14 秒里界面说「没有分享」就是在撒谎——用户切一下视图
  // 回来看到它，会以为分享被删了（2026-09-21 实际发生）。
  // 有会话快照时不再进入这个空窗：直接显示上次结果，后台刷新。
  const [loading, setLoading] = useState(() => lastSnapshot === null)

  useEffect(() => {
    void refresh()
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false))
    void listServers()
      .then(setServers)
      .catch((e) => setError(String(e)))
  }, [refresh])

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
        {refreshing && <span className="text-xs text-neutral-500">更新中…</span>}
      </div>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      {/* 只写用户要据此做决定的三件事（凭证 / 快照 / 格式），一句一条，不做解说、
         不加高亮。 */}
      <div className="space-y-1 text-xs text-neutral-500">
        <p>新分享强制 HTTPS；链接即凭证，泄露即可使用这些节点。</p>
        <p>快照：内容固定在生成那一刻，改节点或重新部署后需重新生成。</p>
        <p>同时提供 mihomo（YAML）与 sing-box（JSON）；旧版分享仍为明文 HTTP。</p>
      </div>

      {unreachable.length > 0 && (
        <p className="rounded border border-neutral-700 bg-neutral-900 px-3 py-2 text-xs text-neutral-400">
          读不到这些服务器上的分享，列表可能不全：{unreachable.join('、')}
        </p>
      )}

      <button
        type="button"
        onClick={() => {
          setCreated(null)
          setWizardOpen(true)
        }}
        className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500"
      >
        新建分享
      </button>

      {wizardOpen && (
        <ShareWizard
          servers={servers}
          onClose={() => setWizardOpen(false)}
          onCreated={(info) => {
            setCreated(info)
            setWizardOpen(false)
            void refresh().catch((e) => setError(String(e)))
          }}
        />
      )}

      {created && (
        <p className="rounded border border-emerald-800 bg-emerald-950/40 px-3 py-2 text-sm text-emerald-300">
          已生成：<code className="font-mono">{created.url}</code>
          {created.url_singbox && (
            <>
              {' · '}
              <code className="font-mono">{created.url_singbox}</code>
            </>
          )}
        </p>
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
                              void refresh().catch((e) => setError(String(e)))
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
