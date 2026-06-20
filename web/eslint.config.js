// eslint.config.js — ESLint flat config（「npm 可用时」的完整前端门）
//
// ⚠️ 本配置需要 `npm install`（拉取 eslint + eslint-plugin-import）。
//    沙箱/CI 可能无网，因此它**不是主门**、不应阻断流水线。
//    主门是 scripts/web-check.sh（仅依赖 node，零 npm 依赖），
//    专抓「语法错误」「import 指向不存在的文件」这两类破损。
//    本配置是它的超集：额外覆盖 no-undef（调用未定义函数/变量）、
//    import/no-unresolved（更严格的 import 解析）、max-lines（文件尺寸警告）。
//
// 用法（需联网环境）：
//    cd web && npm install && npm run lint

import js from '@eslint/js';
import importPlugin from 'eslint-plugin-import';
import globals from 'globals';

export default [
  js.configs.recommended,
  {
    files: ['**/*.js'],
    plugins: {
      import: importPlugin,
    },
    languageOptions: {
      ecmaVersion: 'latest',
      sourceType: 'module',
      globals: {
        ...globals.browser,
        // 浏览器侧偶有用到的全局（CDN 注入 / 平台 API）
        Hls: 'readonly',
      },
    },
    settings: {
      'import/resolver': {
        node: {
          extensions: ['.js', '.mjs'],
        },
      },
    },
    rules: {
      // 调用未定义函数 / import 不存在的符号 —— 当初拆坏 app.js 的那一类
      'no-undef': 'error',
      // import 目标文件必须在磁盘上解析得到 —— 抓 ../state.js 不存在那一类
      'import/no-unresolved': 'error',
      // 文件尺寸软上限（与 file-size-check.sh 的 JS 阈值对齐，仅 warn 不阻断）
      'max-lines': ['warn', 1000],
    },
  },
];
