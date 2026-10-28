// 运行时连接日志 + 泄漏检测面板（挂在仪表盘网络卡下方）。
//
// 数据来自后端秒级采样推来的 `net://conn_log` 事件，本组件只在挂载时拉一次当前窗口。
// **不轮询**：连接短命，轮询只在「恰好活着」的瞬间看得到东西，而漏掉的那些连接
// 正是这个功能要找的。
//
// 三处刻意的措辞，改文案前先读一遍：
// - `suspect` 显示为**疑点**，绝不叫泄漏。判据是「拿不准就不下结论」，
//   面板满屏假警报的后果是用户学会无视它，真泄漏也一起被无视。
// - `direct_cn` 显示为**国内直连**：目标在国内，规则本就让它直连
//   （`GEOIP,CN,DIRECT` / `GEOSITE,cn,DIRECT`），**不是泄漏**。现场 5 条「泄漏」
//   报告里 4 条是这种，判成泄漏就是假警报。
// - 只有经 mihomo 的行才有权威域名与匹配规则；直连行这两列是 `—`，
//   `—` 本身就是「这条没进代理」的证据，不要用 DIRECT 之类的词去填。直连行的域名
//   是**尽力而为**地查系统 DNS 缓存来的，界面上必须和 mihomo 的域名长得不一样。
import { useEffect, useState } from 'react'
import { listen } from '@tauri-apps/api/event'
import { netConnLog } from '../api'
import type { ConnLogSnapshot, ConnRow } from '../api/types'
import { formatBytes } from '../utils/format'

/** 一屏最多几行（后端已按严重度排序，泄漏不会被截掉） */
const ROW_LIMIT = 200

/**
 * 表头列，顺序即渲染顺序。**表头与判据行的跨列数共用这一个数组**：加一列却忘了改
 * `colSpan` 时，判据文字会挤在左边几列里，而 jsdom 根本不校验跨列——四个命令会全绿，
 * 缺陷直达用户（本面板真的漏过一次：加了「国家/地区」列而 colSpan 还是 9）。
 */
const COLUMNS = [
  'PROCESS',
  'DOMAIN',
  'RESOLVED_IP',
  'GeoIP 标签',
  'IPv4-IPv6',
  'TCP-UDP',
  'MATCHED_RULE',
  'OUTBOUND',
  'PROXY_ENDPOINT',
  // 上下行**必须分列**：合成一个数会掩盖「只上传不下载」（或反之）的异常形态
  '上行',
  '下行',
] as const

const VERDICT_TEXT: Record<string, string> = {
  leak: '泄漏',
  suspect: '疑点',
  direct_cn: '国内直连',
  proxy_egress: '隧道出站',
  known_server: '已知服务器',
  direct: '规则直连',
  proxied: '经代理',
}

const VERDICT_CLASS: Record<string, string> = {
  leak: 'bg-red-900 text-red-100',
  suspect: 'bg-amber-900 text-amber-100',
  // 国内直连：配色刻意中性偏冷——它既不是泄漏（红），也不是「已确认安全」（绿）。
  // 用绿色会让人以为这条被核验过，而它只是「规则本来就这么定的」
  direct_cn: 'bg-sky-900 text-sky-100',
  // 隧道出站：比「规则直连」还淡。它是本机代理拨号到自己节点的那条连接，
  // 既不值得看也不该消失——26 条同一目标的记录，抢眼就会把真信号压下去
  proxy_egress: 'bg-neutral-800 text-neutral-400',
  known_server: 'bg-neutral-800 text-neutral-400',
  direct: 'bg-neutral-700 text-neutral-300',
  proxied: 'bg-emerald-900 text-emerald-100',
}

/** 只按可核实的进程身份细分疑点；类别不代表远端域名或连接用途已核实。 */
function suspectCategory(process: string): { label: string; detail: string } {
  switch (process.toLowerCase()) {
    case 'tailscaled.exe':
      return { label: 'Tailscale', detail: 'Tailscale 自身连接；可能用于协调或中转，具体远端用途未核实。' }
    case 'svchost.exe':
      return { label: '系统服务', detail: 'Windows 服务宿主；还需按 PID 查具体服务，不能仅凭镜像名判安全。' }
    case 'mc-fw-host.exe':
      return { label: '安全软件', detail: 'McAfee Framework Host；目标服务与域名尚未核实。' }
    case 'nvidia overlay.exe':
      return { label: 'NVIDIA Overlay', detail: 'NVIDIA App 的 Overlay 进程直连；具体请求用途未核实。' }
    case 'cefsharp.browsersubprocess.exe':
      return { label: '嵌入式浏览器', detail: 'CefSharp 子进程；还需查父进程或路径才能确认宿主应用。' }
    default:
      return process ? { label: '其他应用', detail: '应用进程直连公网；具体用途未核实。' }
        : { label: '进程未知', detail: '未取到进程身份，需结合 PID 核实。' }
  }
}

