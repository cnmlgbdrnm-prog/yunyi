// 建链向导：选择中转/落地机与模式 → probeLink 探测 → 三态分流
// （已握手→接管表单（预填探测回读值）；单端有 WG→提示重建；未配置→建链）→
// buildLink/adoptLink。建链/重建任务的状态取自模块级注册表（jobs/registry，
// 目标即本对的「中转机→落地机」），分步展示，支持取消与失败重试；
// adopt 同步成功即回调 onDone。探测结果面板在 LinkProbePanel，任务进度在 JobProgressPanel。
import { useCallback, useRef, useState } from 'react'
import { adoptLink, buildLink, cancelJob, probeLink } from '../api'
import type { ProbeResult, Server } from '../api/types'
import { bindJob, progressView, release, reserveJob, useJobTerminal, useTrackedJobs } from '../jobs/registry'
import { dnatTargetForPort, pickDnatTarget } from '../utils/dnatTarget'
import JobProgressPanel from './JobProgressPanel'
import LinkProbePanel from './LinkProbePanel'
import LinkWizardForm from './LinkWizardForm'

interface Props {
  servers: Server[]
  onClose: () => void
  onDone: () => void
  /** 预填：节点页「补全中转」入口把向导直接开到要补的那对机器上，省掉用户再选一遍 */
  initialRelayId?: number | null
  initialLandingId?: number | null
  initialForwardMode?: 'single' | 'same_port'
}

type Mode = 'forward' | 'peer'

