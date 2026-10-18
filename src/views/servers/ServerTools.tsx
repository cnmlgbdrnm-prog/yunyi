// 服务器页的「工具」折叠区：批量测速与 iperf3 吞吐对测。
import type { Server, ThroughputTest } from '../../api/types'
import ServerSpeedPanel from '../../components/ServerSpeedPanel'
import ServerThroughputPanel from '../../components/ServerThroughputPanel'

export default function ServerTools({
  open,
  onToggle,
  concurrency,
  onConcurrencyChange,
  serverCount,
  runningCount,
  onBatch,
  onCancelSpeed,
  servers,
  fromId,
  toId,
  seconds,
  onFromChange,
  onToChange,
  onSecondsChange,
  throughputRunning,
  progressMessage,
  onThroughputStart,
  onThroughputCancel,
  installBusy,
  installMsg,
  onInstall,
  results,
}: {
  open: boolean
  onToggle: () => void
  concurrency: number
  onConcurrencyChange: (n: number) => void
  serverCount: number
  runningCount: number
  onBatch: () => void
  onCancelSpeed: () => void
  servers: Server[]
  fromId: number | null
  toId: number | null
  seconds: number
  onFromChange: (id: number | null) => void
  onToChange: (id: number | null) => void
  onSecondsChange: (n: number) => void
  throughputRunning: boolean
  progressMessage: string
  onThroughputStart: () => void
  onThroughputCancel: () => void
  installBusy: boolean
  installMsg: string | null
  onInstall: () => void
  results: ThroughputTest[]
}) {
  return (
    <section className="rounded-lg border border-neutral-800 bg-neutral-900/50">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className="flex w-full items-center justify-between px-4 py-3 text-left text-sm font-medium text-neutral-200 hover:bg-neutral-800/50"
      >
        <span>工具</span>
        <span className="text-xs text-neutral-500">{open ? '收起' : '展开'}</span>
      </button>
      {open && (
        <div className="space-y-3 px-4 pb-4">
          <ServerSpeedPanel
            concurrency={concurrency}
            onConcurrencyChange={onConcurrencyChange}
            serverCount={serverCount}
            runningCount={runningCount}
            onBatch={onBatch}
            onCancel={onCancelSpeed}
          />
          <ServerThroughputPanel
            servers={servers}
            fromId={fromId}
            toId={toId}
            seconds={seconds}
            onFromChange={onFromChange}
            onToChange={onToChange}
            onSecondsChange={onSecondsChange}
            running={throughputRunning}
            progressMessage={progressMessage}
            onStart={onThroughputStart}
            onCancel={onThroughputCancel}
            installBusy={installBusy}
            installMsg={installMsg}
            onInstall={onInstall}
            results={results}
          />
        </div>
      )}
    </section>
  )
}
