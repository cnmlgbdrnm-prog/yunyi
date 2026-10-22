// 上游代理的一次性粘贴解析：把服务商给的完整链接拆成表单字段。
//
// 支持两种常见形态：
//   socks5://用户:密码@主机:端口   （socks5h/http 同理；凭据可含百分号转义）
//   主机:端口:用户名:密码          （很多住宅代理面板的 CLI 列表格式）
// 也接受无 scheme 的 `用户:密码@主机:端口`（协议沿用表单当前选择）。
// 服务商直接给一整条 curl 命令时，自动从中提取 `-x/--proxy` 后面那一段。
//
// 解析只做格式拆分，不判断账号是否可用——真正的连通性由部署后的出口 IP 探测回答。

export interface ParsedProxy {
  proto: 'socks5' | 'http'
  host: string
  port: number
  username: string
  password: string
}

const SCHEME_PROTO: Record<string, 'socks5' | 'http'> = {
  socks5: 'socks5',
  socks5h: 'socks5',
  http: 'http',
}

function normalizeProto(proto: string): 'socks5' | 'http' {
  return proto === 'http' ? 'http' : 'socks5'
}

function decodePart(value: string): string {
  try {
    return decodeURIComponent(value)
  } catch {
    // 非法的百分号转义按原文保留：密码里本来就可能出现 `%`
    return value
  }
}

function finish(
  proto: 'socks5' | 'http',
  rawHost: string,
  rawPort: string,
  username: string,
  password: string,
): ParsedProxy {
  const host = rawHost.replace(/^\[|\]$/g, '').trim()
  if (!host) throw new Error('主机不能为空')
  if (!/^\d+$/.test(rawPort)) throw new Error('端口必须是 1-65535 的整数')
  const port = Number(rawPort)
  if (port < 1 || port > 65535) throw new Error('端口必须在 1-65535 之间')
  return { proto, host, port, username, password }
}

function fromUrl(text: string, proto: 'socks5' | 'http', defaultPort?: string): ParsedProxy {
  let u: URL
  try {
    u = new URL(text)
  } catch {
    throw new Error('链接格式无法解析')
  }
  if ((u.pathname && u.pathname !== '/') || u.search || u.hash) {
    throw new Error('链接里不要带路径、查询参数或片段')
  }
  return finish(
    proto,
    u.hostname,
    u.port || defaultPort || '',
    decodePart(u.username),
    decodePart(u.password),
  )
}

/**
 * 从服务商给的 curl 命令里取出代理地址。
 *
 * 例如：`curl -v -x socks5://用户:密码@主机:端口 https://ipv4.icanhazip.com`
 * 只认命令开头的 curl（允许 `$ ` 提示符），避免把普通文本里的 `-x` 误当代理。
 */
function extractCurlProxy(text: string): string | null {
  if (!/^\$?\s*curl(?:\s|$)/i.test(text)) return null
  const patterns = [
    /(?:^|\s)--proxy(?:=|\s+)(?:"([^"]+)"|'([^']+)'|(\S+))/i,
    /(?:^|\s)-x\s*(?:"([^"]+)"|'([^']+)'|(\S+))/i,
  ]
  for (const re of patterns) {
    const m = re.exec(text)
    if (m) return m[1] ?? m[2] ?? m[3] ?? null
  }
  return null
}

/**
 * 解析一条代理配置。失败时抛出可直接展示给用户的中文原因。
 *
 * `fallbackProto` 用于没有 scheme 的形态（`主机:端口:用户名:密码` 与
 * `用户名:密码@主机:端口`），沿用表单里当前选中的协议。
 */
export function parseProxyInput(raw: string, fallbackProto: string): ParsedProxy {
  const text = raw.trim().replace(/^["']|["']$/g, '')
  if (!text) throw new Error('请粘贴代理链接或「主机:端口:用户名:密码」')
  // 服务商面板常直接给一整条 curl 命令：取出 -x/--proxy 后的代理再按链接解析
  const fromCurl = extractCurlProxy(text)
  if (fromCurl) return parseProxyInput(fromCurl, fallbackProto)
  if (/\s/.test(text)) throw new Error('一次只能填入一条，且不能包含空格')

  const scheme = /^([a-zA-Z][a-zA-Z0-9+.-]*):\/\//.exec(text)?.[1]?.toLowerCase()
  if (scheme) {
    const proto = SCHEME_PROTO[scheme]
    if (!proto) {
      throw new Error(`不支持的协议「${scheme}」；只支持 socks5 / socks5h / http`)
    }
    // URL 会把 http://host:80 的默认端口吃掉，这里按 scheme 补回来
    return fromUrl(text, proto, scheme === 'http' ? '80' : undefined)
  }

  // 无 scheme、带凭据分隔符：用户:密码@主机:端口
  if (text.includes('@')) {
    const proto = normalizeProto(fallbackProto)
    return fromUrl(`${proto}://${text}`, proto)
  }

  // 方括号 IPv6：主机:端口:用户名:密码
  if (text.startsWith('[')) {
    const m = /^(\[[^\]]+\]):(\d+)(?::([^:]*))?(?::(.*))?$/.exec(text)
    if (!m) throw new Error('格式应为 [IPv6]:端口:用户名:密码')
    return finish(
      normalizeProto(fallbackProto),
      m[1],
      m[2],
      m[3] ?? '',
      m[4] ?? '',
    )
  }

  const parts = text.split(':')
  if (parts.length < 2) {
    throw new Error('格式应为「主机:端口:用户名:密码」或「socks5://用户名:密码@主机:端口」')
  }
  // 未加方括号的 IPv6 会被冒号拆散：让端口校验去报错，并提示改用完整链接。
  // 不按「段数 > 4」拒绝——密码本身允许包含冒号（常见于自动生成的强口令）。
  const [host, port, username, ...rest] = parts
  if (port !== undefined && !/^\d+$/.test(port)) {
    throw new Error('端口必须是 1-65535 的整数；IPv6 地址请使用方括号或完整链接')
  }
  return finish(normalizeProto(fallbackProto), host, port ?? '', username ?? '', rest.join(':'))
}

/** 表单里出口名留空时的默认名：主机:端口（同机重名由后端报错提醒）。 */
export function defaultUpstreamName(parsed: ParsedProxy): string {
  return `${parsed.host}:${parsed.port}`
}
