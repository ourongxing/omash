# Clash Verge Rev 功能对照

`omash` 的目标是业务能力对等，而不是复制桌面专属表现。Tauri command、托盘菜单和 React 页面会映射为 Rust service 与 TUI action。

| 能力域 | 状态 | omash 实现 |
| --- | --- | --- |
| 首页与流量 | 已实现 | Dashboard、API 快照、吞吐与连接计数 |
| 代理组 | 已实现 | 浏览、选择、延迟测试 |
| 配置文件 | 基础已实现 | URL/文件导入、事务式更新、自动更新、切换、删除、订阅用量；高级请求选项待移植 |
| 配置增强 | 已实现（无 JS 变体） | Rust YAML deep merge、prepend/append、rules/proxies/groups |
| 内核管理 | 基础已实现 | 与 Verge 相同的应用拥有型 sidecar 生命周期；用户级 supervisor、staging 校验、API readiness、热重载、崩溃退避拉起、日志；不支持外部 API 模式 |
| 连接 | 已实现 | 浏览、单条关闭、全部关闭 |
| 规则 | 已实现 | 规则列表 |
| 模式 | 已实现 | Rule、Global、Direct |
| 系统代理 | Linux 已实现 | gsettings；Omarchy/Hyprland 下同步 systemd 用户环境并动态切换 UWSM service/scope，新启动的 Chrome 等桌面应用自动继承 |
| TUN | 部分实现 | 可生成配置；尚未移植 Clash Verge Rev 特权服务，普通用户 sidecar 不保证可启用 |
| 流媒体解锁 | 已实现 | 复用 Clash Verge Rev Rust 检测 crate |
| 本地备份 | 已实现 | ZIP 创建、列出、确认恢复 |
| WebDAV | 待移植 | — |
| DNS/端口高级编辑 | 待移植 | — |
| Provider 管理 | 基础已实现 | 批量更新 proxy/rule providers；详情页待移植 |
| 系统自启动 | 待移植 | — |
| 特权服务 | 待移植 | — |
| 内核与 GeoData 更新 | 已实现 | Mihomo release channel API 更新、用户托管核心、直连回退、GeoData 原子替换与 supervisor 重启 |

## 无 JavaScript 边界

运行时不包含 Node.js、WebView 或 JavaScript 引擎。Clash Verge Rev 的 `.js` Script profile 不会执行；该能力由声明式 Rust YAML 增强链替代。这是唯一有意不做逐实现兼容的功能。
