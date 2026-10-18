// 服务器保存编排：新建直接落库；编辑时若出口族变了，后端会顺手排一次重部署，
// 这里负责把那个任务接进注册表并在行内显示进度。
//
// 为什么单独成 hook：Servers.tsx 已贴近 500 行上限（组件行数闸门），而这段逻辑
// 还要先占位再发 IPC，塞回视图里既超限又难测。视图只负责表单状态与收尾。
import { useCallback } from 'react'
import { createServer, updateServer } from '../../api'
import type { Server } from '../../api/types'
import type { ServerFormState } from '../../components/ServerFormPanel'
import { bindJob, release, reserveJob } from '../../jobs/registry'

export function useServerSave(
  servers: Server[],
  editingId: number | null,
  form: ServerFormState | null,
  refresh: () => Promise<void>,
  setError: (message: string | null) => void,
  onDone: () => void,
) {
  return useCallback(async () => {
    if (!form) return
    setError(null)
    // 占位必须先于 IPC 发出：后端任务进度可能比 IPC 返回还早，__ingest 对
    // 不认识的 job id 一律丢弃（见 registry.reserveJob）。
    let ticket: number | null = null
    try {
      if (editingId != null) {
        const previous = servers.find((s) => s.id === editingId)?.egress_family
        // 旧值缺失按「变了」处理：宁可多占一个位，也不能让已入队的任务失去跟踪
        if (previous !== form.egressFamily) ticket = reserveJob('deploy', [String(editingId)])
        const result = await updateServer({
          id: editingId,
          name: form.name,
          host: form.host,
          hostV6: form.hostV6,
          port: form.port,
          user: form.user,
          authKind: form.authKind,
          kind: form.kind,
          notes: form.notes,
          credential: form.credential === '' ? null : form.credential,
          egressFamily: form.egressFamily,
        })
        if (result.deploy_job_id != null) {
          // 前端以为没变、后端仍排了任务（列表读旧了）：补个占位再绑定。
          // 这段窗口里的进度事件会丢，但 bindJob 自带一次终态对账兜底。
          if (ticket == null) ticket = reserveJob('deploy', [String(editingId)])
          bindJob(ticket, result.deploy_job_id)
        } else if (ticket != null) {
          release(ticket)
        }
        ticket = null // 已交给注册表或已撤，避免下面的 catch 重复释放
        // 存库本身已经成功：重部署只是后续动作，排不上就报原因，不回滚、不阻断关表单
        if (result.deploy_error != null) setError(result.deploy_error)
        onDone()
        await refresh()
        return
      }
      // 新建：显式组装字段，不把 egressFamily 透给未声明该参数的 command
      await createServer({
        name: form.name,
        host: form.host,
        hostV6: form.hostV6,
        port: form.port,
        user: form.user,
        authKind: form.authKind,
        kind: form.kind,
        notes: form.notes,
        credential: form.credential,
      })
      onDone()
      await refresh()
    } catch (e) {
      if (ticket != null) release(ticket)
      setError(String(e))
    }
  }, [servers, editingId, form, refresh, setError, onDone])
}
