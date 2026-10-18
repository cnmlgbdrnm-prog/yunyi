/// <reference types="vitest/config" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { configDefaults } from 'vitest/config'

// 主工作树内可能嵌套别的 git worktree，它们自带 src/node_modules：必须排除，
// 否则测试会卷进其测试文件造成双 React 副本与重复执行，dev 下其文件改动也会触发
// 主树无谓重载。两个位置都要覆盖——Claude Code 的 worktree 落在 .claude/worktrees/，
// 历史上手工建的在 .worktrees/。
const NESTED_WORKTREES = ['**/.worktrees/**', '**/.claude/worktrees/**']

// vite 构建配置：React 插件 + Tailwind 4 + Vitest 测试环境
export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    // 忽略 src-tauri 目录的监听，避免 Windows 上锁定中的 target DLL 触发 EBUSY；
    // 同时忽略嵌套 worktree，避免其文件触发主树 dev 重载
    watch: {
      ignored: ['**/src-tauri/**', ...NESTED_WORKTREES],
    },
  },
  test: {
    environment: 'jsdom',
    // 测试源码统一放在 tests/frontend（不随 git 上传，见 .gitignore）
    setupFiles: ['./tests/frontend/test-setup.ts'],
    include: ['tests/frontend/**/*.{test,spec}.{ts,tsx}'],
    globals: true,
    // 新克隆/CI 上没有 tests/ 目录时，不让 vitest 因“未找到测试文件”报错
    passWithNoTests: true,
    exclude: [...configDefaults.exclude, ...NESTED_WORKTREES],
  },
})
