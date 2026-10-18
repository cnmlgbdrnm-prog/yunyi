// 上次测速时间格式化：RFC3339 时间串 → 本地时区短格式「YYYY-MM-DD HH:mm」。
// null → 占位符「—」；解析失败原样返回；成功用 Date 本地 getter 拼接并补零
// （不用 toLocaleString：其输出依赖运行环境 locale，无法做确定性断言）。
// 纯函数无副作用；相邻单测 checkedAtFormat.test.ts 以镜像断言验证时区行为。
export function formatCheckedAt(iso: string | null): string {
  if (iso == null) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const pad = (n: number): string => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}
