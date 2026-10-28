// 上游出口入口协议的展示名。后端 `REUSABLE_PROTOS` 是事实源，这里只负责人类可读的
// 变体名；两处漂移只会让文字不再精确，不会影响选择/校验。
export const ENTRY_PROTO_LABEL: Record<string, string> = {
  vless: 'VLESS + REALITY + Vision',
  trojan: 'Trojan + REALITY',
  vmess: 'VMess + WebSocket',
  hysteria2: 'Hysteria2（QUIC）',
  tuic: 'TUIC（QUIC）',
}

export function entryProtoLabel(proto: string): string {
  return ENTRY_PROTO_LABEL[proto] ?? proto
}
