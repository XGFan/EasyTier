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
- 上游贡献流程:PR 打到 `main`(`CONTRIBUTING.md` 里写的 `develop` 已不存在),conventional commits。
- 上游分支模型:`main` 是唯一主线;`releases/vX.Y.Z` 是发版时从 `main` 切出的快照,切出后不再动(tip = 同名 tag,无 backport)。

## 这个 fork 携带什么

只有一组本地改动:**macOS 全隧道支持**,修复 macOS 上 `--manual-routes 0.0.0.0/0 --exit-nodes` 全隧道场景的路由环路 / 打洞失败问题。核心:

- underlay socket 绑定到物理默认路由接口(避免流量被自家 /1 路由吸进 TUN 造成环路);
- 用 mihomo 同款拆分默认路由(1/8..128/1 网关型,绕开 macOS 内核对 `0.0.0.0` 目标路由打 `RTF_GLOBAL` 导致 scoped socket sendto 失败的坑);
- STUN 探测 socket 一并绑 underlay,避免全隧道下把出口节点公网 IP 当自己的公告出去。

代码分两部分:

| 位置 | 内容 | rebase 风险 |
|---|---|---|
| `easytier/src/arch/`(全部新增)+ `easytier/tests/underlay_udp_bind.rs` | macOS 路由子系统(默认路由探测、split route、bypass /32)、平台 `ProxyRouteHook` seam、不变量守卫 | **无**,上游没有同名文件 |
| `fork/`(全部新增) | follow 流程脚本、实机 gate(`mini.sh` / `et-test.sh` / `verify-fulltunnel.sh`)、SOP | **无** |
| `tunnel/common.rs`、`common/{stun,global_ctx}.rs`、`common/ifcfg/darwin.rs`、`connector/{mod,direct}.rs`、`connector/udp_hole_punch/*`、`instance/virtual_nic.rs`、`arch/mod.rs` | 上游文件里的接入点:socket 绑定穿参 + 路由 hook 调度 | 冲突只会出现在这里 |

**上游已明确拒绝合入这个 feature**([PR #2432](https://github.com/EasyTier/EasyTier/pull/2432#issuecomment-5231339546),理由"为防止滥用"),所以补丁会长期存在。follow 上游的完整流程、冲突处理时要保住的语义、已知既有失败测试清单、实机验证 gate,全部见 **[`fork/SOP.md`](fork/SOP.md)**。

## 测试机

开发机和出口节点在同一个 LAN,而那个 LAN 正是出口节点暴露的子网,所以从开发机永远测不出
全隧道是否真的生效。实机 gate 跑在一台专用测试机上:独立上联(USB CPE)、Wi-Fi 常关、与开发机
一根对连网线承载 SSH。测试机上没有 Rust 工具链,只收二进制:`fork/mini.sh` 负责 build → rsync →
重启 core → 验证,测试机侧的进程管理是 `fork/et-test.sh`。参数在 `fork/mini.env`,拓扑、地址、
排障与基线在 `fork/local/TESTBOX.md`——两者都是本机相关内容,已 gitignore,不入库。

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

- 工作分支:`releases/v2.6.4`(= 上游 release + 6 个 commit:3 个 macOS 修复、不变量守卫、fork 工具与 SOP、本文件与 CONTEXT.md),tracking `origin/releases/v2.6.4`。
- `main`(origin 和本地)是上游 `main` 的纯镜像,只 fast-forward、不提交;**每次同步上游都一并同步**(命令见 `fork/SOP.md` 第 1 步)。
- 原则:**永不在本地直接修改上游代码的"镜像"**;本地补丁始终作为上游之上的一串干净提交,用 **rebase**(不是 merge)维护,这样补丁保持可审阅、可直接投递成 PR。
- `rerere` 已开启(`git config rerere.enabled true`):它会记住每次冲突的解决方式,后续 rebase 到新上游时自动重放,那几处 macOS 文件的冲突基本一劳永逸。

## 如何 follow 上游 release 分支

```bash
fork/follow-upstream.sh v2.7.0       # fetch + rebase 到上游 tag + range-diff + 全部静态检查
fork/mini.sh gate                    # 在测试机上实机验证,硬 gate
# 通过后才 tag / force-push / bump easytier-mac submodule
```

**完整流程见 [`fork/SOP.md`](fork/SOP.md)**,包括每步的通过标准、冲突时每个文件要保住什么语义、
已知的既有失败测试清单、以及回滚方式。要点:

- 本 fork 不改动任何 workspace manifest / lockfile。
- 真正的风险不是 rebase 冲突(会报错),而是**合入成功但功能静默失效**——补丁带了两个不变量守卫专门抓这个。
- **上游发了新版本**(如 `v2.7.0`)时,新开同名分支 `releases/v2.7.0`、补丁 rebase 到新 tag 上;旧分支原样保留(easytier-mac 侧的 submodule branch 记录同步改)。
- 两次发版之间,隔一段时间把补丁在 `upstream/main` 上预演 rebase(本地 `canary` 分支,不发布),冲突提前解、`rerere` 记下。
- 新版本靠手动发现(`git fetch upstream --tags`),不做自动化。

## 提交组织

- **每个 commit 都是一个完整实现单元**:一个 fix、一组守卫测试、一套工具各自独立成 commit;不允许 rustfmt / refactor / 补漏这类碎片 commit 夹在中间才凑成某个功能。做完一轮改动若产生了修补型 commit,并回对应的单元再 push。
- 顺序固定:3 个 macOS 修复 → 不变量守卫 → fork 工具与 SOP → 文档(CLAUDE.md、CONTEXT.md)置顶。
- 整理历史的做法:从上游基点出发按最终代码状态逐 commit checkout 文件集、每步 `cargo check`,末态 `git diff` 旧 HEAD 必须为空。

## 长期策略

上游拒绝合入,所以这是一个长期存在的补丁,目标不是"归零"而是"低摩擦"。三条路径按可能性排序:

1. 维持现状——补丁的主体在 `arch/` 下的自有文件里,上游文件只剩接入点,follow 成本已经压到很低。
2. 上游自己实现了 macOS 全隧道 → 补丁整体删掉,easytier-mac 的 submodule 直接指向上游。
3. 上游重构掉补丁依赖的机制(socket 创建路径、路由下发路径)→ 按 `fork/SOP.md` 的语义表重新接入。
