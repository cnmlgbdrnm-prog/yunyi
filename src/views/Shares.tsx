// 分享视图：列出服务器上的分享链接、新建（选服务器 + 勾节点）、作废。
// 分享的真实状态在服务器上——本地不存记录，所以每次进页面都重新拉，
// 某台连不上时只列不出那几条，并把服务器名摆在明面上（见 unreachable 提示）。
import { useCallback, useEffect, useState } from 'react'
import { createShare, deleteShare, listExportableNodes, listServers, listShares } from '../api'
import ConfirmButton from '../components/ConfirmButton'
import { copyText } from '../utils/clipboard'
import type { ExportableNode, Server, ShareInfo } from '../api/types'

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
      setCreated(await createShare(serverId, [...picked]))
      setWizardOpen(false)
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  // 复制失败必须说出来：剪贴板 API 的拒绝是静默的，不说用户就只会看到「点了没反应」
  const handleCopy = async (s: ShareInfo) => {
    setError(null)
    if (await copyText(s.url)) return
    setError('复制失败：剪贴板不可用。请手动选中链接复制')
  }

  const handleDelete = async (s: ShareInfo) => {
    setError(null)
    try {
      await deleteShare(s.server_id, s.token)
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
        <p>链接即凭证：服务为明文 HTTP，被截获即可使用这些节点。</p>
        <p>快照：内容固定在生成那一刻，改节点或重新部署后需重新生成。</p>
        <p>仅 YAML（mihomo / Clash）；sing-box 暂不支持。</p>
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
        </p>
      )}

      <div className="overflow-x-auto rounded border border-neutral-800">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-neutral-800 text-left text-neutral-400">
              <th className="px-3 py-2">链接</th>
              <th className="px-3 py-2">节点</th>
              <th className="px-3 py-2">服务器</th>
              <th className="px-3 py-2">状态</th>
              <th className="px-3 py-2">操作</th>
            </tr>
          </thead>
          <tbody>
            {shares.map((s) => (
              <tr key={`${s.server_id}-${s.token}`} className="border-b border-neutral-900">
                <td className="px-3 py-2 font-mono text-xs">{s.url}</td>
                <td className="px-3 py-2 text-neutral-400">{s.node_names.join('、')}</td>
                <td className="px-3 py-2 text-neutral-400">{s.server_name}</td>
                <td className="px-3 py-2">
                  {s.active ? (
                    <span className="text-emerald-400">运行中</span>
                  ) : (
                    <span className="text-neutral-500">未运行</span>
                  )}
                </td>
                <td className="px-3 py-2">
                  <div className="flex items-center gap-2">
                    <button
                      type="button"
                      onClick={() => void handleCopy(s)}
                      className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700"
                    >
                      复制链接
                    </button>
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
            ))}
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
