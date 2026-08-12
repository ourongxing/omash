# omash

`omash` 是 Clash Verge Rev 的纯 Rust TUI 实现。目标是保留 Mihomo 管理能力和工作流，只把 Tauri + React 界面替换为 Ratatui，不包含 Node.js、WebView、TypeScript 或 JavaScript 运行时。默认由用户级 `omash-supervisor.service` 持久管理 Mihomo，用户无需配置 API 地址、Secret 或手工启动内核；关闭 TUI 不会中断代理。

后端行为直接以工作区中的 Clash Verge Rev Rust 实现为基准，不自行猜测 Mihomo 协议。当前对照基线和源码映射见 [`docs/UPSTREAM.md`](docs/UPSTREAM.md)。

## 当前可用

- Mihomo sidecar 启动、停止、API readiness 探测、热重载和配置预检
- 用户级后台 supervisor、崩溃拉起、配置变化自动重启和随机 API 凭据
- Dashboard、代理、配置文件、连接、规则、日志、解锁与设置页面
- 代理组浏览、节点切换和节点延迟测试
- 活跃连接查看、关闭单条或全部连接
- Rule / Global / Direct 模式切换
- 远程订阅和本地 YAML 导入、远程更新、切换及删除；导入、更新和切换均先验证再提交
- 订阅流量信息读取
- 纯 Rust 配置增强：深度 Merge、prepend/append、Rules、Proxies、Groups
- TUN、Allow LAN、IPv6 和刷新间隔设置
- Mihomo Release 内核一键更新；yay 安装版仅作首次引导，自动复制为应用拥有的 sidecar，运行时不使用或修改 pacman 文件
- GeoData 一键更新；优先使用 Mihomo API，网络失败时直连官方发布资源并原子替换
- Linux `gsettings` 与 Omarchy/UWSM systemd 用户环境代理后端；代理启用期间新应用使用 UWSM service，确保 Chrome 继承实时代理
- Mihomo 日志查看
- 与 Clash Verge Rev 共用的纯 Rust 流媒体/AI 解锁检测引擎
- 本地 ZIP 备份及带确认的恢复
- 按配置周期自动更新远程订阅，并在当前配置变化后重启内核
- TOML、环境变量和 CLI 参数配置

## 对等移植进度

仍在移植的 Clash Verge Rev 能力：

- WebDAV 备份同步
- 代理提供者和规则提供者的手动更新
- DNS、端口、网络接口等高级编辑器
- 系统自启动和特权服务管理

Clash Verge 的 `.js` Script 增强器不会移植；这是“不使用 JS”的明确边界。相同用途由纯 Rust YAML 增强链承担。

## 安装与运行

系统需要可用的 Mihomo。Arch/Omarchy 可以安装：

```bash
yay -S mihomo
```

构建并运行：

```bash
cargo build --release
./target/release/omash
```

## 配置

配置文件为 `~/.config/omash/config.toml`：

```toml
controller = "http://127.0.0.1:9090"
secret = ""
refresh_ms = 1500
delay_test_url = "https://www.gstatic.com/generate_204"
auto_start = true
mixed_port = 7897
allow_lan = false
ipv6 = true
tun = false
system_proxy = false
proxy_bypass = "localhost,127.0.0.1,::1"
```

运行数据位于 `~/.local/share/omash/`。Mihomo 固定安装在 `~/.local/share/omash/bin/mihomo` 并由 omash 监管，不支持连接外部内核。`OMASH_REFRESH_MS` 和对应命令行参数优先于配置文件。

按 `?` 查看快捷键。配置页面中 `b` 创建备份，`R` 恢复最近备份；恢复前必须确认。
配置页面选择 `Update Mihomo core` 或 `Update GeoData` 后按 `Enter` 即可更新；鼠标可双击执行。

## License

MPL-2.0
