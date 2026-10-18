// 服务器地址族的展示口径：与后端 domain::effective_host 保持同一语义——
// addr_family=v6 且登记了 host_v6 时用 IPv6，否则回落到 host（IPv4/域名）。
import type { Server } from '../api/types'

/** 当前生效的管理/节点地址（不带方括号）。 */
export function effectiveServerHost(s: Pick<Server, 'host' | 'host_v6' | 'addr_family'>): string {
  return s.addr_family === 'v6' && s.host_v6 !== '' ? s.host_v6 : s.host
}

/** 带端口展示：IPv6 字面量必须加方括号，否则 host:port 无法区分。 */
export function formatHostPort(host: string, port: number): string {
  return host.includes(':') && !host.startsWith('[') ? `[${host}]:${port}` : `${host}:${port}`
}
