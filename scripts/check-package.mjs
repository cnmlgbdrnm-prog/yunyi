// 打包资源完整性检查：断言 src-tauri/binaries/mihomo.exe 存在、大小 > 1MB、
// 哈希与 sidecar.rs SIDECAR_EXE_SHA256 一致，且 .gitignore 已忽略 binaries 目录。
// 供 npm run check:package 使用；任一断言失败打印中文原因并退出非零。
import { createHash } from 'node:crypto'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const EXE_PATH = join(ROOT, 'src-tauri', 'binaries', 'mihomo.exe')
const TOR_DIR = join(ROOT, 'src-tauri', 'binaries', 'tor')
const TOR_EXE = join(TOR_DIR, 'tor.exe')
// 与 src-tauri/src/speed/sidecar.rs SIDECAR_EXE_SHA256 同值
const EXE_SHA256 = 'a4feef7e4f8cbaf19e2079b3caa3e23821dfe419ad2e39e0539b843bd2d4d1be'
const TOR_EXE_SHA256 = '60c45b01938c799862e511a9a5bab12f959a819c6264a24502edc342165f570c'
const MIN_BYTES = 1024 * 1024

const sha256File = (p) => createHash('sha256').update(readFileSync(p)).digest('hex')

const failures = []
if (!existsSync(EXE_PATH)) {
  failures.push(`缺少 ${EXE_PATH}：先执行 npm run fetch:sidecar 下载 mihomo`)
} else {
  const size = statSync(EXE_PATH).size
  if (size <= MIN_BYTES) {
    failures.push(`mihomo.exe 大小异常（${size} 字节），疑似占位或损坏文件`)
  }
  if (sha256File(EXE_PATH) !== EXE_SHA256) {
    failures.push('mihomo.exe 哈希不匹配（与 sidecar.rs SIDECAR_EXE_SHA256 不一致），请重新执行 npm run fetch:sidecar')
  }
}
if (!existsSync(TOR_EXE)) {
  failures.push(`缺少 ${TOR_EXE}：先执行 npm run fetch:tor`)
} else {
  if (sha256File(TOR_EXE) !== TOR_EXE_SHA256) {
    failures.push('tor.exe 哈希不匹配，请重新执行 npm run fetch:tor')
  }
}
const tauriConfig = JSON.parse(readFileSync(join(ROOT, 'src-tauri', 'tauri.conf.json'), 'utf8'))
if (!tauriConfig.bundle?.resources?.includes('binaries/tor/')) {
  failures.push('tauri.conf.json 未把 Tor 目录加入 bundle.resources')
}
const gitignore = readFileSync(join(ROOT, '.gitignore'), 'utf8')
if (!gitignore.includes('src-tauri/binaries/')) {
  failures.push('.gitignore 未忽略 src-tauri/binaries/，二进制会被误提交')
}

if (failures.length > 0) {
  for (const f of failures) console.error(`[check-package] ${f}`)
  process.exit(1)
}
console.log('[check-package] mihomo 与 Tor 资源完整，.gitignore 已忽略 binaries，通过')
