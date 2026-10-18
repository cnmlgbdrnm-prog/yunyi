// updater 发布配置检查（供 npm run check:updater-pubkey 与 CI 使用）：
// 1. plugins.updater.pubkey 必须是「公钥盒全文（untrusted comment 行 + base64 密钥行）
//    的 base64」——tauri CLI 2.11（helpers/updater_signature.rs decode_key）与
//    tauri-plugin-updater 2.10（updater.rs base64_to_string → PublicKey::decode）
//    的 decode 链都是：base64 解码配置值 → 必须是 UTF-8 文本 → 按 minisign .pub
//    文件格式解析两行。裸密钥行（如 RWSh...）会被二次 base64 解码成二进制，
//    构建时报「failed to decode base64 pubkey: failed to convert base64 to utf8」。
// 2. bundle.createUpdaterArtifacts 必须为 true——false/缺失时 tauri build 不生成
//    安装包 .sig，tauri-action 报「Signature not found for the updater JSON」并
//    跳过 latest.json 上传，客户端检查更新将永远停在旧版本
//    （2026-08-15 0.2.2 首轮构建实际发生）。
// 用法：node scripts/check-updater-pubkey.mjs [配置文件路径]——
// 不传参数检查仓库内 tauri.conf.json；传入路径供测试注入临时夹具。
// 任一断言失败打印中文原因并退出非零。
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..')
const CONF_PATH = process.argv[2] ?? join(ROOT, 'src-tauri', 'tauri.conf.json')

const failures = []
let conf
try {
  conf = JSON.parse(readFileSync(CONF_PATH, 'utf8'))
} catch (e) {
  console.error(`[check-updater-pubkey] 读取 ${CONF_PATH} 失败: ${e.message}`)
  process.exit(1)
}
const pubkey = conf?.plugins?.updater?.pubkey
if (typeof pubkey !== 'string' || pubkey.length === 0) {
  failures.push('tauri.conf.json 缺少 plugins.updater.pubkey')
} else if (!/^[A-Za-z0-9+/]+={0,2}$/.test(pubkey)) {
  // 镜像 Rust 侧 STANDARD.decode 的严格字符集：Buffer.from 是宽容解码，
  // 混入空白/换行的 base64 在 JS 侧能解但在 tauri build 侧报错，必须先行拦截
  failures.push('pubkey 含 base64 字符集之外的字符（疑似换行/空白混入或裸密钥行被二次编码）')
} else {
  const boxText = Buffer.from(pubkey, 'base64').toString('utf8')
  if (Buffer.from(boxText, 'utf8').toString('base64') !== pubkey) {
    // 往返校验镜像 Rust 侧 RequireCanonical：缺失/多余 padding 或尾部多余位
    // 在 Node 宽容解码下能得到相同 box，但 tauri build 侧 STANDARD.decode 会拒绝
    failures.push('pubkey 不是规范 base64（padding 或尾部位不规范）')
  } else if (boxText.includes('�') || !/^[\x20-\x7E\r\n]*$/.test(boxText)) {
    failures.push('pubkey 解码后不是合法 UTF-8 文本（疑似存放了裸 minisign 密钥行而非公钥盒全文的 base64）')
  } else {
    const lines = boxText.split(/\r?\n/).filter((l) => l.length > 0)
    if (lines.length !== 2 || !lines[0].startsWith('untrusted comment: ')) {
      failures.push(`pubkey 解码后应为 minisign .pub 文件两行格式（untrusted comment + 密钥行），实际 ${lines.length} 行`)
    } else {
      const keyBin = Buffer.from(lines[1], 'base64')
      if (keyBin.length !== 42 || keyBin.toString('latin1', 0, 2) !== 'Ed') {
        failures.push(`pubkey 密钥行解码后应为 42 字节且以 'Ed' 开头（minisign Ed25519），实际 ${keyBin.length} 字节`)
      }
    }
  }
}

const artifacts = conf?.bundle?.createUpdaterArtifacts
if (artifacts !== true) {
  failures.push('tauri.conf.json 的 bundle.createUpdaterArtifacts 必须为 true——false/缺失时安装包不生成 .sig，发布会缺 latest.json，客户端检查更新停在旧版')
}

if (failures.length > 0) {
  for (const f of failures) console.error(`[check-updater-pubkey] ${f}`)
  process.exit(1)
}
console.log('[check-updater-pubkey] pubkey 为公钥盒全文 base64（与 CLI/插件 decode 链一致）且 createUpdaterArtifacts 已开启，通过')
