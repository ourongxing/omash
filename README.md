```text
 ██████╗ ███╗   ███╗ █████╗ ███████╗██╗  ██╗
██╔═══██╗████╗ ████║██╔══██╗██╔════╝██║  ██║
██║   ██║██╔████╔██║███████║███████╗███████║
██║   ██║██║╚██╔╝██║██╔══██║╚════██║██╔══██║
╚██████╔╝██║ ╚═╝ ██║██║  ██║███████║██║  ██║
 ╚═════╝ ╚═╝     ╚═╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝
```

`omash` 是面向 Mihomo 的纯 Rust 终端控制台，以 Ratatui 重现 Clash Verge Rev 的核心管理能力和工作流，不依赖 Node.js、WebView、TypeScript 或 JavaScript 运行时。

Mihomo 默认由用户级 `omash-supervisor.service` 持久管理。用户无需配置 API 地址、Secret 或手工启动内核，关闭 TUI 也不会中断代理。

后端行为直接以工作区中的 Clash Verge Rev Rust 实现为基准，不自行猜测 Mihomo 协议。当前对照基线和源码映射见 [`docs/UPSTREAM.md`](docs/UPSTREAM.md)。

## 界面预览

<p align="center">
  <img src="screenshots/1.jpg" alt="omash 蓝色主题界面" width="49%">
  <img src="screenshots/2.jpg" alt="omash 橙色主题界面" width="49%">
</p>

> [!TIP]
> **omash 会自动跟随 Omarchy 主题。** 切换 Omarchy 配色后，运行中的 TUI 会实时同步，无需重启或手动配置。

## 当前可用

- Mihomo sidecar 启动、停止、API readiness 探测、热重载和配置预检
- 用户级后台 supervisor、崩溃拉起、配置变化自动重启和随机 API 凭据
- Dashboard、代理、配置文件、连接、规则、日志、解锁与设置页面
- 代理组按 Mihomo 配置声明顺序浏览、节点切换和节点延迟测试
- 代理提供者和规则提供者手动更新
- 活跃连接查看、关闭单条或全部连接
- Rule / Global / Direct 模式切换
- 远程订阅和本地 YAML 导入、远程更新、切换及删除；导入、更新和切换均先验证再提交
- 订阅流量信息读取
- 纯 Rust 配置增强：深度 Merge、prepend/append、Rules、Proxies、Groups
- TUN、Allow LAN、IPv6 和刷新间隔设置
- Mihomo Release 内核一键更新；yay 安装版仅作首次引导，自动复制为应用拥有的 sidecar，运行时不使用或修改 pacman 文件
- GeoData 一键更新；优先使用 Mihomo API，网络失败时直连官方发布资源并原子替换
- Linux `gsettings` 与 Omarchy/UWSM systemd 用户环境代理后端；代理启用期间新应用使用 UWSM service，确保 Chrome 继承实时代理
- 独立 TUI 主题文件；未配置时自动持续同步 Omarchy 配色
- 可选 Omarchy QML 状态栏组件，支持切换代理模式和 Selector 节点
- Omarchy 图形登录时通过 systemd 用户服务自动启动；可在设置页启用或关闭
- Mihomo 日志查看
- 与 Clash Verge Rev 共用的纯 Rust 流媒体/AI 解锁检测引擎
- 本地 ZIP 备份及带确认的恢复
- 按配置周期自动更新远程订阅，并在当前配置变化后重启内核
- TOML、环境变量和 CLI 参数配置

## 界面与操作

宽终端使用侧栏布局，较窄的终端会自动切换为顶部导航。底栏只显示当前页面可用的操作，按 `?` 可随时打开完整快捷键说明。

| 按键 | 操作 |
| --- | --- |
| `1`–`9` | 直接打开对应页面 |
| `↑` / `↓`、`j` / `k` | 移动当前选择 |
| `Tab`、`←` / `→`、`h` / `l` | 在代理组和节点面板间切换 |
| `Enter` | 执行当前选择 |
| `r` | 立即刷新 |
| `?` | 打开或关闭快捷键说明 |
| `q`、`Ctrl-C` | 退出 TUI，不停止后台代理 |

