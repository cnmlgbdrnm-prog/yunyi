// 通道面板：单台服务器的端口开关管理——扫描监听端口、一键登记、放行/封锁。
// 开关与状态经 SSH iptables（channel_status_cmd/channel_switch_cmd）实时执行，
// 状态不入库（iptables 规则是事实源）；扫描经 scanServerPorts（ss -tlnp 解析）；
// 添加为纯本地 DB 操作。删除默认只删登记；当前已放行时二次确认，可选「封锁并删除」
// ——SSH 管理端口不提供该选项，避免把自己锁在门外。
import { useCallback, useEffect, useState } from 'react'
import {
  addChannel,
  channelStatus,
  channelSwitch,
  listChannels,
  removeChannel,
  scanServerPorts,
} from '../api'
import type { Channel, ChannelStatus, ServerPort } from '../api/types'

const PROTOCOL_OPTIONS = ['socks5', 'http']

export default function ChannelPanel({ serverId, sshPort }: { serverId: number; sshPort: number }) {
  const [channels, setChannels] = useState<Channel[]>([])
  const [statuses, setStatuses] = useState<Record<number, ChannelStatus>>({})
  const [busy, setBusy] = useState<Set<number>>(new Set())
  // 删除确认：通道当前已放行时，让用户选「封锁并删除」还是「仅删除登记」。
  // running=null 表示查不到（服务器不可达）：仍允许清理登记，但不假装端口已关。
  const [pendingDelete, setPendingDelete] = useState<{ channel: Channel; running: boolean | null } | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 添加表单：端口必填，名称选填（留空自动用「端口 N」）
  const [name, setName] = useState('')
  const [port, setPort] = useState('')
  const [protocol, setProtocol] = useState('socks5')
  const [adding, setAdding] = useState(false)
  // 扫描：监听端口结果与扫描中状态
  const [scanned, setScanned] = useState<ServerPort[] | null>(null)
  const [scanning, setScanning] = useState(false)

  const refresh = useCallback(async () => {
    try {
      setChannels(await listChannels(serverId))
    } catch (e) {
      setError(String(e))
    }
  }, [serverId])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const handleAdd = async () => {
    setError(null)
    setAdding(true)
    try {
      const p = Number(port)
      await addChannel({ serverId, name: name.trim() || `端口 ${p}`, port: p, protocol })
      setName('')
      setPort('')
      await refresh()
      setScanned(null)
    } catch (e) {
      setError(String(e))
    } finally {
      setAdding(false)
    }
  }

  const handleScan = async () => {
    setError(null)
    setScanning(true)
    try {
      setScanned(await scanServerPorts(serverId))
    } catch (e) {
      setError(String(e))
    } finally {
      setScanning(false)
    }
  }

  /** 扫描结果一键登记（名称=进程名，无进程名用「端口 N」） */
  const handleRegister = async (p: ServerPort) => {
    setError(null)
    try {
      await addChannel({
        serverId,
        name: p.process ? `${p.process}（${p.port}）` : `端口 ${p.port}`,
        port: p.port,
        protocol: 'socks5',
      })
      await refresh()
      setScanned((prev) => (prev ? prev.filter((x) => x.port !== p.port) : null))
    } catch (e) {
      setError(String(e))
    }
  }

  /**
   * 删除入口：先实时查一次防火墙状态（缓存可能已过期）。
   * - 已封锁：没有需要撤销的规则，直接删登记；
   * - 已放行 / 查不到：进入二次确认，由用户决定是否连端口一起封锁。
   * 查询失败**不阻断**本地清理——服务器离线时仍应能删掉登记。
   */
  const askRemove = async (c: Channel) => {
    setError(null)
    setBusy((prev) => new Set(prev).add(c.id))
    let running: boolean | null = null
    try {
      const s = await channelStatus(c.id)
      setStatuses((prev) => ({ ...prev, [c.id]: s }))
      running = s.running
    } catch {
      running = null
    } finally {
      setBusy((prev) => {
        const next = new Set(prev)
        next.delete(c.id)
        return next
      })
    }
    if (running === false) {
      await confirmRemove(c, false)
      return
    }
    setPendingDelete({ channel: c, running })
  }

  /** 执行删除；block=true 时先撤销 iptables 放行规则，撤不掉就不删登记并上屏原因。 */
  const confirmRemove = async (c: Channel, block: boolean) => {
    setError(null)
    setBusy((prev) => new Set(prev).add(c.id))
    try {
      if (block) {
        const s = await channelSwitch(c.id, false)
        setStatuses((prev) => ({ ...prev, [c.id]: s }))
        if (s.running) {
          setError(`端口 ${c.port} 仍处于放行状态，登记未删除`)
          return
        }
        // 远端命令非 0 时后端仍返回 running=false（detail 以「操作失败」开头）：
        // 可能只是规则本来就不在（-C 短路），也可能是 iptables 真失败。回查一次
        // 实际规则，确认端口确实不再放行才继续删登记。
        if (s.detail.startsWith('操作失败')) {
          const actual = await channelStatus(c.id)
          setStatuses((prev) => ({ ...prev, [c.id]: actual }))
          if (actual.running) {
            setError(`端口 ${c.port} 仍处于放行状态，登记未删除`)
            return
          }
        }
      }
      await removeChannel(c.id)
      setStatuses((prev) => {
        const next = { ...prev }
        delete next[c.id]
        return next
      })
      setPendingDelete(null)
      await refresh()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy((prev) => {
        const next = new Set(prev)
        next.delete(c.id)
        return next
      })
    }
  }

  const handleStatus = async (id: number) => {
    setError(null)
    setBusy((prev) => new Set(prev).add(id))
    try {
      const s = await channelStatus(id)
      setStatuses((prev) => ({ ...prev, [id]: s }))
    } catch (e) {
      setStatuses((prev) => ({ ...prev, [id]: { running: false, detail: String(e) } }))
    } finally {
      setBusy((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const handleSwitch = async (id: number, enabled: boolean) => {
    setError(null)
    setBusy((prev) => new Set(prev).add(id))
    try {
      const s = await channelSwitch(id, enabled)
      setStatuses((prev) => ({ ...prev, [id]: s }))
    } catch (e) {
      setStatuses((prev) => ({ ...prev, [id]: { running: false, detail: String(e) } }))
    } finally {
      setBusy((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  return (
    <div className="mt-2 rounded border border-neutral-800 bg-neutral-900/60 p-3">
      <p className="text-xs font-medium text-neutral-400">端口开关（服务器防火墙放行/封锁）</p>
      {error && (
        <p role="alert" className="mt-1 text-xs text-red-400">
          {error}
        </p>
      )}
      {channels.length === 0 && (
        <p className="mt-1 text-xs text-neutral-500">还没有端口。先「扫描端口」把服务器正在监听的端口拉出来，一键登记后即可放行/封锁。</p>
      )}
      <div className="mt-2 flex items-center gap-2">
        <button
          type="button"
          disabled={scanning}
          onClick={() => void handleScan()}
          className="rounded bg-neutral-800 px-3 py-1 text-xs text-sky-300 hover:bg-neutral-700 disabled:opacity-50"
        >
          {scanning ? '扫描中…' : '扫描端口'}
        </button>
        {scanned !== null && (
          <span className="text-xs text-neutral-500">
            共 {scanned.length} 个监听端口{scanned.length === 0 ? '（无结果）' : ''}
          </span>
        )}
      </div>
      {scanned !== null && scanned.length > 0 && (
        <ul className="mt-2 space-y-1">
          {scanned.map((p) => (
            <li key={p.port} className="flex items-center justify-between gap-2 rounded bg-neutral-900 px-2 py-1.5">
              <p className="truncate text-sm">
                {p.port} <span className="text-xs text-neutral-500">{p.process || '未知进程'}</span>
              </p>
              <button
                type="button"
                onClick={() => void handleRegister(p)}
                className="shrink-0 rounded bg-blue-600 px-2 py-1 text-xs text-white hover:bg-blue-500"
              >
                登记
              </button>
            </li>
          ))}
        </ul>
      )}
      <ul className="mt-2 space-y-1">
        {channels.map((c) => {
          const st = statuses[c.id]
          const isBusy = busy.has(c.id)
          return (
            <li key={c.id} className="flex items-center justify-between gap-2 rounded bg-neutral-900 px-2 py-1.5">
              <div className="min-w-0">
                <p className="truncate text-sm">
                  {c.name} <span className="text-xs text-neutral-500">{c.port} · {c.protocol}</span>
                </p>
                {st && (
                  <p className={`text-xs ${st.running ? 'text-emerald-400' : 'text-neutral-500'}`}>{st.detail}</p>
                )}
              </div>
              {pendingDelete?.channel.id === c.id ? (
                <div className="flex shrink-0 flex-wrap items-center justify-end gap-1">
                  {c.port === sshPort ? (
                    <span className="text-xs text-amber-400">SSH 管理端口不可自动封锁</span>
                  ) : pendingDelete.running === null ? (
                    <span className="text-xs text-amber-400">查不到防火墙状态</span>
                  ) : null}
                  {c.port !== sshPort && (
                    <button
                      type="button"
                      disabled={isBusy}
                      onClick={() => void confirmRemove(c, true)}
                      className="rounded bg-red-700 px-2 py-1 text-xs text-white hover:bg-red-600 disabled:opacity-50"
                    >
                      封锁并删除
                    </button>
                  )}
                  <button
                    type="button"
                    disabled={isBusy}
                    onClick={() => void confirmRemove(c, false)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-300 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    仅删除登记
                  </button>
                  <button
                    type="button"
                    disabled={isBusy}
                    onClick={() => setPendingDelete(null)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-400 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    取消
                  </button>
                </div>
              ) : (
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    type="button"
                    disabled={isBusy}
                    onClick={() => void handleStatus(c.id)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-neutral-300 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    查状态
                  </button>
                  <button
                    type="button"
                    disabled={isBusy}
                    onClick={() => void handleSwitch(c.id, !(st?.running ?? false))}
                    className={`rounded px-2 py-1 text-xs text-white disabled:opacity-50 ${
                      st?.running ? 'bg-red-700 hover:bg-red-600' : 'bg-emerald-700 hover:bg-emerald-600'
                    }`}
                  >
                    {st?.running ? '封锁' : '放行'}
                  </button>
                  <button
                    type="button"
                    disabled={isBusy}
                    onClick={() => void askRemove(c)}
                    className="rounded bg-neutral-800 px-2 py-1 text-xs text-red-300 hover:bg-neutral-700 disabled:opacity-50"
                  >
                    删除
                  </button>
                </div>
              )}
            </li>
          )
        })}
      </ul>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <input
          aria-label="端口号"
          placeholder="端口"
          value={port}
          onChange={(e) => setPort(e.target.value)}
          className="w-24 rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
        />
        <input
          aria-label="端口名称"
          placeholder="名称（可选）"
          value={name}
          onChange={(e) => setName(e.target.value)}
          className="w-32 rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
        />
        <select
          aria-label="端口协议"
          value={protocol}
          onChange={(e) => setProtocol(e.target.value)}
          className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
        >
          {PROTOCOL_OPTIONS.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
        <button
          type="button"
          disabled={adding || port.trim() === ''}
          onClick={() => void handleAdd()}
          className="rounded bg-blue-600 px-3 py-1 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {adding ? '添加中…' : '添加端口'}
        </button>
      </div>
    </div>
  )
}
