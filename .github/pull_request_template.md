## 变更说明

<!-- 说明改了什么、为什么改；关联 issue 请写 `Closes #123`。 -->

## 检查

- [ ] `npm run check:org`（文档分层/命名/死链、根目录白名单、views 依赖）
- [ ] `npm run check:size`（UI 组件 ≤1000 行、单文件 ≤3000 行）
- [ ] `npx tsc --noEmit -p tsconfig.json`
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] 涉及 UI 时，确认没有重复实现 `src/components/` 已有的公共子组件

## 文档

<!-- 新增/修改的文档是否已放入 docs/<分类>/？没有则写「无」。 -->

## 提交信息

PR 标题会作为 squash 合并的提交信息，请使用 `type(scope): 描述` 格式
（如 `fix(net): 修复 TUN 启动失败`）。规范见
[项目组织规范](../docs/engineering/project-conventions.md)。
