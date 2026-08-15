# 上游实现基线

omash 的业务逻辑以本地 `../clash-verge-rev` 为直接参考。当前审计基线：

- Clash Verge Rev：`a5aea75f3779bb1a24041fa869568e6f81ac58e8`
- Clash Verge Rev 锁定的 `tauri-plugin-mihomo`：`d9398bf8e862c0cd613b79ada45cc5d893820ed6`

## Rust 源码映射

| omash 能力 | Clash Verge Rev 基准源码 | 已采用的关键约束 |
| --- | --- | --- |
| Core 生命周期 | `src-tauri/src/core/manager/state.rs`、`lifecycle.rs` | 沿用 supervisor 生命周期和 30 次 `/version` readiness 探测；omash 有意改用 Arch 的 `/usr/bin/mihomo`，ready 后才应用系统代理，停止前清理代理 |
| 配置应用 | `src-tauri/src/core/manager/config.rs`、`validate.rs` | staging → mihomo 校验 → 原子提交；优先 API reload，失败才重启；无效配置保留当前核心 |
| Runtime 资源 | `src-tauri/src/core/runtime_bundle.rs` | 当前配置只复用 Arch `clash-geoip` 提供的 `Country.mmdb`；其他规则数据由 profile 的 MRS rule providers 管理 |
| 节点选择 | `src-tauri/src/config/profiles.rs`、`feat/profile.rs` | profile 持久记录 group → node；启动和 reload 后恢复；启用 `profile.store-selected` |
| Mihomo API | `tauri-plugin-mihomo/src/models.rs`、`tests/models_compat.rs` | `Proxy.now/all` 和 `Connections.connections` 按上游 nullable 模型处理；未知字段保持向前兼容 |
| 订阅 | `src-tauri/src/config/prfitem.rs`、`feat/profile.rs` | 读取订阅响应头、去 BOM、先校验下载内容；更新失败不覆盖旧配置 |
| 系统代理 | `src-tauri/src/core/proxy_control.rs` | 仅在核心 ready 后启用；停机、崩溃或配置切换前 fail-closed 清理 |

Tauri、WebView、托盘和 JavaScript Script 增强器不会进入 omash。TUI 之外的业务能力应先在上述 Rust 源码中找到对应行为与测试，再进行移植；若尚未完成，应在 `PARITY.md` 标为部分实现或待移植，不能宣称对等。
