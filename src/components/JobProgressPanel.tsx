// 后台任务分步进度面板：步骤/消息 + 进度条；任务已提交但事件未达时显示等待占位。
// 供建链向导与设备面板复用；进度数值先夹取再渲染，防非有限值撑破布局。
import type { JobProgress } from '../api/types'

/** 面板需要的字段子集：注册表里的在跟踪任务（jobs/registry 的 TrackedJob）与
 *  IPC 直出的 JobProgress 都满足它，组件就不必知道数据是从哪条路来的。 */
export type ProgressView = Pick<JobProgress, 'step' | 'message' | 'progress'>

interface Props {
  progress: ProgressView | null
}

export default function JobProgressPanel({ progress }: Props) {
  if (progress == null) {
    return <p className="text-sm text-neutral-300">任务已提交，等待执行…</p>
  }
  return (
    <>
      <p className="text-sm text-neutral-300">步骤：{progress.step}</p>
      {progress.message !== '' && <p className="text-sm text-neutral-400">{progress.message}</p>}
      <div className="h-2 w-full overflow-hidden rounded bg-neutral-800">
        <div className="h-full bg-emerald-600 transition-all" style={{ width: `${Math.min(100, Math.max(0, progress.progress))}%` }} />
      </div>
    </>
  )
}
