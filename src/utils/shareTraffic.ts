import { formatBytes } from './format'

/** Share traffic uses decimal SI units; 100 GB is exactly 100 billion bytes. */
export const SHARE_GB = 1_000_000_000

/** 分享流量与连接日志、中转链路共用同一个十进制格式化器，避免两套单位口径。 */
export function formatShareBytes(bytes: number): string {
  return formatBytes(bytes)
}
