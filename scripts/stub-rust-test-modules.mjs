#!/usr/bin/env node
// CI 专用：为 #[path] 指向仓库根 tests/ 的模块补一个空占位文件。
//
// 测试源码集中放在 tests/ 且不入库（见 .gitignore 的 /tests/），CI 检出里没有
// 这些文件；rustfmt 不认 cfg 门控，解析 mod 时找不到文件会直接失败。这里只创建
// 缺失的空模块，让 cargo fmt 能继续检查真实源码。
//
// 只允许在仓库根 tests/ 下写入；已存在的文件绝不覆盖。本地 tests/ 齐全时是空操作。

import { readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { dirname, join, relative, resolve, sep } from 'node:path'

const ROOT = execFileSync('git', ['rev-parse', '--show-toplevel'], {
  encoding: 'utf8',
}).trim()
const PATH_ATTR_RE = /#\[path\s*=\s*"([^"]+)"\s*\]/g

const files = execFileSync('git', ['ls-files', '-z', 'src-tauri/src'], {
  cwd: ROOT,
  encoding: 'utf8',
})
  .split('\0')
  .filter((p) => p.endsWith('.rs'))

const created = []
for (const rel of files) {
  const source = join(ROOT, rel)
  const text = readFileSync(source, 'utf8')
  for (const match of text.matchAll(PATH_ATTR_RE)) {
    const target = resolve(dirname(source), match[1])
    const relTarget = relative(ROOT, target)
    if (!relTarget.startsWith(`tests${sep}`)) continue
    if (existsSync(target)) continue
    mkdirSync(dirname(target), { recursive: true })
    writeFileSync(target, '\n')
    created.push(relTarget)
  }
}

if (created.length) {
  console.log(`[stub-rust-tests] 已补 ${created.length} 个占位模块：${created.join('、')}`)
} else {
  console.log('[stub-rust-tests] 无需补占位模块')
}
