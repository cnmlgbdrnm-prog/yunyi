#!/usr/bin/env node
// 敏感信息护栏：扫描仓库中所有已跟踪的文本文件，阻止真实主机地址与凭据进入版本库。
//
// 规则：
//   1. 公网 IPv4 必须出现在 scripts/sensitive-allowlist.txt 中；
//      私有/保留/文档网段（10/8、127/8、192.168/16、198.51.100/24 等）自动放行。
//   2. 私钥块：`-----BEGIN ... PRIVATE KEY-----` 一律拦截（UI 占位符行除外）。
//   3. 常见凭据前缀：GitHub token、AWS Access Key、Anthropic key、Slack token。
//
// 运行：npm run check:sensitive（CI 已接入）

import { execSync } from 'node:child_process'
import { readFileSync, existsSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const ALLOWLIST = join(ROOT, 'scripts', 'sensitive-allowlist.txt')

function loadAllowlist() {
  if (!existsSync(ALLOWLIST)) return new Set()
  return new Set(
    readFileSync(ALLOWLIST, 'utf8')
      .split(/\r?\n/)
      .map((l) => l.replace(/#.*$/, '').trim())
      .filter(Boolean),
  )
}

// 私有 / 保留 / 文档 / 组播 / 广播地址：这些不是真实主机地址
function isReserved(ip) {
  const p = ip.split('.').map(Number)
  if (p.length !== 4 || p.some((n) => n > 255)) return true
  const [a, b] = p
  if (a === 0 || a === 10 || a === 127) return true
  if (a === 100 && b >= 64 && b <= 127) return true // CGNAT
  if (a === 169 && b === 254) return true
  if (a === 172 && b >= 16 && b <= 31) return true
  if (a === 192 && b === 168) return true
  if (a === 192 && b === 0) return true // IETF 协议分配 / 文档
  if (a === 198 && (b === 18 || b === 19)) return true // 基准测试
  if (a === 198 && b === 51 && p[2] === 100) return true
  if (a === 203 && b === 0 && p[2] === 113) return true
  if (a >= 224) return true // 组播 / 保留 / 广播
  return false
}

const TOKEN_PATTERNS = [
  [/\bgh[pousr]_[A-Za-z0-9]{20,}\b/, 'GitHub token'],
  [/\bgithub_pat_[A-Za-z0-9_]{20,}\b/, 'GitHub PAT'],
  [/\bAKIA[0-9A-Z]{16}\b/, 'AWS Access Key'],
  [/\bsk-ant-[A-Za-z0-9_-]{20,}\b/, 'Anthropic API key'],
  [/\bxox[baprs]-[A-Za-z0-9-]{10,}\b/, 'Slack token'],
]

const IP_RE = /\b(?:\d{1,3}\.){3}\d{1,3}\b/g
const KEY_RE = /-----BEGIN (?:RSA |OPENSSH |EC |DSA |PGP )?PRIVATE KEY-----/

const allowlist = loadAllowlist()
const files = execSync('git ls-files -z', { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 })
  .split('\0')
  .filter(Boolean)

const findings = []
const report = (file, line, message) => findings.push(`${file}:${line}: ${message}`)

for (const file of files) {
  let text
  try {
    text = readFileSync(join(ROOT, file), 'utf8')
  } catch {
    continue
  }
  if (text.includes('\0')) continue // 二进制

  text.split(/\r?\n/).forEach((line, i) => {
    const lineNo = i + 1

    for (const [re, label] of TOKEN_PATTERNS) {
      if (re.test(line)) report(file, lineNo, `疑似 ${label}`)
    }

    if (KEY_RE.test(line) && !line.includes('placeholder')) {
      report(file, lineNo, '疑似私钥块')
    }

    for (const m of line.matchAll(IP_RE)) {
      const ip = m[0]
      if (isReserved(ip) || allowlist.has(ip)) continue
      report(file, lineNo, `未登记的公网 IP ${ip}（确为测试夹具请加入 scripts/sensitive-allowlist.txt）`)
    }
  })
}

if (findings.length) {
  console.error('[check-sensitive] 发现疑似敏感信息：')
  for (const f of findings) console.error('  ' + f)
  console.error(`共 ${findings.length} 处。如为公开的示例/实测第三方地址，请登记到 scripts/sensitive-allowlist.txt。`)
  process.exit(1)
}
console.log(`[check-sensitive] 通过：${files.length} 个已跟踪文件，未发现未登记的公网 IP 或凭据。`)
