// 代码规模硬规范检查（随仓库分发，CI 强制执行）：
//   1. React 组件（src/ 下的 .tsx）不得超过 500 行；
//   2. 单个代码文件不得超过 1500 行。
// 统计口径为物理行数（与 wc -l 一致）。
// 不适用范围：docs/ 下的计划与设计文档、锁文件、生成物——规范只约束代码。
import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'

const COMPONENT_LIMIT = 500
const FILE_LIMIT = 1500

// 计入 1500 行上限的代码扩展名。
const CODE_EXTS = new Set(['.ts', '.tsx', '.mts', '.cts', '.js', '.jsx', '.mjs', '.cjs', '.rs', '.css', '.html'])
// 计划/设计文档、锁文件与生成物不属于代码规范范围。
const EXCLUDED_PATHS = [
  /^docs\//,
  /^src-tauri\/Cargo\.lock$/,
  /^package-lock\.json$/,
  /^src-tauri\/gen\//,
  /^dist\//,
  /^node_modules\//,
]

const extOf = (path) => {
  const i = path.lastIndexOf('.')
  return i === -1 ? '' : path.slice(i).toLowerCase()
}

const isExcluded = (path) => EXCLUDED_PATHS.some((re) => re.test(path))

const countLines = (path) => {
  const text = readFileSync(path, 'utf8')
  let lines = 0
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10) lines++
  return lines
}

const files = execFileSync('git', ['ls-files', '-z'], { encoding: 'utf8' })
  .split('\0')
  .filter(Boolean)

const violations = []
for (const path of files) {
  if (isExcluded(path)) continue
  const ext = extOf(path)
  if (!CODE_EXTS.has(ext)) continue
  const lines = countLines(path)
  // 组件规则：前端 src/ 下的 .tsx（视图与组件都按组件口径约束）。
  if (ext === '.tsx' && path.startsWith('src/') && lines > COMPONENT_LIMIT) {
    violations.push(`${path}: ${lines} 行，超过组件上限 ${COMPONENT_LIMIT} 行`)
  }
  if (lines > FILE_LIMIT) {
    violations.push(`${path}: ${lines} 行，超过单文件上限 ${FILE_LIMIT} 行`)
  }
}

if (violations.length > 0) {
  console.error('[check-code-size] 未通过：')
  for (const v of violations) console.error(`  - ${v}`)
  console.error(
    '\n规范：组件 ≤500 行，单文件 ≤1500 行；公共子组件必须复用，不得复制重写。',
  )
  process.exit(1)
}

console.log(
  `[check-code-size] 通过：${files.length} 个已跟踪文件；组件 ≤${COMPONENT_LIMIT} 行、单文件 ≤${FILE_LIMIT} 行。`,
)
