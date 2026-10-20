// 设置视图：订阅服务启停与订阅 URL 展示、配置导出（YAML / sing-box）、
// 关于区块（当前版本展示 + 检查更新：无更新/发现新版本+说明/下载安装/失败原因）。
// 订阅 URL 只在服务运行时对外（127.0.0.1）可访问；端口与 token 来自设置存储。
// 更新安装完成后由 updater 插件自动重启应用，前端不做 relaunch。
import { useEffect, useRef, useState } from 'react'
import {
  checkForUpdate,
  exportSingboxSelected,
  exportYamlSelected,
  getAppVersion,
  getSettings,
  getSubServerStatus,
  netFirewallClear,
  netServiceInstall,
  netServiceState,
  netServiceUninstall,
  netStatus,
  setSetting,
  startSubServer,
  stopSubServer,
} from '../api'
import ExportSelectDialog from '../components/ExportSelectDialog'
import type {
  ExportOutcome,
  FirewallState,
  NetStatus,
  ServiceState,
  UpdateInfo,
} from '../api/types'

/** 把任意抛错转成用户可读的原因文本：Error 实例取 message，其余原样字符串化 */
function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/**
 * 会话阻断状态的完整说法。**这里是那个「完整清单」的落点**：仪表盘上只剩一小行
 * 标记（用户嫌整段话丑），而原因——哪些程序没挡住、为什么——必须能查得到，
 * 否则就是把 fail-open 藏起来。设置页有地方，就把话说全。
 */
const FIREWALL_TEXT: Record<FirewallState, string> = {
  off: '未启用（当前没开网络，或本次会话没有要挡的程序）',
  applied: '已生效（关注程序的公网 IPv4 / IPv6 直连已被挡）',
  partial: '只装上了一部分——没挡住的那部分流量仍会绕过代理直连',
  denied: '被防火墙拒绝：网络照常可用，但非 TUN 下推不到的流量不会被挡住',
  unavailable: '没能生效：服务不可用（或服务是旧构建，不认识阻断命令）',
  unsupported: '当前系统没有这套防火墙语义',
}

// 检查更新流程状态机：available 持有可下载的更新；failed 区分检查与下载阶段
type UpdateState =
  | { kind: 'idle' }
  | { kind: 'checking' }
  | { kind: 'latest' }
  | { kind: 'available'; info: UpdateInfo }
  | { kind: 'updating' }
  | { kind: 'completed' }
  | { kind: 'failed'; phase: 'check' | 'install'; message: string }