export default function LinkWizard({
  servers,
  onClose,
  onDone,
  initialRelayId = null,
  initialLandingId = null,
  initialForwardMode = 'single',
}: Props) {
  const relayServers = servers.filter((s) => s.kind === 'relay')
  const landingServers = servers.filter((s) => s.kind === 'landing')
  const [relayId, setRelayId] = useState<number | null>(initialRelayId)
  const [landingId, setLandingId] = useState<number | null>(initialLandingId)
  const [mode, setMode] = useState<Mode>('forward')
  const [forwardMode, setForwardMode] = useState<'single' | 'same_port'>(initialForwardMode)
  const [port, setPort] = useState(443)
  const [landingPort, setLandingPort] = useState(443)
  const [converged, setConverged] = useState(true)
  // 接管表单回读值（探测预填，可编辑）
  const [wgPort, setWgPort] = useState(51820)
  const [subnet, setSubnet] = useState('')
  const [forwardPort, setForwardPort] = useState(443)
  const [probing, setProbing] = useState(false)
  const [probe, setProbe] = useState<ProbeResult | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [adopted, setAdopted] = useState(false)
  // 建链完成标记：任务已从注册表移除，向导要停在「完成」态而不是又摆出「建链」按钮
  const [finished, setFinished] = useState(false)
  // 本向导自己的建链任务：目标是选中的那对机器（中转在前）。
  // 用 link_build 这个前端 kind 与「拆链」任务（同为后端 link）分开——两者目标
  // 都是数字 id（拆链的是 rule id），混在一个桶里会认错任务，把拆链进度画进向导。
  const buildJobs = useTrackedJobs('link_build')
  const buildJob = [...buildJobs.values()].find(
    (j) => j.targets[0] === String(relayId) && j.targets[1] === String(landingId),
  )
  const [error, setError] = useState<string | null>(null)
  // 最近一次提交入口：失败重试按同入口重放（探测失败→重探测，任务失败→重建）
  const lastActionRef = useRef<'probe' | 'build' | 'adopt'>('probe')

  // 成功终态回调 onDone（失败只上屏原因，交由「重试」）。回调经 ref 引用：
  // 视图每次渲染的新闭包不会重建订阅。事件丢失由注册表那个唯一的 5s 对账兜底
  // ——本向导原先**没有**兜底，丢一个终态事件就永久卡在「建链中…」（评审 #15）。
  const onDoneRef = useRef(onDone)
  onDoneRef.current = onDone
  useJobTerminal('link_build', (job) => {
    if (job.error) {
      setError(job.error)
      return
    }
    setFinished(true)
    onDoneRef.current()
  })

  const handleProbe = async () => {
    if (relayId == null || landingId == null) return
    setError(null)
    setProbe(null)
    setProbing(true)
    lastActionRef.current = 'probe'
    try {
      const r = await probeLink(relayId, landingId)
      setProbe(r)
      if (r.handshake) {
        // 端口一律取回读的 DNAT 表：转发端口是入口端口，落地端口是目标端口。
        // 落地端口曾经留默认 443 要用户手填——填错的表现是接管成功但挂中转
        // 永远匹配不上节点（匹配按落地端口对端口做），而这半截一直在回读里。
        const pair = pickDnatTarget(r.dnat_pairs, r.landing_wg_ip, r.forward_port)
        setWgPort(r.wg_port ?? 51820)
        setSubnet(r.subnet ?? '')
        setForwardPort(pair?.forward_port ?? r.forward_port ?? 443)
        if (pair) setLandingPort(pair.target_port)
      }
    } catch (e) {
      setError(String(e))
    } finally {
      setProbing(false)
    }
  }

  const linkName = useCallback((): string | null => {
    if (relayId == null || landingId == null) return null
    const relay = relayServers.find((s) => s.id === relayId)
    const landing = landingServers.find((s) => s.id === landingId)
    if (!relay || !landing) return null
    return `${relay.name}→${landing.name}`
  }, [relayId, landingId, relayServers, landingServers])

  const handleBuild = async (action: 'build' | 'rebuild') => {
    const name = linkName()
    if (name == null || relayId == null || landingId == null) return
    setError(null)
    setFinished(false)
    lastActionRef.current = 'build'
    setSubmitting(true)
    // 占位先行：IPC 回来之前进度区就已出现；「提交→登记」窗口里的终态由注册表补查
    const ticket = reserveJob('link_build', [String(relayId), String(landingId)])
    try {
      bindJob(
        ticket,
        await buildLink({
          name,
          relayServerId: relayId,
          landingServerId: landingId,
          clientMode: mode,
          wgPort: mode === 'peer' ? port : wgPort,
          forwardPort: mode === 'forward' ? port : null,
          landingPort,
          converged,
          // 模式 B 不做端口转发，整机转发在它身上没有意义
          forwardMode: mode === 'forward' ? forwardMode : 'single',
          action,
        }),
      )
    } catch (e) {
      release(ticket)
      setError(String(e))
    } finally {
      setSubmitting(false)
    }
  }

  const handleAdopt = async () => {
    const name = linkName()
    if (name == null || relayId == null || landingId == null) return
    setError(null)
    lastActionRef.current = 'adopt'
    setSubmitting(true)
    try {
      await adoptLink({
        name,
        relayServerId: relayId,
        landingServerId: landingId,
        clientMode: mode,
        wgPort,
        forwardPort: mode === 'forward' ? forwardPort : null,
        relayPubkey: probe?.relay_pubkey ?? null,
        landingPubkey: probe?.landing_pubkey ?? null,
        landingPort,
        subnet,
        converged,
      })
      setAdopted(true)
      onDoneRef.current()
    } catch (e) {
      setError(String(e))
    } finally {
      setSubmitting(false)
    }
  }

  // 失败重试按最近一次提交入口重放：探测失败→重探测；接管失败→重接管；
  // 任务失败→以 build 重建（failed 状态规则允许 build 装载，rebuild 会被拒）
  const handleRetry = () => {
    if (lastActionRef.current === 'probe') void handleProbe()
    else if (lastActionRef.current === 'adopt') void handleAdopt()
    else void handleBuild('build')
  }

  const handleCancelJob = async () => {
    const job = buildJob
    if (job != null) {
      if (job.id >= 0) {
        try {
          await cancelJob(job.id)
        } catch (e) {
          setError(String(e))
          return
        }
      }
      // 本地先撤（后端终态事件到达时任务已不在表里，自然忽略）；占位（负数 id）后端不认
      release(job.id)
    }
    // 复位完成标记，回到初始表单（可调整参数重试）
    setFinished(false)
  }

  const failed = error != null
  // 任务还在跟踪表里 = 还在跑：终态即被注册表移除，不必再自己判 progress>=100
  const running = buildJob != null && !failed && !finished

  return (
    <div className="space-y-3 rounded border border-emerald-900 bg-neutral-900 p-4">
      <h3 className="text-lg font-semibold">新建中转链路</h3>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      {buildJob == null && !adopted && (
        <>
          <LinkWizardForm
            relayServers={relayServers}
            landingServers={landingServers}
            relayId={relayId}
            landingId={landingId}
            mode={mode}
            port={port}
            landingPort={landingPort}
            converged={converged}
            forwardMode={forwardMode}
            disabled={running}
            onRelayId={setRelayId}
            onLandingId={setLandingId}
            onMode={setMode}
            onPort={setPort}
            onLandingPort={setLandingPort}
            onConverged={setConverged}
            onForwardMode={setForwardMode}
          />

          {probe && (
            <LinkProbePanel
              probe={probe}
              mode={mode}
              wgPort={wgPort}
              subnet={subnet}
              forwardPort={forwardPort}
              disabled={running}
              onWgPort={setWgPort}
              onSubnet={setSubnet}
              onForwardPort={(v) => {
                setForwardPort(v)
                // 换服务（如 4443→8443）时落地端口跟着走：目标端口就在同一条 DNAT 行里。
                // 只在表里确有这个入口端口时才动，否则手打中途会把落地端口拽走
                const pair = dnatTargetForPort(probe.dnat_pairs, probe.landing_wg_ip, v)
                if (pair) setLandingPort(pair.target_port)
              }}
            />
          )}

          {probe != null && mode === 'forward' && probe.forward_port == null && (
            <p className="rounded bg-amber-950 px-3 py-2 text-sm text-amber-300">
              未检测到中转机 DNAT 转发规则：接管后转发将不生效，请确认规则已配置或手动填写转发端口
            </p>
          )}
        </>
      )}

      {buildJob != null && (
        <div className="space-y-1 rounded border border-neutral-800 p-3">
          <JobProgressPanel progress={progressView(buildJob)} />
        </div>
      )}

      {adopted && <p className="text-sm text-emerald-400">已接管链路</p>}
      {finished && !failed && <p className="text-sm text-emerald-400">建链完成</p>}

      <div className="flex gap-2">
        {running ? (
          <button type="button" onClick={() => void handleCancelJob()} className="rounded bg-red-800 px-3 py-1.5 text-sm text-white hover:bg-red-700">
            取消任务
          </button>
        ) : failed || finished || adopted ? (
          <>
            {failed && !adopted && (
              <button type="button" onClick={handleRetry} className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600">
                重试
              </button>
            )}
            <button type="button" onClick={onClose} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
              关闭
            </button>
          </>
        ) : (
          <>
            {probe != null && (
              <>
                {probe.handshake ? (
                  <>
                    <button type="button" onClick={() => void handleAdopt()} disabled={submitting} className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50">
                      {submitting ? '接管中…' : '接管管理'}
                    </button>
                    <button type="button" onClick={() => void handleBuild('rebuild')} disabled={submitting} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50">
                      {submitting ? '提交中…' : '重新建链'}
                    </button>
                  </>
                ) : probe.relay_wg || probe.landing_wg ? (
                  <button type="button" onClick={() => void handleBuild('rebuild')} disabled={submitting} className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50">
                    {submitting ? '提交中…' : '重新建链'}
                  </button>
                ) : (
                  <button type="button" onClick={() => void handleBuild('build')} disabled={submitting} className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50">
                    {submitting ? '提交中…' : '建链'}
                  </button>
                )}
              </>
            )}
            <button
              type="button"
              onClick={() => void handleProbe()}
              disabled={probing || submitting || relayId == null || landingId == null || running}
              className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50"
            >
              {probing ? '探测中…' : '开始探测'}
            </button>
            <button type="button" onClick={onClose} className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700">
              取消
            </button>
          </>
        )}
      </div>
    </div>
  )
}