/** 空值一律渲染 —，不要留白：留白看不出「没有」还是「没拿到」 */
function dash(v: string): string {
  return v && v.trim() ? v : '—'
}

/**
 * 一行的身份：**除两个字节计数外**整行参与。`upload`/`download` 每秒都在涨，
 * 让它们进键等于每秒换一次键——那正是本缺陷的另一种写法。
 *
 * 分隔符用 NUL：字段值来自进程名/域名/规则文本，用 `-`、`|` 之类都可能与字段内容
 * 撞车，撞了就会把两条不同的连接拼成同一个键。
 */
function rowIdentity(r: ConnRow): string {
  return [
    r.process,
    r.pid,
    r.domain,
    r.domain_source,
    r.country,
    r.resolved_ip,
    r.dest_port,
    r.family,
    r.proto,
    r.matched_rule,
    r.outbound,
    r.proxy_endpoint,
    r.start ?? '',
    r.verdict,
    r.reason,
  ].join('\u0000')
}

/**
 * 行键：身份 + **同身份内的出现序号**。
 *
 * 为什么要序号：`pid + 目标` 在 UDP 场景并不唯一（同一个 pid 打同一目标的多条连接）。
 * 后端 join 用的键是 `(proto, 本地源端口)`（connlog.rs:501），而本地源端口**没有进
 * IPC 载荷**（ConnRow 里没有这个字段），前端无从区分。
 *
 * 为什么不能用数组下标代替序号：下标代表**位置**，而后端每秒整份换新并按严重度重排
 * （connlog.rs 的 `rank`），位置一变键就变——React 认不出是同一行，只能卸载重建，
 * 整张表（上限 200 行）每秒重来一次。序号只在「身份相同的这几行」内部计数，
 * 整体重排不改变它；而那几行在屏幕上本来就分不出彼此。
 */
function rowKeys(rows: ConnRow[]): string[] {
  const seen = new Map<string, number>()
  return rows.map((r) => {
    const id = rowIdentity(r)
    const n = seen.get(id) ?? 0
    seen.set(id, n + 1)
    return n === 0 ? id : `${id}\u0000#${n}`
  })
}

function bytesText(n: number): string {
  // 0/未采样仍显示「—」；其余与分享页、中转链路共用十进制流量单位。
  return n ? formatBytes(n) : '—'
}

