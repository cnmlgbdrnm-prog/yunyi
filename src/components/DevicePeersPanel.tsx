// 设备面板（模式 B 链路）：设备列表 + 添加 + 查看配置 + 删除（二次确认）。
// 添加/删除经 add_device_peer/remove_device_peer 提交为 peer 类后台任务，任务态取自
// 模块级注册表（jobs/registry）：分步展示、成功后刷新列表与行内提示、失败展示原始错误
// （后端保证带主机/端口/原因）。加/删是两个前端 kind（后端只有一个 peer），成功后那句
// 提示就由 kind 决定——不必再记「这次点的是哪个」，切页回来也还认得。
import { useCallback, useEffect, useState } from 'react'
import { addDevicePeer, listDevicePeers, removeDevicePeer } from '../api'
import type { DevicePeer } from '../api/types'
import { bindJob, jobForTarget, progressView, release, reserveJob, useJobTerminal, useTrackedJobs } from '../jobs/registry'
import ConfirmButton from './ConfirmButton'
import JobProgressPanel from './JobProgressPanel'
import PeerConfigDialog from './PeerConfigDialog'

interface Props {
  ruleId: number
}

export default function DevicePeersPanel({ ruleId }: Props) {
  const [peers, setPeers] = useState<DevicePeer[]>([])
  const [name, setName] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [success, setSuccess] = useState<string | null>(null)
  const [configPeer, setConfigPeer] = useState<DevicePeer | null>(null)

  // 本面板同一时间只有一个任务在跑（按钮在忙时禁用）。
  // targets[0] = 本面板的 rule id；删除任务的 targets[1] = 被删的那台设备，
  // 行内「删除中…」据此认行——它是任务状态，切页重挂后仍要认得。
  const addJob = jobForTarget(useTrackedJobs('peer'), String(ruleId))
  const removeJob = jobForTarget(useTrackedJobs('peer_remove'), String(ruleId))
  const activeJob = addJob ?? removeJob
  const removingPeer = removeJob?.targets[1]

  const refresh = useCallback(async () => {
    try {
      setPeers(await listDevicePeers(ruleId))
    } catch (e) {
      setError(String(e))
    }
  }, [ruleId])

  useEffect(() => {
    void refresh()
  }, [refresh])

  useJobTerminal('peer', (job) => {
    if (job.error) {
      setError(job.error)
      return
    }
    setSuccess('配置已生成，请打开配置导入')
    void refresh()
  })
  useJobTerminal('peer_remove', (job) => {
    if (job.error) {
      setError(job.error)
      return
    }
    setSuccess('设备已删除')
    void refresh()
  })

  const handleAdd = async () => {
    if (name.trim() === '' || activeJob != null) return
    setError(null)
    setSuccess(null)
    const ticket = reserveJob('peer', [String(ruleId)])
    try {
      bindJob(ticket, await addDevicePeer(ruleId, name.trim()))
      setName('')
    } catch (e) {
      release(ticket)
      setError(String(e))
    }
  }

  const handleRemove = async (peer: DevicePeer) => {
    if (activeJob != null) return
    setError(null)
    setSuccess(null)
    const ticket = reserveJob('peer_remove', [String(ruleId), String(peer.id)])
    try {
      bindJob(ticket, await removeDevicePeer(peer.id))
    } catch (e) {
      release(ticket)
      setError(String(e))
    }
  }

  const busy = activeJob != null

  return (
    <div className="space-y-2 rounded border border-neutral-800 bg-neutral-950/50 p-3">
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}
      {success && <p className="text-sm text-emerald-400">{success}</p>}
      <div className="flex gap-2">
        <input
          value={name}
          placeholder="设备名称"
          disabled={busy}
          onChange={(e) => setName(e.target.value)}
          className="w-48 rounded border border-neutral-700 bg-neutral-900 px-2 py-1 text-sm"
        />
        <button
          type="button"
          onClick={() => void handleAdd()}
          disabled={busy || name.trim() === ''}
          className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50"
        >
          {busy && addJob != null ? '添加中…' : '添加设备'}
        </button>
      </div>
      {busy && <JobProgressPanel progress={progressView(activeJob)} />}
      <ul className="space-y-1">
        {peers.map((p) => (
          <li key={p.id} className="flex items-center justify-between rounded border border-neutral-800 px-3 py-2 text-sm">
            <span>
              <span className="font-medium">{p.name}</span>
              <span className="ml-2 text-neutral-400">{p.wg_ip}</span>
              <span className="ml-2 text-xs text-neutral-500">{p.created_at}</span>
            </span>
            <span className="flex items-center gap-2">
              <button type="button" onClick={() => setConfigPeer(p)} className="rounded bg-neutral-800 px-2 py-1 text-xs hover:bg-neutral-700">
                查看配置
              </button>
              <ConfirmButton
                label="删除"
                confirmLabel={`确认删除 ${p.name}`}
                busyLabel="删除中…"
                busy={removingPeer === String(p.id)}
                disabled={busy}
                onConfirm={() => void handleRemove(p)}
              />
            </span>
          </li>
        ))}
        {peers.length === 0 && !busy && <li className="text-sm text-neutral-500">还没有设备，输入名称添加。</li>}
      </ul>
      {configPeer && <PeerConfigDialog peer={configPeer} onClose={() => setConfigPeer(null)} />}
    </div>
  )
}
