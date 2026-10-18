// 下载固定版本的 Windows Tor Expert Bundle，校验官方发布的归档哈希，
// 并把 tor.exe 及同目录的 DLL 一起放进 Tauri 资源目录。
import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import http from 'node:http'
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const DEST = join(ROOT, 'src-tauri', 'binaries', 'tor')
const VERSION = '15.0.25'
const ARCHIVE = `tor-expert-bundle-windows-x86_64-${VERSION}.tar.gz`
const URL = `https://dist.torproject.org/torbrowser/${VERSION}/${ARCHIVE}`
// 网络受限时可指定完整镜像 URL；归档哈希始终按官方值校验。
const DOWNLOAD_URL = process.env.TOR_ARCHIVE_URL ?? URL
// https://dist.torproject.org/torbrowser/15.0.25/sha256sums-signed-build.txt
const ARCHIVE_SHA256 = 'c1988c701de2f24f7d098d1d6d7f507b58f27740d097f29c246e0524d639b252'
// 与 src-tauri/src/tor/mod.rs 的 TOR_EXE_SHA256 同值。
const EXE_SHA256 = '8c38513b3c4584154158e7c584f4144849dc679a64cd679553d72dad28230c54'

const sha256 = (path) => createHash('sha256').update(readFileSync(path)).digest('hex')

// Node 24.14+ 的 fetch 需显式启用环境代理；旧版本可用 --use-env-proxy 启动脚本。
http.setGlobalProxyFromEnv?.()

function findExe(dir) {
  for (const ent of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, ent.name)
    if (ent.isDirectory()) {
      const found = findExe(path)
      if (found) return found
    } else if (ent.isFile() && ent.name.toLowerCase() === 'tor.exe') {
      return path
    }
  }
  return null
}

async function run() {
  const current = join(DEST, 'tor.exe')
  if (existsSync(current) && sha256(current) === EXE_SHA256) {
    console.log('[fetch-tor] Tor 与依赖已存在，跳过下载')
    return
  }

  const temp = mkdtempSync(join(tmpdir(), 'vpnctl-tor-'))
  try {
    const archive = join(temp, ARCHIVE)
    console.log(`[fetch-tor] 下载 ${DOWNLOAD_URL}`)
    const response = await fetch(DOWNLOAD_URL, { signal: AbortSignal.timeout(120_000) })
    if (!response.ok) throw new Error(`下载失败：HTTP ${response.status}`)
    const bytes = Buffer.from(await response.arrayBuffer())
    writeFileSync(archive, bytes)
    if (sha256(archive) !== ARCHIVE_SHA256) throw new Error('Tor 归档 SHA256 不匹配')

    const extracted = join(temp, 'extracted')
    mkdirSync(extracted)
    // 以 temp 为工作目录并传相对文件名：Git Bash 下 Node 解析到的是 GNU tar，
    // 它会把 Windows 盘符路径里的 `C:` 当成远程主机（Cannot connect to C）。
    const tar = spawnSync('tar', ['-xzf', ARCHIVE, '-C', 'extracted'], {
      cwd: temp,
      stdio: 'inherit',
    })
    if (tar.status !== 0) throw new Error(`解压 Tor 归档失败（tar 退出码 ${tar.status}）`)
    const exe = findExe(extracted)
    if (!exe || sha256(exe) !== EXE_SHA256) throw new Error('归档内 tor.exe 缺失或 SHA256 不匹配')
    const source = dirname(exe)
    // Windows Expert Bundle 无独立 DLL；保留官方目录布局。
    mkdirSync(DEST, { recursive: true })
    cpSync(source, DEST, { recursive: true, force: true })
    if (!statSync(join(DEST, 'tor.exe')).isFile()) throw new Error('Tor 资源目录不完整')
    console.log(`[fetch-tor] 完成：${DEST}`)
  } finally {
    if (!resolve(temp).startsWith(resolve(tmpdir()) + sep)) {
      throw new Error('Tor 临时目录越界，拒绝清理')
    }
    rmSync(temp, { recursive: true, force: true })
  }
}

run().catch((e) => {
  const message = e instanceof Error ? `${e.message}${e.cause ? ` (${String(e.cause)})` : ''}` : String(e)
  console.error(`[fetch-tor] ${message}；可用 TOR_ARCHIVE_URL 指定归档镜像`)
  process.exitCode = 1
})
