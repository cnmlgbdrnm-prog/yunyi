// iperf3 吞吐对测面板：源/目标/秒数 + 开始/取消对测 + 安装 iperf3 + 最近一次结果。
//
// 单位提醒：这里是 **bits/秒 → Mbps**（iperf3 的 sum_sent.bits_per_second）；
// 服务器下载带宽是 bytes/s，走 utils/format 的 formatBandwidth，两者别混用。
import type { Server, ThroughputTest } from '../api/types'
import { serverKindLabel } from '../utils/serverKindLabel'

/** 对测时长档位（秒），与后端 server_throughput 的参数一致 */
export const SPEED_SECONDS_OPTIONS = [5, 15, 30]

/** bits/秒 → Mbps（两位小数） */
const fmtThroughputMbps = (b: number): string => (b / 1e6).toFixed(2)

interface Props {
  servers: Server[]
  fromId: number | null
  toId: number | null
  seconds: number
  onFromChange: (id: number) => void
  onToChange: (id: number) => void
  onSecondsChange: (n: number) => void
  /** 有任务在跑：三个选择器与开始按钮一律禁用（同一时间只允许一个对测） */
  running: boolean
  /** 运行中的阶段文案（来自注册表，空串表示没有可展示的步骤） */
  progressMessage: string
  onStart: () => void
  onCancel: () => void
  installBusy: boolean
  installMsg: string | null
  onInstall: () => void
  /** 后端按时间倒序，只取首行当「最近一次」 */
  results: ThroughputTest[]
}

export default function ServerThroughputPanel({
  servers,
  fromId,
  toId,
  seconds,
  onFromChange,
  onToChange,
  onSecondsChange,
  running,
  progressMessage,
  onStart,
  onCancel,
  installBusy,
  installMsg,
  onInstall,
  results,
}: Props) {
  const latest = results[0]
  const toName = servers.find((s) => s.id === toId)?.name
  return (
    <>
      <div className="flex flex-wrap items-center gap-3 text-sm">
        <label className="flex items-center gap-1">
          源
          <select aria-label="源服务器" value={fromId ?? ''} onChange={(e) => onFromChange(Number(e.target.value))} disabled={running} className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1.5">
            {servers.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}（{serverKindLabel(s.kind)}）
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1">
          目标
          <select aria-label="目标服务器" value={toId ?? ''} onChange={(e) => onToChange(Number(e.target.value))} disabled={running} className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1.5">
            {servers.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}（{serverKindLabel(s.kind)}）
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1">
          秒数
          <select aria-label="对测秒数" value={seconds} onChange={(e) => onSecondsChange(Number(e.target.value))} disabled={running} className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1.5">
            {SPEED_SECONDS_OPTIONS.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>
        <button
          type="button"
          onClick={onStart}
          disabled={running || fromId == null || toId == null || fromId === toId}
          className="rounded bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {running ? '对测中…' : '开始对测'}
        </button>
        {running && (
          <button
            type="button"
            onClick={onCancel}
            className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-700"
          >
            取消对测
          </button>
        )}
        <button
          type="button"
          onClick={onInstall}
          disabled={installBusy || toId == null}
          className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-amber-300 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {installBusy ? '安装中…' : `安装 iperf3（${toName ?? '未选目标'}）`}
        </button>
      </div>
      {running && progressMessage !== '' && (
        <p className="text-xs text-neutral-400">{progressMessage}</p>
      )}
      {installMsg && <p className="text-xs text-emerald-400">{installMsg}</p>}
      {latest &&
        (latest.bits_per_second != null ? (
          <p className="text-xs text-neutral-400">
            最近对测：{fmtThroughputMbps(latest.bits_per_second)} Mbps
          </p>
        ) : (
          <p className="text-xs text-red-400">
            最近对测失败：{latest.error ?? '未知错误'}
          </p>
        ))}
      {results.length === 0 && <p className="text-xs text-neutral-500">还没有对测记录。</p>}
    </>
  )
}