export default function ConnLogPanel() {
  const [snap, setSnap] = useState<ConnLogSnapshot | null>(null)
  const [onlyLeaks, setOnlyLeaks] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    // 挂载先拉当前窗口：面板打开前那一秒的记录不该因为「事件还没来」而空白。
    // 始终拉全量：只看泄漏只筛表格，不得把疑点分类与计数一起筛成 0。
    netConnLog(ROW_LIMIT, false)
      .then(setSnap)
      .catch((e) => setError(String(e)))
    // 订阅失败（事件通道不可用）不该让面板炸掉：挂载时那次拉取已经给了当前窗口，
    // 最坏只是不再增量刷新。这里吞掉 rejection，卸载路径上也就不会抛未处理的错误。
    const unlisten = listen('net://conn_log', (e) => {
      setSnap(e.payload as ConnLogSnapshot)
    }).catch(() => () => {})
    return () => {
      void unlisten.then((f) => f())
    }
  }, [])

  const all = snap?.rows ?? []
  // 事件带的是全量快照，渲染时再筛一次（否则切到「只看泄漏」后事件一到就打回原形）
  const rows = all.filter((r) => !onlyLeaks || r.verdict === 'leak')
  const keys = rowKeys(rows)
  const leaks = all.filter((r) => r.verdict === 'leak').length
  const suspects = all.filter((r) => r.verdict === 'suspect').length

  // 面板**不设 max-w**：这张表列多，10 列时就实测要 ~1134px 才排得开，现在多了一列只会
  // 更宽。钉在 896 会把它挤成横向滚动——最大化后明明有 ~1467px 可用，却还要拖着看。
  return (
    <div className="rounded border border-neutral-800 bg-neutral-900 p-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p className="text-sm text-neutral-400">连接日志与泄漏检测</p>
          <p className="mt-1 text-xs text-neutral-500">
            {snap === null
              ? '正在读取采样结果…'
              : snap.sampling
                ? `每秒采样中${snap.sampled_at ? ` · 最近 ${new Date(snap.sampled_at).toLocaleTimeString()}` : ''}`
                : '采样未运行'}
          </p>
        </div>
        <div className="flex items-center gap-3 text-xs">
          <span className={leaks > 0 ? 'text-red-300' : 'text-neutral-400'}>
            泄漏 {leaks}
          </span>
          <span className={suspects > 0 ? 'text-amber-300' : 'text-neutral-400'}>
            疑点 {suspects}
          </span>
          <label className="flex items-center gap-1 text-neutral-400">
            <input
              type="checkbox"
              checked={onlyLeaks}
              onChange={(e) => setOnlyLeaks(e.target.checked)}
            />
            只看泄漏
          </label>
        </div>
      </div>

      {/* 会话总计。**必须写明「经 mihomo」**：mihomo 的累计只统计经过它的流量，
          绕过它的泄漏流量它根本看不见（表格里那些显示 — 的行就是证据），
          写成「总流量」是假话。
          sampling 为假时整条隐藏：不在采样＝不在读 mihomo，留着一个冻住的数字
          看着可信、其实过期——未知不许冒充已知。 */}
      {snap?.sampling && (
        <>
          <p className="mt-2 flex flex-wrap items-center gap-3 text-xs text-neutral-400">
            <span>本次会话（经 mihomo）</span>
            <span className="text-neutral-200">↑ {bytesText(snap.upload_total)}</span>
            <span className="text-neutral-200">↓ {bytesText(snap.download_total)}</span>
          </p>
          {/* 这两句是**口径本身**，不是装饰：系统代理模式下数字天然远小于预期，
              唯一能自证这件事的线索必须在屏幕上，不能让用户自己去猜或去翻 CHANGELOG。 */}
          {/* 用字符串拼接而不是 JSX 换行：换行会折成一个空格，把「混合端口的程序计入」
              劈成两半——那是渲染产物，不该出现在源码意图里 */}
          <p className="mt-1 text-[11px] text-neutral-500">
            {'只统计经过 mihomo 的流量：绕过它的连接不计入。系统代理模式下只有走这个' +
              '混合端口的程序计入，TUN 模式才全部计入。'}
          </p>
        </>
      )}

      {error && (
        <p role="alert" className="mt-2 rounded bg-red-950 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}
      {snap?.last_error && (
        <p role="alert" className="mt-2 rounded border border-amber-700 bg-amber-950 px-3 py-2 text-sm text-amber-200">
          采样已停止：{snap.last_error}
        </p>
      )}
      {snap && !snap.sampling && !snap.last_error && (
        <p className="mt-2 text-xs text-neutral-500">
          开启网络后开始记录。检测器只看得见本机 socket 表：没进 mihomo 的连接
          （域名、匹配规则拿不到）在这里显示为 —，那正是「这条漏了」的证据。
        </p>
      )}

      {snap && rows.length === 0 && (
        <p className="mt-3 text-sm text-neutral-500">
          {onlyLeaks ? '当前没有已确认的泄漏。' : '本窗口没有记录到连接。'}
        </p>
      )}

      {rows.length > 0 && (
        <div className="mt-3 overflow-x-auto">
          <table className="w-full text-left text-xs">
            <thead className="text-neutral-500">
              <tr>
                {COLUMNS.map((c, i) => (
                  <th
                    key={c}
                    className={`py-1 font-normal ${i === COLUMNS.length - 1 ? '' : 'pr-3'}`}
                  >
                    {c}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((r, i) => (
                // 键见 rowKeys：身份 + 同身份内的出现序号。下标只用来取键，不参与键的构成
                <ConnLogRow key={keys[i]} row={r} />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* 裁决的含义写在面板里而不是靠用户猜：现场那 5 条「泄漏」报告里 4 条是国内直连，
          这条说明就是用来防同一类误读的 */}
      <p className="mt-3 text-xs text-neutral-500">
        <span className="text-sky-300">国内直连</span>
        ：目标在国内，按 GEOIP,CN / GEOSITE,cn 的规则国内走直连是本规则集的设计意图，
        <span className="text-neutral-300">不是泄漏</span>
        ；「疑点」是拿不准、也还没有证据，两者都不计入泄漏。
      </p>
      <p className="mt-1 text-xs text-neutral-500">
        DOMAIN 列：经 mihomo 的行是它报的真实 host；直连行尽力查系统 DNS 缓存（带
        <span className="mx-1 text-neutral-400">?</span>
        标记，一个 IP 可能对应多个域名）。Chrome/Edge 自带解析器甚至 DoH，这类程序多半查不到——
        拿不到就显示 —，面板不猜。
      </p>
      {snap && snap.watch.length > 0 && (
        <p className="mt-1 text-xs text-neutral-500">
          关注名单：{snap.watch.join('、')}
        </p>
      )}
    </div>
  )
}

/**
 * DOMAIN 单元格。
 *
 * 两种来源**必须看得出区别**：经 mihomo 的是这条连接真正请求的 host；
 * 直连行是从系统 DNS 缓存反查来的，**尽力而为**——同一个 IP 上可能挂着很多域名，
 * 而且自带解析器/DoH 的程序根本不会留下记录。带个 `?` 并挂 tooltip，用户才知道
 * 该信到什么程度。**绝不猜**：拿不到就 `—`（不做反向 DNS）。
 */
function DomainCell({ row }: { row: ConnRow }) {
  if (!row.domain) return <>—</>
  return (
    <>
      {row.domain}
      {row.domain_source === 'dns-cache' && (
        <span
          title="来自系统 DNS 缓存，可能不准或缺失"
          className="ml-1 cursor-help text-neutral-500"
        >
          ?
        </span>
      )}
    </>
  )
}

/** 一行连接。泄漏/疑点行下面是判据，正常行不占这行高度。 */
function ConnLogRow({ row }: { row: ConnRow }) {
  const suspicious = row.verdict === 'leak' || row.verdict === 'suspect'
  const category = row.verdict === 'suspect' ? suspectCategory(row.process) : null
  return (
    <>
      <tr
        data-verdict={row.verdict}
        className={row.verdict === 'leak' ? 'bg-red-950/60' : undefined}
      >
        <td className="py-1 pr-3" title={row.pid ? `pid ${row.pid}` : undefined}>
          {/* 徽标上挂判据：悬停能看到「为什么这么判」，表格本身不必为每条都多占一行 */}
          <span
            title={row.reason}
            className={`mr-2 rounded px-1 text-[10px] ${
              VERDICT_CLASS[row.verdict] ?? 'bg-neutral-700 text-neutral-300'
            }`}
          >
            {VERDICT_TEXT[row.verdict] ?? row.verdict}
          </span>
          {category && <span className="mr-2 text-[10px] text-amber-300">{category.label}</span>}
          {dash(row.process)}
        </td>
        <td className="py-1 pr-3">
          <DomainCell row={row} />
        </td>
        <td className="py-1 pr-3">
          {row.resolved_ip}:{row.dest_port}
        </td>
        {/* 国家：拿不到域名时（浏览器多半如此）它就是唯一的线索——
            「这条是国内的，本来就该直连」 */}
        <td className="py-1 pr-3">{dash(row.country)}</td>
        <td className="py-1 pr-3">{row.family}</td>
        <td className="py-1 pr-3">{row.proto}</td>
        <td className="py-1 pr-3">{dash(row.matched_rule)}</td>
        <td className="py-1 pr-3">{dash(row.outbound)}</td>
        <td className="py-1 pr-3">{dash(row.proxy_endpoint)}</td>
        <td className="py-1 pr-3">{bytesText(row.upload)}</td>
        {/* 未经 mihomo 的行这两个字段是 0，`bytesText(0)` 给 `—`——与左边三列
            同一套语义（`—` 是结论本身：这条压根没经过 mihomo），不需要特判 */}
        <td className="py-1">{bytesText(row.download)}</td>
      </tr>
      {suspicious && (
        <tr data-verdict={`${row.verdict}-reason`}>
          <td colSpan={COLUMNS.length} className="pb-2 text-[11px] text-neutral-500">
            {category && `${category.detail} `}{row.reason}
          </td>
        </tr>
      )}
    </>
  )
}
