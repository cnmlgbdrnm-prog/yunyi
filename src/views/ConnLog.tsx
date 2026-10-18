// 连接日志视图：把泄漏检测面板从仪表盘挪到独立的侧边栏一页。
//
// 挪窝不影响「面板不开就漏」那条设计：采样是后端常驻任务，事件照发、窗口照留，
// 本页挂载时先拉一次当前窗口即可（见 ConnLogPanel 的 useEffect）。
// 放在独立页是因为它是一张会长到几百行的表，塞在仪表盘里会把概览挤没。
import ConnLogPanel from '../components/ConnLogPanel'

export default function ConnLog() {
  return (
    <div className="space-y-4">
      <h2 className="text-xl font-semibold">连接日志</h2>
      <ConnLogPanel />
    </div>
  )
}
