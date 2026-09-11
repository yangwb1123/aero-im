# Aero IM — SolidJS Web Frontend

Aero IM 的 Web 前端已直接切换为 SolidJS + Vite，并使用本地
`~/iris-ui` 的 `@iris-ui-kit/solid` 组件适配器。

## 构建

```bash
cd /home/u1/aero-im/web
pnpm install --frozen-lockfile

# ~/iris-ui 源码或其组件发生变化时执行
pnpm run build:iris

pnpm run build
```

产物位于 `web/dist/`。`pnpm run build` 会先执行 TypeScript 类型检查，再执行
Vite 生产构建。

开发模式：

```bash
cd web
pnpm run dev
```

开发服务器地址为 `http://127.0.0.1:5177`，并将 `/api`、`/ws`、`/hls` 代理到
本地 Rust gateway 的 `3030` 端口。

让 Rust gateway 托管构建产物：

```bash
AERO__SERVER__WEB_DIR=./web/dist cargo run --bin aero-server
```

## 目录

- `index.html` — Vite/SolidJS 页面入口。
- `src/main.tsx` — Solid 根挂载。
- `src/App.tsx` — Iris Provider、会话恢复、登录注册和认证门。
- `src/ChatShell.tsx` — 房间、消息历史、Presence 和 WebSocket 实时消息。
- `src/api.ts` — 类型化 REST API 和本地会话存储。
- `src/style.css` — 使用 Iris CSS tokens 的页面布局样式。
- `vite.config.ts` — Vite、`vite-plugin-solid`、开发代理和 `dist` 配置。

旧版原生 JS 文件暂时保留为迁移参考，但不再被 `index.html` 加载；新的
SolidJS 应用是唯一的前端入口。

## Iris UI 本地依赖

`web/package.json` 通过 pnpm `link:` 依赖相邻目录中的 Iris UI：

```text
../../iris-ui/packages/solid
../../iris-ui/packages/core
../../iris-ui/packages/theme
../../iris-ui/packages/tokens
```

因此当前工作区布局需要满足：

```text
/home/u1/iris-ui
/home/u1/aero-im
```

`pnpm run build:iris` 实际执行 `~/iris-ui` 工作区中 Solid 适配器及其依赖的构建。

## 后端通信

前端使用同源相对路径：

- REST：`/api/*`
- WebSocket：`/ws?token=...`
- HLS：`/hls/*`

生产环境建议由 Rust gateway 或反向代理同时提供静态文件、REST 和 WebSocket，
避免跨源认证和 WebSocket 配置问题。

## 校验

```bash
cd web
pnpm run typecheck
pnpm test
pnpm lint
bash ../scripts/web-check.sh
```