export default function Settings() {
  const [port, setPort] = useState('8731')
  const [token, setToken] = useState('')
  const [running, setRunning] = useState(false)
  const [exported, setExported] = useState<ExportOutcome | null>(null)
  // 待导出的格式：null = 未在选择；否则选择弹窗打开中
  const [exportTarget, setExportTarget] = useState<'yaml' | 'singbox' | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [version, setVersion] = useState<string | null>(null)
  const [updateState, setUpdateState] = useState<UpdateState>({ kind: 'idle' })
  // 导出智能分流开关（缺省开；设置表 export_smart_split "1"/"0"）
  const [smartSplit, setSmartSplit] = useState(true)
  // 分享域名的 ACME 邮箱（选填；空 = 签发时不带邮箱，证书照常有效，只是收不到到期提醒）
  const [acmeEmail, setAcmeEmail] = useState('')
  const [savedAcmeEmail, setSavedAcmeEmail] = useState('')
  const [acmeNotice, setAcmeNotice] = useState<string | null>(null)
  // 提权服务状态：null = 还没查到（禁用按钮，防止在未知状态下点了弹 UAC）
  const [svc, setSvc] = useState<ServiceState | null>(null)
  const [svcBusy, setSvcBusy] = useState(false)
  // 清除残留阻断规则：忙碌态与结果提示
  const [fwBusy, setFwBusy] = useState(false)
  const [fwNotice, setFwNotice] = useState<string | null>(null)
  // 上网状态（只为读会话阻断那一对字段）。null = 还没查到/查不到 → 显示「未知」
  const [net, setNet] = useState<NetStatus | null>(null)

  // 卸载标志：UAC 轮询是事件处理器里起的 for 循环，组件卸载不会自动中断它。
  // 轮询窗口最长 6s（12 × 500ms），不拦就是 6 秒的无效 setState + 十几次 IPC 往返。
  // 手法同 Shares.tsx:49（那里是 async 回调里的局部 alive 变量，这里是跨 await 的 ref）。
  const aliveRef = useRef(true)
  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  useEffect(() => {
    getSettings(['sub_port', 'sub_token', 'export_smart_split', 'share_acme_email'])
      .then((s) => {
        if (s.sub_port) setPort(s.sub_port)
        if (s.sub_token) setToken(s.sub_token)
        setSmartSplit(s.export_smart_split !== '0')
        setAcmeEmail(s.share_acme_email ?? '')
        setSavedAcmeEmail(s.share_acme_email ?? '')
      })
      .catch((e) => setError(String(e)))
    // 视图切换后组件重新挂载，本地 running 态会重置：
    // 必须向后端查询真实运行态，否则运行中的服务会显示成已停止且无法停止
    getSubServerStatus()
      .then(setRunning)
      .catch((e) => setError(String(e)))
    // 版本号来自 tauri.conf.json；读取失败时降级展示「未知」
    getAppVersion()
      .then(setVersion)
      .catch(() => setVersion(null))
    // 服务状态查不到就保持 null（按钮禁用）——在未知状态下点安装会白弹一次 UAC
    netServiceState()
      .then(setSvc)
      .catch(() => setSvc(null))
    // 会话阻断的真实状态与原因。**查不到就保持 null 并显示「未知」**，不要瞎报：
    // 后端比前端旧时这个字段整个缺失，猜一个「已生效」等于替用户签字
    netStatus()
      .then(setNet)
      .catch(() => setNet(null))
  }, [])

  /**
   * 装/卸网络服务：只见证「提权助手已拉起」，**不能断言成功**。
   *
   * 后端不等 UAC 窗口（模态，等下去会卡住主线程），所以这里轮询真实状态，
   * 拿结果说话。轮询 12 次 × 500ms 覆盖用户在 UAC 窗口犹豫的时间；
   * 超时就如实说「没看到结果」，不假装成功。
   */
  const handleServiceToggle = async (install: boolean) => {
    setError(null)
    setSvcBusy(true)
    try {
      if (install) await netServiceInstall()
      else await netServiceUninstall()
      for (let i = 0; i < 12; i++) {
        await new Promise((r) => setTimeout(r, 500))
        if (!aliveRef.current) return
        const s = await netServiceState()
        if (!aliveRef.current) return
        setSvc(s)
        if (s.installed === install) return
      }
      setError(
        install
          ? '没等到服务装好。若刚才在 UAC 窗口点了「否」，重新点一次安装即可'
          : '没等到服务卸掉。若刚才在 UAC 窗口点了「否」，重新点一次卸载即可',
      )
    } catch (e) {
      if (aliveRef.current) setError(errorMessage(e))
    } finally {
      if (aliveRef.current) setSvcBusy(false)
    }
  }

  /**
   * 清除残留的阻断规则。
   *
   * 这是**逃生门**：正常路径上不该有残留（关网络无条件撤、开网络自愈上次强杀留下
   * 的），但残留的 BLOCK 规则是真会挡住用户网络的，所以必须有一条不依赖任何自动
   * 路径的手工出口。网络跑着时后端会拒绝——那时规则正在生效，清掉等于界面撒谎。
   */
  const handleFirewallClear = async () => {
    setError(null)
    setFwNotice(null)
    setFwBusy(true)
    try {
      const r = await netFirewallClear()
      setFwNotice(
        r.removed > 0 ? `已清除 ${r.removed} 条残留的阻断规则` : '没有残留的阻断规则',
      )
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setFwBusy(false)
    }
  }

  const handleCheckUpdate = async () => {
    setUpdateState({ kind: 'checking' })
    try {
      const info = await checkForUpdate()
      setUpdateState(info ? { kind: 'available', info } : { kind: 'latest' })
    } catch (e) {
      setUpdateState({ kind: 'failed', phase: 'check', message: errorMessage(e) })
    }
  }

  const handleInstall = async () => {
    const info = updateState.kind === 'available' ? updateState.info : null
    if (!info) return
    setUpdateState({ kind: 'updating' })
    try {
      await info.downloadAndInstall()
      setUpdateState({ kind: 'completed' })
    } catch (e) {
      setUpdateState({ kind: 'failed', phase: 'install', message: errorMessage(e) })
    }
  }

  // 订阅 URL 始终展示（客户端配置用），运行状态单独指示
  const subUrl = `http://127.0.0.1:${port}/sub/${token}`

  const handleStart = async () => {
    setError(null)
    try {
      await startSubServer(Number(port), token)
      setRunning(true)
    } catch (e) {
      setError(String(e))
    }
  }

  const handleStop = async () => {
    setError(null)
    try {
      await stopSubServer()
      setRunning(false)
    } catch (e) {
      setError(String(e))
    }
  }


  const handleToggleSmartSplit = async (value: boolean) => {
    setError(null)
    setSmartSplit(value)
    try {
      await setSetting('export_smart_split', value ? '1' : '0')
    } catch (e) {
      setSmartSplit(!value) // 保存失败回滚
      setError(String(e))
    }
  }

  const handleSaveAcmeEmail = async () => {
    setError(null)
    setAcmeNotice(null)
    const next = acmeEmail.trim()
    try {
      await setSetting('share_acme_email', next)
      setSavedAcmeEmail(next)
      setAcmeEmail(next)
      setAcmeNotice(next ? '已保存' : '已清空：签发时将不填写邮箱')
    } catch (e) {
      setAcmeEmail(savedAcmeEmail) // 保存失败回滚到上次成功的值
      setError(String(e))
    }
  }

  // 不设宽度上限：窗口拉大时**内容跟着变宽**（分区铺满整行），而不是拆成两列或把
  // 单列钉死在中间留白。分区之间的间距靠 space-y。
  return (
    <div className="space-y-6">
      <h2 className="text-xl font-semibold">设置</h2>
      {error && (
        <p role="alert" className="rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      <section className="space-y-3 rounded border border-neutral-800 p-4">
        <h3 className="font-medium">订阅服务</h3>
        <label className="block text-sm">
          端口
          <input
            className="mt-1 w-32 rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
            value={port}
            onChange={(e) => setPort(e.target.value)}
          />
        </label>
        <p className="text-sm text-neutral-400">
          状态：{running ? <span className="text-emerald-400">运行中</span> : <span>已停止</span>}
        </p>
        <p className="break-all rounded bg-neutral-900 px-3 py-2 text-sm text-emerald-300">
          {subUrl}
        </p>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={handleStart}
            disabled={running}
            className="rounded bg-emerald-700 px-3 py-1.5 text-sm text-white hover:bg-emerald-600 disabled:opacity-50"
          >
            启动订阅服务
          </button>
          <button
            type="button"
            onClick={handleStop}
            disabled={!running}
            className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50"
          >
            停止
          </button>
        </div>
      </section>

      <section className="space-y-3 rounded border border-neutral-800 p-4">
        <h3 className="font-medium">分享 HTTPS</h3>
        <p className="text-sm text-neutral-400">
          新分享强制 HTTPS。分享域名在「服务器 → 编辑」里配置；未配置时，IP 服务器会自动使用
          sslip.io 域名签发。ACME 邮箱选填：不填也能签发，只是收不到证书到期提醒。
        </p>
        <label className="block text-sm">
          ACME 邮箱
          <input
            aria-label="ACME 邮箱"
            value={acmeEmail}
            onChange={(e) => {
              setAcmeEmail(e.target.value)
              setAcmeNotice(null)
            }}
            placeholder="you@example.com"
            className="mt-1 w-72 rounded border border-neutral-700 bg-neutral-900 px-2 py-1"
          />
        </label>
        <div className="flex items-center gap-3">
          <button
            type="button"
            onClick={() => void handleSaveAcmeEmail()}
            className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700"
          >
            保存邮箱
          </button>
          {acmeNotice && <span className="text-sm text-emerald-400">{acmeNotice}</span>}
        </div>
      </section>

      <section className="space-y-3 rounded border border-neutral-800 p-4">
        <h3 className="font-medium">配置导出</h3>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={smartSplit}
            onChange={(e) => handleToggleSmartSplit(e.target.checked)}
          />
          导出配置包含智能分流规则（国内直连，国外走节点）
        </label>
        <p className="text-xs text-neutral-500">
          关闭后导出配置不含 GEOSITE/GEOIP CN 直连规则，所有流量按默认分组转发
        </p>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => setExportTarget('yaml')}
            className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500"
          >
            导出 YAML
          </button>
          <button
            type="button"
            onClick={() => setExportTarget('singbox')}
            className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500"
          >
            导出 sing-box
          </button>
        </div>
        {exportTarget && (
          <ExportSelectDialog
            formatLabel={exportTarget === 'yaml' ? 'YAML' : 'sing-box'}
            onExport={async (nodeIds) => {
              setError(null)
              setExported(
                exportTarget === 'yaml'
                  ? await exportYamlSelected(nodeIds)
                  : await exportSingboxSelected(nodeIds),
              )
            }}
            onClose={() => setExportTarget(null)}
          />
        )}
        {exported && (
          <div className="space-y-1 text-sm">
            <p className="text-emerald-400">已导出到：{exported.path}</p>
            {exported.skipped_ssr > 0 && (
              <p className="text-amber-300">
                {exported.skipped_ssr} 个节点未包含（SSR 不受支持；sing-box 导出还会跳过 Tor 隐私线路）
              </p>
            )}
          </div>
        )}
      </section>

      <section className="space-y-3 rounded border border-neutral-800 p-4">
        <h3 className="font-medium">网络服务</h3>
        <p className="text-sm text-neutral-400">
          建 TUN 网卡需要管理员权限：安装时过一次 UAC，之后开关 TUN 不再弹窗。
        </p>
        <p className="text-sm">
          状态：
          {svc === null
            ? '未知'
            : svc.running
              ? '已安装 · 运行中'
              : svc.installed
                ? '已安装 · 已停止'
                : '未安装'}
        </p>
        <p className="text-sm text-neutral-400">
          系统代理模式下，为启动时可定位到的关注程序安装公网 IPv4 / IPv6 出站阻断；它们若不使用本地代理，
          需要开启 TUN 才能继续联网。关闭网络时撤销阻断。改动服务后需重新安装才生效。
        </p>
        {/* 会话阻断的**完整**状态与原因。仪表盘上已经收成一行小字（用户嫌整段话丑），
            原因就挪到这里：哪些程序没挡住、为什么，必须查得到——删掉不提等于把
            fail-open 藏起来，那正是这个功能要消灭的东西。 */}
        <p className="text-sm">
          会话阻断：
          {net === null || !net.firewall
            ? '未知'
            : (FIREWALL_TEXT[net.firewall] ?? net.firewall)}
        </p>
        {net?.firewall_detail && (
          <p className="text-sm text-neutral-400">{net.firewall_detail}</p>
        )}
        <div className="flex items-center gap-3">
          <button
            type="button"
            onClick={() => handleServiceToggle(!svc?.installed)}
            disabled={svc === null || svcBusy}
            className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50"
          >
            {svcBusy
              ? '等待 UAC…'
              : svc?.installed
                ? '卸载网络服务'
                : '安装网络服务'}
          </button>
          <button
            type="button"
            onClick={handleFirewallClear}
            disabled={fwBusy}
            className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50"
          >
            {fwBusy ? '清除中…' : '清除残留阻断规则'}
          </button>
        </div>
        {fwNotice && <p className="text-sm text-emerald-400">{fwNotice}</p>}
      </section>

      <section className="space-y-3 rounded border border-neutral-800 p-4">
        <h3 className="font-medium">关于</h3>
        <p className="text-sm text-neutral-400">
          当前版本：{version ? `v${version}` : '未知'}
        </p>
        <div className="flex items-center gap-3">
          <button
            type="button"
            onClick={handleCheckUpdate}
            disabled={updateState.kind === 'checking' || updateState.kind === 'updating'}
            className="rounded bg-neutral-800 px-3 py-1.5 text-sm hover:bg-neutral-700 disabled:opacity-50"
          >
            检查更新
          </button>
          {updateState.kind === 'checking' && (
            <span className="text-sm text-neutral-400">正在检查更新…</span>
          )}
        </div>
        {updateState.kind === 'latest' && (
          <p className="text-sm text-emerald-400">已是最新版本</p>
        )}
        {updateState.kind === 'available' && (
          <div className="space-y-2">
            <p className="text-sm text-emerald-400">
              发现新版本 v{updateState.info.version}
            </p>
            {updateState.info.body && (
              <pre className="max-h-48 overflow-auto whitespace-pre-wrap rounded bg-neutral-900 px-3 py-2 text-xs text-neutral-300">
                {updateState.info.body}
              </pre>
            )}
            <button
              type="button"
              onClick={handleInstall}
              className="rounded bg-blue-600 px-3 py-1.5 text-sm text-white hover:bg-blue-500"
            >
              立即更新
            </button>
          </div>
        )}
        {updateState.kind === 'updating' && (
          <p className="text-sm text-neutral-400">
            正在下载并安装更新，完成后应用将自动重启
          </p>
        )}
        {updateState.kind === 'completed' && (
          <p className="text-sm text-emerald-400">更新已完成，应用即将重启</p>
        )}
        {updateState.kind === 'failed' && (
          <p role="alert" className="text-sm text-red-300">
            {updateState.phase === 'check' ? '检查更新失败：' : '更新下载失败：'}
            {updateState.message}
          </p>
        )}
      </section>
    </div>
  )
}
