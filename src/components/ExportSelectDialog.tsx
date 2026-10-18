// 导出选择弹窗：拉可导出清单、勾选后交给调用方导出。
// 不可导出的节点（ssr）列出但置灰——静默剔除正是「这条线怎么传不上去」的观感来源。
// 清单拉取失败不降级为「导出全部」：那会让用户以为筛过了。
import { useEffect, useState } from 'react'
import { listExportableNodes } from '../api'
import type { ExportableNode } from '../api/types'

interface Props {
  /** 标题里显示的格式名（YAML / sing-box） */
  formatLabel: string
  onExport: (nodeIds: string[]) => Promise<void>
  onClose: () => void
}

export default function ExportSelectDialog({ formatLabel, onExport, onClose }: Props) {
  const [items, setItems] = useState<ExportableNode[]>([])
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let alive = true
    void (async () => {
      try {
        const list = await listExportableNodes()
        if (!alive) return
        setItems(list)
        // 默认全勾可导出的；不可导出的不勾（按钮也禁用，防「勾了却导不出去」）
        setSelected(new Set(list.filter((e) => e.exportable).map((e) => e.node.id)))
      } catch (e) {
        if (alive) setError(String(e))
      } finally {
        if (alive) setLoading(false)
      }
    })()
    return () => {
      alive = false
    }
  }, [])

  const toggle = (id: string) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const handleConfirm = async () => {
    setBusy(true)
    setError(null)
    try {
      await onExport([...selected])
      onClose()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-2 rounded border border-neutral-700 bg-neutral-900 p-3 text-sm">
      <h4 className="font-medium">选择要导出的节点（{formatLabel}）</h4>
      {loading && <p className="text-neutral-400">加载中…</p>}
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-red-300">
          {error}
        </p>
      )}
      {!loading && !error && items.length === 0 && (
        <p className="text-neutral-400">没有可导出的节点。</p>
      )}
      {items.length > 0 && (
        <ul className="max-h-64 space-y-1 overflow-y-auto">
          {items.map((e) => (
            <li key={e.node.id} className="flex items-center gap-2">
              <input
                type="checkbox"
                aria-label={`选择 ${e.node.name}`}
                checked={selected.has(e.node.id)}
                disabled={!e.exportable}
                onChange={() => toggle(e.node.id)}
              />
              <span className={e.exportable ? '' : 'text-neutral-500'}>{e.node.name}</span>
              <span className="text-xs text-neutral-500">{e.node.protocol}</span>
              {!e.exportable && (
                <span className="text-xs text-amber-300">✗ {e.skip_reason ?? '不可导出'}</span>
              )}
            </li>
          ))}
        </ul>
      )}
      <div className="flex gap-2">
        <button
          type="button"
          onClick={() => void handleConfirm()}
          disabled={busy || selected.size === 0}
          className="rounded bg-blue-600 px-3 py-1.5 text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? '导出中…' : `导出选中 ${selected.size}`}
        </button>
        <button
          type="button"
          onClick={onClose}
          disabled={busy}
          className="rounded bg-neutral-800 px-3 py-1.5 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
        >
          取消
        </button>
      </div>
    </div>
  )
}
