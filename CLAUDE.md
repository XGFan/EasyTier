# EasyTier Fork(XGFan)维护指南

这是 [EasyTier](https://github.com/EasyTier/EasyTier) 的个人 fork。本文件分两部分:EasyTier 项目本体速览,以及本 fork 相对上游的改动、分支/remote 拓扑、如何持续 follow 上游 release 分支。

## EasyTier 本体速览

EasyTier 是 Rust 写的去中心化 mesh VPN(类 Tailscale/ZeroTier,但无中心控制面):节点以 `network_name + network_secret` 组网,P2P 直连优先(UDP/TCP 打洞),失败则经其他节点中继;支持子网代理、exit node、WireGuard 接入口、Magic DNS、SOCKS5 等。

### Workspace 布局

| 目录 | 内容 |
|---|---|
| `easytier/` | 核心 crate:库 + 两个二进制 `easytier-core`(节点守护进程)、`easytier-cli`(经 RPC portal 管理,默认 `127.0.0.1:15888`) |
| `easytier-web/` | 自托管 Web 控制台:axum + sea-orm/sqlite 后端 + Vue 前端;core 以 `-w <token>` 注册上来(`web_client` 模块) |
| `easytier-gui/` | 官方桌面/移动 GUI:Tauri 2 + Vue 3 + PrimeVue;Android 走 `tauri-plugin-vpnservice/` |
| `easytier-rpc-build/` | 从 `.proto` 生成自研 RPC 框架 stub 的 build 工具(配合 prost) |
| `easytier-contrib/` | ffi / uptime / android-jni 等外围 crate |

### 核心 crate 模块地图(`easytier/src/`)

分层自底向上:
- `tunnel/`:传输层抽象,一条 "tunnel" = 一种承载协议(tcp/udp/websocket/wireguard/quic/kcp/faketcp/ring 内存隧道),对应同名 feature 开关。
- `peers/`:节点层——`peer_manager`(转发核心)、`peer_ospf_route`(OSPF 式路由协商)、加密、`foreign_network_*`(为其他网络当中继)。
- `connector/`:主动建连——`direct`(交换 IP 后直连)、`udp_hole_punch/`/`tcp_hole_punch`、`manual`(`--peers` 指定)。
- `gateway/`:流量出口——基于 smoltcp 的用户态 TCP/UDP/ICMP 代理(子网代理/exit node 的实现)、socks5、kcp/quic 代理加速。
- `instance/`:把上面组装成一个运行实例——`virtual_nic`(TUN)、`listeners`、`dns_server`(Magic DNS);`launcher.rs`/`instance_manager.rs` 是 GUI/FFI 复用的启动封装。
- `proto/`:protobuf 定义(`.proto` 与生成的 `.rs` 同目录提交)+ `rpc_impl/` 自研 RPC;`rpc_service/` 是 cli/web 用的管理 RPC 实现。
- `common/`:`config.rs`(TOML 配置,默认监听 `tcp+udp 11010`)、`global_ctx`(实例全局上下文)、`stun`、`ifcfg/`(各平台网卡/路由配置)。
- `arch/`:平台特定代码(本 fork 的 `macos.rs` 在这)。

### 构建与测试

- 工具链:Rust 1.95(edition 2024,见 `rust-toolchain.toml`)+ **protoc**(build.rs 编译 proto 必需)+ pnpm(前端,`pnpm -r install/build`)。
- 常用:`cargo build`(default-members 只含 easytier + easytier-web);GUI 用 `cd easytier-gui && pnpm tauri build`。
- feature:默认已含 wireguard/quic/kcp/smoltcp/tun 等;`full` 额外加 openssl/aes-gcm。
- 测试:`cargo test` 的集成测试(`three_node` 等)依赖 Linux netns,**macOS 上会被 cfg 掉,只能跑普通单测**;CI 全量测试用 `--no-default-features --features=full`。
- 上游贡献流程:PR 打到 `develop` 分支,conventional commits(见 `CONTRIBUTING.md`)。

## 这个 fork 携带什么

只有一组本地改动:**macOS 全隧道修复**(3 个 `fix(macos)` 提交,面向上游),修复 macOS 上 `--manual-routes 0.0.0.0/0 --exit-nodes` 全隧道场景的路由环路 / 打洞失败问题。核心:

- underlay socket 绑定到物理默认路由接口(避免流量被自家 /1 路由吸进 TUN 造成环路);
- 用 mihomo 同款拆分默认路由(1/8..128/1 网关型,绕开 macOS 内核对 `0.0.0.0` 目标路由打 `RTF_GLOBAL` 导致 scoped socket sendto 失败的坑);
- STUN 探测 socket 一并绑 underlay,避免全隧道下把出口节点公网 IP 当自己的公告出去。

触及文件(**rebase 时的主要冲突点**):`easytier/src/arch/macos.rs`(新增)、`arch/mod.rs`、`common/global_ctx.rs`、`common/ifcfg/darwin.rs`、`common/stun.rs`、`connector/{direct,mod}.rs`、`connector/udp_hole_punch/*`、`instance/virtual_nic.rs`、`tunnel/common.rs`。

**这组是上游的真 bug fix,应尽快提 PR。合并后本 fork 与上游的差异归零,follow 上游零成本。**

## 与 easytier-mac 独立仓库的关系

macOS 原生托盘 GUI + 特权 supervisor 曾以 `easytier-mac/` 目录存在于本 fork,2026-07 已拆分为独立仓库(本地 `~/Developer/Joy/easytier-mac`),并以 **git submodule 形式 pin 本 fork 的 `releases/v2.6.4` 分支**(easytier 库 + easytier-core 二进制的唯一来源)。

因此每次 rebase + force-push 本 fork 之后:
1. 去 easytier-mac 仓库 bump submodule(`cd vendor/EasyTier && git fetch origin && git checkout origin/releases/v2.6.4`),重建验证后提交 gitlink;
2. **在本 fork 给新 pin 的 commit 打 tag 并 push**(如 `mac-base-YYYYMMDD`)——force-push 会让旧 commit 在远端不可达,不打 tag 的话 GitHub GC 后 submodule 会拉不到对象。

## Remote / 分支拓扑

| remote | 指向 | 用途 |
|---|---|---|
| `origin` | `git@github.com:XGFan/EasyTier.git` | 你的 fork,`push`/`pull` 走这里 |
| `upstream` | `git@github.com:EasyTier/EasyTier.git` | 上游,只读,follow 用 |

- 工作分支:`releases/v2.6.4`(= 上游 release + 3 个 macOS 修复 + 本文件),tracking `origin/releases/v2.6.4`。
- 原则:**永不在本地直接修改上游代码的"镜像"**;本地补丁始终作为上游之上的一串干净提交,用 **rebase**(不是 merge)维护,这样补丁保持可审阅、可直接投递成 PR。
- `rerere` 已开启(`git config rerere.enabled true`):它会记住每次冲突的解决方式,后续 rebase 到新上游时自动重放,那几处 macOS 文件的冲突基本一劳永逸。

## 如何 follow 上游 release 分支

```bash
git fetch upstream
git rebase upstream/releases/v2.6.4        # 把本地补丁重放到上游最新之上
# 若有冲突:解决(rerere 会帮你重放旧解法)→ git add → git rebase --continue
git push --force-with-lease origin releases/v2.6.4
# 然后按「与 easytier-mac 独立仓库的关系」一节 bump submodule + 打 tag
```

要点:
- 本 fork 不再改动任何 workspace manifest / lockfile,rebase 冲突只会出现在上面列出的 macOS 修复文件里。
- **核对 rebase 没改坏补丁**:`git range-diff upstream/releases/v2.6.4...HEAD`(rebase 前后对比补丁系列)。
- **上游开了新 release 分支**(如 `releases/v2.6.5`)时,把 rebase 目标换成新分支即可平移过去(easytier-mac 侧的 submodule branch 记录也同步改)。

## 提交组织

- macOS 修复:一个 fix 一个 commit,保持可直接投递 PR。
- 本文件(CLAUDE.md)单独一个 commit,始终 rebase 在补丁串顶端。

## 长期策略

优先把 macOS 修复提交给上游(按 `CONTRIBUTING.md` 打到 `develop` 分支)。合并后本 fork 与上游差异归零;easytier-mac 仓库届时可把 submodule 直接指向上游。
