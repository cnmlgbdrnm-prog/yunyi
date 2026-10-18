// 订阅管理视图：订阅列表、新建/编辑表单、同步按钮与状态展示。
// 数据经 src/api 类型化封装调用 Rust 命令层。
import { useCallback, useEffect, useState } from 'react'
import {
  createSubscription,
  deleteSubscription,
  listSubscriptions,
  syncSubscription,
} from '../api'
import type { Subscription, SyncOutcome } from '../api/types'

interface FormState {
  name: string
  url: string
  filterJson: string
}

const EMPTY_FORM: FormState = { name: '', url: '', filterJson: '{"include":[],"exclude":[]}' }

export default function Subscriptions() {
  const [subs, setSubs] = useState<Subscription[]>([])
  const [form, setForm] = useState<FormState | null>(null)
  // 多个订阅可同时处于同步中，用 Set 按订阅分别标记，互不干扰
  const [syncing, setSyncing] = useState<Set<string>>(new Set())
  const [syncResult, setSyncResult] = useState<Record<string, SyncOutcome>>({})
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      setSubs(await listSubscriptions())
    } catch (e) {
      setError(String(e))
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const handleSync = async (id: string) => {
    setSyncing((prev) => new Set(prev).add(id))
    setError(null)
    try {
      const outcome = await syncSubscription(id)
      setSyncResult((prev) => ({ ...prev, [id]: outcome }))
    } catch (e) {
      setError(String(e))
    } finally {
      setSyncing((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
      await refresh()
    }
  }

  const handleSave = async () => {
    if (!form) return
    setError(null)
    try {
      await createSubscription({ ...form, enabled: true })
      setForm(null)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  const handleDelete = async (id: string) => {
    setError(null)
    try {
      await deleteSubscription(id)
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold">订阅管理</h2>
        <button
          type="button"
          onClick={() => setForm(EMPTY_FORM)}
          className="rounded bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500"
        >
          新建订阅
        </button>
      </div>

      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      {form && (
        <div className="space-y-2 rounded border border-neutral-800 p-4">
          <label className="block text-sm">
            名称
            <input
              className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
            />
          </label>
          <label className="block text-sm">
            订阅 URL
            <input
              className="mt-1 w-full rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
              value={form.url}
              onChange={(e) => setForm({ ...form, url: e.target.value })}
            />
          </label>
          <div className="flex gap-2">
            <button
              type="button"
              onClick={handleSave}
              className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500"
            >
              保存
            </button>
            <button
              type="button"
              onClick={() => setForm(null)}
              className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700"
            >
              取消
            </button>
          </div>
        </div>
      )}

      <ul className="space-y-2">
        {subs.map((s) => (
          <li
            key={s.id}
            className="flex items-center justify-between rounded border border-neutral-800 p-3"
          >
            <div>
              <p className="font-medium">{s.name}</p>
              <p className="text-xs text-neutral-500">{s.url}</p>
              {s.last_sync_at && (
                <p className="text-xs text-neutral-400">上次同步：{s.last_sync_at}</p>
              )}
              {s.last_sync_error && (
                <p className="text-xs text-red-400">同步失败：{s.last_sync_error}</p>
              )}
              {syncResult[s.id] && (
                <p className="text-xs text-emerald-400">
                  新增 {syncResult[s.id].upserted}，清理 {syncResult[s.id].removed}
                </p>
              )}
            </div>
            <div className="flex gap-2">
              <button
                type="button"
                disabled={syncing.has(s.id)}
                onClick={() => handleSync(s.id)}
                className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50"
              >
                {syncing.has(s.id) ? '同步中…' : '同步'}
              </button>
              <button
                type="button"
                onClick={() => handleDelete(s.id)}
                className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-red-300 hover:bg-neutral-700"
              >
                删除
              </button>
            </div>
          </li>
        ))}
        {subs.length === 0 && (
          <p className="text-sm text-neutral-500">还没有订阅，点击"新建订阅"添加。</p>
        )}
      </ul>
    </div>
  )
}
