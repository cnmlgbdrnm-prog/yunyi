// 服务器列表：每行一张 ServerRow 卡片，展开态内嵌部署向导或编辑表单。
import type { Dispatch, SetStateAction } from 'react'
import type { ConnectionTest, Server } from '../../api/types'
import { isTargetBusy, targetResult, type TrackedJob } from '../../jobs/registry'
import DeployWizard from '../../components/DeployWizard'
import ServerFormPanel, { type ServerFormState } from '../../components/ServerFormPanel'
import ServerRow from '../../components/ServerRow'

export default function ServerList({
  servers,
  speedJobs,
  testing,
  pwBusy,
  pwDone,
  bbrAlgo,
  bbrChecking,
  bbrEnabling,
  bbrMsg,
  deployedProtos,
  tests,
  linkSummary,
  moreOpen,
  channelsOpen,
  upstreamsOpen,
  expand,
  form,
  setForm,
  onToggleMore,
  onToggleChannels,
  onToggleUpstreams,
  onSpeedTest,
  onTest,
  onPasswordless,
  onBbrEnable,
  onBbrCheck,
  onDeploy,
  onEdit,
  onDelete,
  onClose,
  onSave,
  onCancelEdit,
  onDeployDone,
}: {
  servers: Server[]
  speedJobs: ReadonlyMap<number, TrackedJob>
  testing: Set<number>
  pwBusy: Set<number>
  pwDone: Record<number, boolean>
  bbrAlgo: Record<number, string>
  bbrChecking: Set<number>
  bbrEnabling: Set<number>
  bbrMsg: Record<number, { ok: boolean; text: string }>
  deployedProtos: Record<number, string[]>
  tests: Record<number, ConnectionTest>
  linkSummary: (s: Server) => string | null
  moreOpen: Set<number>
  channelsOpen: Set<number>
  upstreamsOpen: Set<number>
  expand: { type: 'deploy' | 'edit'; id: number } | null
  form: ServerFormState | null
  setForm: Dispatch<SetStateAction<ServerFormState | null>>
  onToggleMore: (id: number) => void
  onToggleChannels: (id: number) => void
  onToggleUpstreams: (id: number) => void
  onSpeedTest: (id: number) => void
  onTest: (id: number) => void
  onPasswordless: (id: number) => void
  onBbrEnable: (id: number) => void
  onBbrCheck: (id: number) => void
  onDeploy: (id: number) => void
  onEdit: (s: Server) => void
  onDelete: (id: number) => void
  onClose: () => void
  onSave: () => void
  onCancelEdit: () => void
  onDeployDone: (s: Server) => void
}) {
  return (
    <ul className="space-y-2">
      {servers.map((s) => {
        const live = targetResult(speedJobs, String(s.id))
        return (
          <ServerRow
            key={s.id}
            server={s}
            speed={{
              busy: isTargetBusy(speedJobs, String(s.id)),
              // 行内失败原因优先于库里上次的：任务刚跑完时这才是新的那份
              error: live?.error ?? s.last_check_error,
              done: live != null,
            }}
            testing={testing.has(s.id)}
            passwordlessBusy={pwBusy.has(s.id)}
            passwordless={pwDone[s.id] || s.auth_kind === 'key'}
            passwordlessJustDone={pwDone[s.id] === true}
            bbr={{
              algo: bbrAlgo[s.id],
              checking: bbrChecking.has(s.id),
              enabling: bbrEnabling.has(s.id),
              msg: bbrMsg[s.id],
            }}
            deployedProtos={deployedProtos[s.id]}
            test={tests[s.id]}
            linkSummary={linkSummary(s)}
            open={{
              more: moreOpen.has(s.id),
              channels: channelsOpen.has(s.id),
              upstreams: upstreamsOpen.has(s.id),
            }}
            expanded={expand?.id === s.id}
            onToggleMore={() => onToggleMore(s.id)}
            onToggleChannels={() => onToggleChannels(s.id)}
            onToggleUpstreams={() => onToggleUpstreams(s.id)}
            onSpeedTest={() => onSpeedTest(s.id)}
            onTest={() => onTest(s.id)}
            onPasswordless={() => onPasswordless(s.id)}
            onBbrEnable={() => onBbrEnable(s.id)}
            onBbrCheck={() => onBbrCheck(s.id)}
            onDeploy={() => onDeploy(s.id)}
            onEdit={() => onEdit(s)}
            onDelete={() => onDelete(s.id)}
            onClose={onClose}
          >
            {expand?.id === s.id && expand.type === 'deploy' && (
              <div className="px-4 pb-4">
                <DeployWizard
                  server={s}
                  onClose={onClose}
                  onDone={() => onDeployDone(s)}
                />
              </div>
            )}
            {expand?.id === s.id && expand.type === 'edit' && form != null && (
              <div className="space-y-2 px-4 pb-4">
                <ServerFormPanel
                  form={form}
                  editing
                  onChange={setForm}
                  onSave={onSave}
                  onCancel={onCancelEdit}
                />
              </div>
            )}
          </ServerRow>
        )
      })}
      {servers.length === 0 && (
        <p className="text-sm text-neutral-500">还没有服务器，点击"新建服务器"添加。</p>
      )}
    </ul>
  )
}
