// 项目组织规范检查（随仓库分发，CI 强制执行）：
//   1. 文档必须分层：docs/<分类>/…；根目录只允许 README.md / CHANGELOG.md，
//      docs/ 根目录不允许裸放文件；
//   2. docs 下文档名使用 ASCII 小写 kebab-case；
//   3. src/ 顶层只允许入口文件，其余按职责进子目录；
//   4. src/views 下的页面不得互相引用，共享 UI 必须放 src/components。
// 规范全文见 docs/engineering/project-conventions.md。
import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { posix } from 'node:path'

// 纳入检查的文档扩展名；HTML 仅在 docs/ 下按文档约束（根目录 index.html 是应用入口）。
const DOC_EXTS = new Set(['.md', '.rst', '.adoc'])
// 根目录仅允许这两份仓库级文档，其余文档一律进 docs/<分类>/。
const ROOT_DOC_ALLOW = new Set(['README.md', 'CHANGELOG.md'])
// docs/ 下的分类白名单。新增分类需要同时改本文件与规范文档。
const DOC_CATEGORIES = new Set([
  'product',
  'engineering',
  'design',
  'guides',
  'audits',
  'superpowers',
  'archive',
])
// src/ 顶层只允许入口文件；页面、组件、工具分别进 views/components/utils。
const SRC_ROOT_ALLOW = new Set(['src/App.tsx', 'src/main.tsx', 'src/styles.css', 'src/vite-env.d.ts'])
const DOC_NAME_RE = /^[a-z0-9][a-z0-9._-]*$/
const IMPORT_RE = /(?:from\s+|import\s*\(\s*|import\s+)['"]([^'"]+)['"]/g

const extOf = (path) => {
  const i = path.lastIndexOf('.')
  return i === -1 ? '' : path.slice(i).toLowerCase()
}

const files = execFileSync('git', ['ls-files', '-z'], { encoding: 'utf8' })
  .split('\0')
  .filter(Boolean)
const tracked = new Set(files)
const violations = []

// ---------- 1/2. 文档落位与命名 ----------
for (const path of files) {
  // .github/ 下是 GitHub 规定的文档位置（Issue/PR 模板），不受 docs 分层约束。
  if (path.startsWith('.github/')) continue
  const ext = extOf(path)
  const isDoc = DOC_EXTS.has(ext) || (ext === '.html' && path.startsWith('docs/'))
  if (!isDoc || ROOT_DOC_ALLOW.has(path)) continue

  const match = /^docs\/([^/]+)\/(.+)$/.exec(path)
  if (!match) {
    violations.push(`${path}: 文档必须放在 docs/<分类>/ 下，禁止裸放`)
    continue
  }
  const [, category, rest] = match
  if (!DOC_CATEGORIES.has(category)) {
    violations.push(
      `${path}: 未登记的分类 docs/${category}/（可用分类：${[...DOC_CATEGORIES].join('、')}）`,
    )
  }
  // 文件名（去扩展名）必须是小写 kebab-case：避免空格/中文名进入 URL 与检索。
  const base = posix.basename(rest)
  const stem = base.slice(0, base.length - extOf(base).length)
  if (!DOC_NAME_RE.test(stem)) {
    violations.push(`${path}: 文档名须为小写 kebab-case（字母/数字/点/下划线/连字符）`)
  }
}

// ---------- 3. src/ 顶层只允许入口文件 ----------
for (const path of files) {
  if (!path.startsWith('src/')) continue
  const rel = path.slice('src/'.length)
  if (!rel.includes('/') && !SRC_ROOT_ALLOW.has(path)) {
    violations.push(
      `${path}: src/ 顶层只允许入口文件（${[...SRC_ROOT_ALLOW].join(' / ')}），其余文件按职责进子目录`,
    )
  }
}

// ---------- 4. views 不得互相引用 ----------
// 把相对导入解析为仓库内路径；未命中的候选返回规范化路径即可。
const resolveImport = (importer, spec) => {
  if (!spec.startsWith('.')) return null
  const target = posix.normalize(posix.join(posix.dirname(importer), spec))
  if (target.startsWith('../') || target === '..') return null
  const candidates = [target, `${target}.tsx`, `${target}.ts`, `${target}.jsx`, `${target}.js`]
  return candidates.find((c) => tracked.has(c)) ?? target
}

for (const path of files) {
  if (!/^src\/views\/.+\.(ts|tsx)$/.test(path)) continue
  const text = readFileSync(path, 'utf8')
  for (const m of text.matchAll(IMPORT_RE)) {
    const target = resolveImport(path, m[1])
    if (!target || target === path || !target.startsWith('src/views/')) continue
    // 仅拦「页面引用页面」：目标是 src/views 顶层 .tsx。
    // views/<页面>/ 下的私有子组件（如 views/nodes/NodeRow.tsx）允许被该页面引用。
    if (posix.dirname(target) === 'src/views' && target.endsWith('.tsx')) {
      violations.push(`${path}: 引用了其它页面 ${target}；共享 UI 请提取到 src/components/`)
    }
  }
}

if (violations.length > 0) {
  console.error('[check-organization] 未通过：')
  for (const v of violations) console.error(`  - ${v}`)
  console.error(
    '\n规范：文档必须进 docs/<分类>/；src/ 顶层只放入口文件；views 不得互相引用，共享 UI 放 src/components。',
  )
  process.exit(1)
}

console.log(
  `[check-organization] 通过：${files.length} 个已跟踪文件；文档分层与命名、src 入口、views 依赖均符合规范。`,
)
