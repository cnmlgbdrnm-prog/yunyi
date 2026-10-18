// 批量测速面板：并发数选择 + 批量测速 + 取消（有在跟踪任务时才出现）。
//
// 「取消测速」按**任务表非空**出现，而不是按本地 busy 标志：任务态在模块级注册表里
// （jobs/registry），切页回来仍在跟踪，按钮也就该还在（评审 #5/#6）。

/** 并发数档位：与后端 server_speed 的参数一致，改这里要同步看那边 */
export const CONCURRENCY_OPTIONS = [1, 3, 5]

interface Props {
  concurrency: number
  onConcurrencyChange: (n: number) => void
  /** 无服务器时批量测速无意义，按钮置灰（而不是点了再报错） */
  serverCount: number
  /** 当前在跟踪的测速任务数：> 0 即给出取消入口 */
  runningCount: number
  onBatch: () => void
  onCancel: () => void
}

export default function ServerSpeedPanel({
  concurrency,
  onConcurrencyChange,
  serverCount,
  runningCount,
  onBatch,
  onCancel,
}: Props) {
  return (
    <div className="flex flex-wrap items-center gap-3">
      <select
        aria-label="并发数"
        value={concurrency}
        onChange={(e) => onConcurrencyChange(Number(e.target.value))}
        className="rounded border border-neutral-700 bg-neutral-900 px-2 py-1.5 text-sm"
      >
        {CONCURRENCY_OPTIONS.map((c) => (
          <option key={c} value={c}>
            {c}
          </option>
        ))}
      </select>
      <button
        type="button"
        onClick={onBatch}
        disabled={serverCount === 0}
        className="rounded bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"
      >
        批量测速
      </button>
      {runningCount > 0 && (
        <button
          type="button"
          onClick={onCancel}
          className="rounded bg-neutral-800 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-700"
        >
          取消测速
        </button>
      )}
    </div>
  )
}
