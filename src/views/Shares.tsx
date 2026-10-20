// 分享视图：列出服务器上的分享链接、新建（选服务器 + 勾节点）、作废。
// 分享的真实状态在服务器上——本地不存记录，所以每次进页面都重新拉，
// 某台连不上时只列不出那几条，并把服务器名摆在明面上（见 unreachable 提示）。
import { useCallback, useEffect, useState } from 'react'
import { createShare, deleteShare, listExportableNodes, listServers, listShares } from '../api'
import ConfirmButton from '../components/ConfirmButton'
import { copyText } from '../utils/clipboard'
import type { ExportableNode, Server, ShareInfo } from '../api/types'

const GB = 1024 ** 3

/** 用量展示：GB 保留一位小数；旧版分享没有配额，显示「—」。 */
function fmtBytes(n: number): string {
  if (n >= GB) return `${(n / GB).toFixed(1)} GB`
  if (n >= 1024 ** 2) return `${(n / 1024 ** 2).toFixed(1)} MB`
  if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${n} B`
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

export default function Shares() {
  const [shares, setShares] = useState<ShareInfo[]>([])
  const [unreachable, setUnreachable] = useState<string[]>([])
  const [servers, setServers] = useState<Server[]>([])
  const [error, setError] = useState<string | null>(null)

  // 新建向导
  const [wizardOpen, setWizardOpen] = useState(false)
  const [serverId, setServerId] = useState<number | null>(null)
  const [candidates, setCandidates] = useState<ExportableNode[]>([])
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [busy, setBusy] = useState(false)
  const [created, setCreated] = useState<ShareInfo | null>(null)

  const refresh = useCallback(async () => {
    const list = await listShares()
    setShares(list.shares)
    setUnreachable(list.unreachable)
  }, [])

  // 首次取数期间**不能**渲染「还没有分享」：分享在服务器上，这台要 SSH 逐台读，
  // 实测一次 ~14 秒。那 14 秒里界面说「没有分享」就是在撒谎——用户切一下视图
  // 回来看到它，会以为分享被删了（2026-09-21 实际发生）。
  const [loading, setLoading] = useState(true)

  useEffect(() => {
    void refresh()
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false))
    void listServers()
      .then((s) => {
        setServers(s)
        setServerId(s[0]?.id ?? null)
      })
      .catch((e) => setError(String(e)))
  }, [refresh])

  // 向导打开时才拉候选清单（与导出同一口径：当前能进配置的那些）
  useEffect(() => {
    if (!wizardOpen) return
    let alive = true
    void listExportableNodes()
      .then((list) => {
        if (!alive) return
        setCandidates(list)
        setPicked(new Set(list.filter((e) => e.exportable).map((e) => e.node.id)))
      })
      .catch((e) => alive && setError(String(e)))
    return () => {
      alive = false
    }
  }, [wizardOpen])

  const toggle = (id: string) =>
    setPicked((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })

  const handleCreate = async () => {
    if (serverId == null || picked.size === 0) return
    setBusy(true)
    setError(null)
    try {
      // T12 的向导会带上用户填写的配额/速率/天数；这里先落默认值，
      // 保证 API 签名切换后页面仍可编译与工作。
      setCreated(await createShare(serverId, [...picked], 100, 100, 100, 30))
      setWizardOpen(false)
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

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
      <h2 className="text-xl font-semibold">分享</h2>
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
        <div className="space-y-2 rounded border border-neutral-700 bg-neutral-900 p-3 text-sm">
          <label className="flex items-center gap-2">
            服务器
            <select
              aria-label="分享服务器"
              value={serverId ?? ''}
              onChange={(e) => setServerId(Number(e.target.value))}
              className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
            >
              {servers.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name}（{s.host}）
                </option>
              ))}
            </select>
          </label>
          <ul className="max-h-64 space-y-1 overflow-y-auto">
            {candidates.map((e) => (
              <li key={e.node.id} className="flex items-center gap-2">
                <input
                  type="checkbox"
                  aria-label={`选择 ${e.node.name}`}
                  checked={picked.has(e.node.id)}
                  disabled={!e.exportable}
                  onChange={() => toggle(e.node.id)}
                />
                <span>{e.node.name}</span>
                <span className="text-xs text-neutral-500">{e.node.protocol}</span>
                {e.skip_reason && <span className="text-xs text-amber-500">{e.skip_reason}</span>}
              </li>
            ))}
          </ul>
          <div className="flex gap-2">
            <button
              type="button"
              onClick={() => void handleCreate()}
              disabled={busy || picked.size === 0 || serverId == null}
              className="rounded bg-blue-600 px-3 py-1.5 text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy ? '生成中…' : `生成链接（${picked.size} 条）`}
            </button>
            <button
              type="button"
              onClick={() => setWizardOpen(false)}
              disabled={busy}
              className="rounded bg-neutral-800 px-3 py-1.5 hover:bg-neutral-700 disabled:opacity-50"
            >
              取消
            </button>
          </div>
        </div>
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
                <tr key={`${s.server_id}-${s.id}`} className="border-b border-neutral-900">
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
              )
            })}
          </tbody>
        </table>
        {shares.length === 0 && (
          <p className="p-4 text-sm text-neutral-500">
            {loading ? '读取中…' : '还没有分享。'}
          </p>
        )}
      </div>
    </div>
  )
}
