// 提交信息规范：首行使用 Conventional Commits 前缀（type(scope): 描述），
// 让 `git log --oneline` 与发布记录可被机器归类；正文/脚注是自由纯文本，
// 不要求 Markdown，也不强制固定换行——中文长句按自然语言书写即可。
export default {
  extends: ['@commitlint/config-conventional'],
  rules: {
    // 正文与脚注不限制行宽：中文没有空格，强制 100 字符换行反而难读。
    'body-max-line-length': [0, 'always'],
    'footer-max-line-length': [0, 'always'],
    // 不限制描述的大小写风格（如英文 "Add ..." 允许自然句式）。
    'subject-case': [0, 'always'],
    // 首行仍限长，保证单行日志可读。
    'header-max-length': [2, 'always', 100],
  },
}
