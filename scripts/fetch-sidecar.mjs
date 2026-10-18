// mihomo sidecar 与分流数据集获取脚本。
// mihomo：下载 → zip sha256 校验 → 解压 → exe sha256 校验 → 落盘。
// geo：逐个下载（无哈希可钉，见 fetchGeo 的说明）。
// 输入：MIHOMO_MIRROR_URL（换源前缀，如 ghproxy）、MIHOMO_VER（默认 v1.19.10）、
//       GEODATA_BASE（换源）；
// 输出：src-tauri/binaries/mihomo.exe、src-tauri/binaries/geo/{geoip.dat,geosite.dat,country.mmdb}。
// 幂等：两者都按「已就位即跳过」处理，重复执行退出 0。
//
// geo 为什么必须在这里落盘：它是配置里 geodata-dir 指向的数据集，且要以
// tauri.conf.json 的资源形式打进安装包（服务以 SYSTEM 运行时从安装目录自带的数据集
// 取，不能去读非提权可写的用户数据目录）。缺失会让打包步骤直接失败。
// 哈希常量与默认版本绑定：换版本时两处哈希必须人工同步
// （zip digest 来自 GitHub API assets digest，2026-08-14 核验；
// exe digest 与 sidecar.rs 的 SIDECAR_EXE_SHA256 同值）。
// 任一步失败：删除半成品、打印中文原因、退出非零；临时目录结束必清理。
import { createHash } from 'node:crypto'
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from 'node:fs'
import { writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { spawnSync } from 'node:child_process'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const EXE_PATH = join(ROOT, 'src-tauri', 'binaries', 'mihomo.exe')
const ZIP_INNER_EXE = 'mihomo-windows-amd64.exe'

// mihomo v1.19.10 windows-amd64 校验常量（2026-08-14 核验）
const ZIP_SHA256 = '22f301591e522e81ecbd4ae2987b2fcb4642c234c870094904188c4dc1aceb60'
const EXE_SHA256 = 'a4feef7e4f8cbaf19e2079b3caa3e23821dfe419ad2e39e0539b843bd2d4d1be'

const VER = process.env.MIHOMO_VER ?? 'v1.19.10'
const DEFAULT_URL = `https://github.com/MetaCubeX/mihomo/releases/download/${VER}/mihomo-windows-amd64-${VER}.zip`
const MIRROR_PREFIX = process.env.MIHOMO_MIRROR_URL ?? ''
const DOWNLOAD_URL = MIRROR_PREFIX ? `${MIRROR_PREFIX}${DEFAULT_URL}` : DEFAULT_URL

// 与 net/geodata.rs 的 GEO_FILES / DEFAULT_GEODATA_BASE 同源，改一处必须改两处。
const GEO_DIR = join(ROOT, 'src-tauri', 'binaries', 'geo')
const GEO_FILES = ['geoip.dat', 'geosite.dat', 'country.mmdb']
const GEO_BASE =
  process.env.GEODATA_BASE ??
  'https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest'

const sha256File = (p) => createHash('sha256').update(readFileSync(p)).digest('hex')

/// geo 数据集：与 mihomo.exe 不同，上游 `latest` 是滚动 tag，**没有可钉的哈希**
/// （每次都变）。所以这里只做「存在即跳过」，不做校验——校验会随上游更新天天失败。
/// 数据本身是公开的只读分流表，不是信任边界上的东西。
const fetchGeo = async () => {
  mkdirSync(GEO_DIR, { recursive: true })
  for (const name of GEO_FILES) {
    const dst = join(GEO_DIR, name)
    if (existsSync(dst)) {
      console.log(`[fetch-sidecar] geo/${name} 已存在，跳过`)
      continue
    }
    const url = `${GEO_BASE}/${name}`
    console.log(`[fetch-sidecar] 下载 ${url}`)
    const res = await fetch(url)
    if (!res.ok) {
      throw new Error(
        `geo/${name} 下载失败：HTTP ${res.status} ${res.statusText}（网络受限可设 GEODATA_BASE 换源）`,
      )
    }
    await writeFile(dst, Buffer.from(await res.arrayBuffer()))
    console.log(`[fetch-sidecar] 完成：${dst}`)
  }
}

const fetchMihomo = async () => {
  // 幂等：已落盘且哈希匹配 → 跳过
  if (existsSync(EXE_PATH) && sha256File(EXE_PATH) === EXE_SHA256) {
    console.log('[fetch-sidecar] mihomo.exe 已是最新，跳过下载')
    return
  }
  if (existsSync(EXE_PATH)) {
    // 存量哈希不匹配：视为不可信，删除后重新下载
    console.log('[fetch-sidecar] 既有 mihomo.exe 哈希不匹配，删除后重新下载')
    rmSync(EXE_PATH)
  }
  const tmp = mkdtempSync(join(tmpdir(), 'mihomo-sidecar-'))
  try {
    console.log(`[fetch-sidecar] 下载 ${DOWNLOAD_URL}`)
    const res = await fetch(DOWNLOAD_URL)
    if (!res.ok) {
      throw new Error(`下载失败：HTTP ${res.status} ${res.statusText}（网络受限可设 MIHOMO_MIRROR_URL 换源）`)
    }
    const zipPath = join(tmp, 'mihomo.zip')
    await writeFile(zipPath, Buffer.from(await res.arrayBuffer()))
    const zipHash = sha256File(zipPath)
    if (zipHash !== ZIP_SHA256) {
      throw new Error(`zip 校验失败：期望 ${ZIP_SHA256}，实际 ${zipHash}（换版本/换源需人工同步脚本头部哈希常量）`)
    }
    const exeDir = join(tmp, 'extracted')
    mkdirSync(exeDir)
    console.log('[fetch-sidecar] 解压 zip（powershell Expand-Archive）')
    const ps = spawnSync(
      'powershell',
      ['-NoProfile', '-Command', `Expand-Archive -Path '${zipPath}' -DestinationPath '${exeDir}' -Force`],
      { stdio: 'inherit' },
    )
    if (ps.status !== 0) {
      throw new Error(`解压失败（powershell 退出码 ${ps.status ?? 'null'}）`)
    }
    const inner = join(exeDir, ZIP_INNER_EXE)
    if (!existsSync(inner)) {
      throw new Error(`解压产物缺失：${ZIP_INNER_EXE} 不存在（压缩包内容与预期不符）`)
    }
    const exeHash = sha256File(inner)
    if (exeHash !== EXE_SHA256) {
      throw new Error(`exe 校验失败：期望 ${EXE_SHA256}，实际 ${exeHash}`)
    }
    mkdirSync(dirname(EXE_PATH), { recursive: true })
    copyFileSync(inner, EXE_PATH)
    console.log(`[fetch-sidecar] 完成：${EXE_PATH}`)
  } catch (e) {
    console.error(`[fetch-sidecar] 失败：${e instanceof Error ? e.message : String(e)}`)
    process.exitCode = 1
  } finally {
    rmSync(tmp, { recursive: true, force: true })
  }
}

// 两者互不跳过：mihomo 已就绪不代表 geo 也在了（反之亦然）。任一步失败即置
// 非零退出码，但**另一项仍会尝试**——能补一样是一样。
const run = async () => {
  await fetchMihomo()
  try {
    await fetchGeo()
  } catch (e) {
    console.error(`[fetch-sidecar] 失败：${e instanceof Error ? e.message : String(e)}`)
    process.exitCode = 1
  }
}

void run()