列表支持鼠标点击和滚轮移动，代理节点、配置文件及设置项支持双击执行。TUI 的 Proxy Groups 保持 `runtime.yaml` 中 `proxy-groups` 的声明顺序，不按名称重新排序；每个组中的节点保持 Mihomo 返回的原始顺序。

## 对等移植进度

仍在移植的 Clash Verge Rev 能力：

- WebDAV 备份同步
- DNS、端口、网络接口等高级编辑器
- 特权服务管理

Clash Verge 的 `.js` Script 增强器不会移植；这是“不使用 JS”的明确边界。相同用途由纯 Rust YAML 增强链承担。

## 安装与运行

系统需要可用的 Mihomo。Arch/Omarchy 可以安装：

```bash
yay -S mihomo
```

本地构建并安装：

```bash
cargo build --release
sudo install -Dm755 target/release/omash /usr/bin/omash
sudo install -Dm644 systemd/omash-supervisor.service \
  /usr/lib/systemd/user/omash-supervisor.service
omash
```

发行包也需要把这两个文件分别安装到 `/usr/bin/omash` 和
`/usr/lib/systemd/user/omash-supervisor.service`。首次运行 `omash` 时会根据
`auto_start` 启用或禁用该用户服务，并立即启动当前会话的 supervisor。关闭
`Start on login` 只取消下次图形登录时的自动启动，不会中断当前代理。

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

配置页面中 `b` 创建备份，`R` 恢复最近备份；恢复前必须确认。选择 `Update Mihomo core` 或 `Update GeoData` 后按 `Enter` 即可更新，也可以使用鼠标双击执行。

`auto_start` 对应设置页的 `Start on login`。启用后，
`omash-supervisor.service` 随 Omarchy 的 `graphical-session.target` 启动；它是
登录级自启动，不会通过 systemd lingering 在无人登录时运行。

## 主题

在 Omarchy 中，omash **默认自动跟随当前系统主题**。无论 TUI 是否已在运行，切换 Omarchy 配色后都会持续、实时同步，无需重启 omash。

如果需要使用独立配色，omash 也可以读取 `~/.config/omash/theme.toml`。主题支持部分覆盖，未填写的颜色沿用内置默认值。可复制示例开始配置：

```bash
mkdir -p ~/.config/omash
cp themes/default.toml ~/.config/omash/theme.toml
```

独立主题修改后，运行中的 TUI 也会自动重新加载。颜色均使用 `#RRGGBB` 格式，字段含义见 [`themes/default.toml`](themes/default.toml)。创建或安装 `theme.toml` 后，omash 会停止跟随 Omarchy，改用该独立主题；删除它即可恢复自动跟随。

## Omarchy 状态栏

仓库附带一个原生 Quickshell 状态栏组件。点击 omash 图标可以切换 Rule、Global、Direct 模式；Proxy Groups 面板采用紧凑双栏布局，左栏显示代理组及当前选择，右栏显示所选组的节点和已测延迟，并可独立滚动。点击标题右侧的延迟测试按钮会测试当前组的全部节点。

左栏只显示 Selector 类型的代理组，并保持 `runtime.yaml` 中 `proxy-groups` 的声明顺序；右栏节点保持 Mihomo 返回的原始顺序。安装后 Omarchy Shell 会自动热重载：

```bash
mkdir -p ~/.config/omarchy/plugins
cp -r integrations/omarchy/ourongxing.omash ~/.config/omarchy/plugins/
omarchy plugin enable ourongxing.omash --section right
```

组件通过内部的 `omash bar` 命令调用已有 Rust API 层，因此不会读取或复制 Mihomo API Secret；需要确保 `omash` 已安装在 Omarchy Shell 的 `PATH` 中。组件默认每 5 秒刷新一次，也可在 Omarchy 插件设置中调整为 2–60 秒。

## License

GPL-3.0-only
