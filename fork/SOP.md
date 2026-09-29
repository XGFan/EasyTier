# Follow-upstream SOP

这个 fork 只维护一件事:**macOS 全隧道**(`--manual-routes 0.0.0.0/0 --exit-nodes ...`)。
上游以"防止滥用"为由拒绝了这个 feature([PR #2432](https://github.com/EasyTier/EasyTier/pull/2432#issuecomment-5231339546)),
所以补丁会长期存在,follow 上游是常态操作,不是一次性迁移。

本文是那个常态操作的流程。仓库拓扑、remote、补丁内容说明见 [`../CLAUDE.md`](../CLAUDE.md)。

## 上游的分支模型

- `main` 是唯一的开发主线,PR 直接 squash 进 `main`(`CONTRIBUTING.md` 里的 `develop` 已不存在)。
- `releases/vX.Y.Z` 是发版时从 `main` 切出的快照,**切出后不再动**:分支 tip 就是同名 tag,
  没有 backport,每个补丁版本各开一条新分支。
- 所以上游的修复只会出现在 `main`,以及下一个版本的 tag 里。

对应到本 fork:

- **每个上游版本一条 fork 分支**,与上游同名(`releases/v2.6.4`、`releases/v2.7.0`……),
  基点是上游的 tag。换版本时新开分支,旧分支原样保留,不改名、不删。
- **平时在 `upstream/main` 上预演 rebase**(见文末),把冲突摊到平时解决;
  `rerere` 记下的解法在真正换版本时自动重放。
- 新版本靠手动发现:`git fetch upstream --tags` 后看有没有新 tag。

## 核心判断

跟上游最大的风险**不是 rebase 冲突**。冲突会报错、会停下来、会逼你看代码。
真正的风险是**静默失效**:上游重构后补丁依然合入、依然编译、单测依然全绿,
但全隧道下打洞悄悄失败,退化成中继——只表现为"网慢了点"。

所以这套流程的重心是:让静默失效变成响的。三层:

| 层 | 覆盖 | 何时跑 |
|---|---|---|
| 不变量守卫(单测) | 上游新增未绑定的 underlay socket;`IP_BOUND_IF` 被提前 return 绕过 | 每次 rebase 后,`follow-upstream.sh` 自动 |
| 路由逻辑单测 | split route 全或无回滚、bypass 路由所有权 | 同上 |
| 实机冒烟 | 路由表实际形态、socket 实际走向、真实出口 IP、p2p 而非中继 | **每次 rebase 后必跑**,见第 4 步 |

前两层单测盖得到,第三层盖不到——这也是为什么第 4 步是硬 gate。

## 流程

### 1. 准备

```bash
git status                      # 必须干净
git config rerere.enabled true  # 只需一次;会记住冲突解法并在后续 rebase 自动重放
```

**同步 `main`(每次同步上游都做)**:fork 的 `main` 是上游 `main` 的纯镜像,只 fast-forward,
不在上面提交。origin 和本地一起同步:

```bash
git fetch upstream --tags --prune
git push origin upstream/main:refs/heads/main   # origin/main;非 fast-forward 会被拒,说明有人在上面提交过
git fetch . upstream/main:main                  # 本地 main(当前不能正处在 main 上)
```

### 2. Rebase + 静态检查

```bash
fork/follow-upstream.sh v2.7.0           # 目标是上游 tag(换版本见文末)
fork/follow-upstream.sh                  # 不带参数:对准同名上游分支,它不会动,等于只重跑检查
```

脚本会:fetch → rebase → `range-diff` → `fmt --check` → 两种 feature 组合的 build →
不变量守卫 → 全量单测。任何一步失败就停,**不会 push**。

**通过标准**

- `range-diff` 里除了上下文行位移之外没有内容变化。有变化 = 上游动了补丁依赖的代码,逐处看懂再继续。
- 单测只剩第 5 节列出的既有环境失败。出现新的失败先归因,别默认无关。

**冲突了怎么办** — 见第 3 节。

### 3. 冲突处理

冲突**只应该**出现在下面这些文件里。出现在别处,说明上游把补丁依赖的东西挪走了,
先读懂上游那个改动再动手。

解冲突时要保住的是语义,不是字面:

| 文件 | 必须保住的语义 |
|---|---|
| `tunnel/common.rs` | `setup_socket2_ext` 里 macOS 的 `IP_BOUND_IF` 分支必须在 `bind_addr.ip().is_unspecified()` 提前 return **之前**。这是整个修复的地基:underlay socket 全是 wildcard bind,提前 return 会让它们全部不 scoped。另有 `bind_underlay_udp_socket` / `bind_socket2_to_underlay_device` 两个 helper |
| `common/stun.rs` | 所有探测 socket(UDP/TCP)走上述 helper;`bind_device` 由构造参数穿到各 detector。漏一个 → STUN 走隧道 → 把出口节点的公网 IP 当自己的公告出去 → 打洞必败 |
| `common/global_ctx.rs` | `GlobalCtx::new` 把 `flags.bind_device` 同步给 `StunInfoCollector`;mock ctx 强制 `bind_device = false`(loopback 测试不能绑物理网卡) |
| `connector/mod.rs` | 连接器竞速时跳过 utun 拥有的本地 IP |
| `connector/direct.rs`、`connector/udp_hole_punch/*` | 所有临时 UDP socket 走 helper |
| `common/ifcfg/darwin.rs` | `MacIfConfiger` 的 `add_ipv4_route_via_gateway` / `remove_ipv4_route_any` / `query_ipv4_route_exact`,以及 `route -n get` 输出解析 |
| `instance/virtual_nic.rs` | 4 处 `ProxyRouteHook` 调度:`try_add_route` / `try_remove_route` / `reconcile` / `on_peer_conn_added` + `shutdown` |

`arch/` 下的文件(`route_hook.rs`、`macos/*`)和 `easytier/tests/underlay_udp_bind.rs`
全是本 fork 新增的,上游没有同名文件,**永远不会冲突**。macOS 路由逻辑的主体都在那里——
这是当初把它们从 `virtual_nic.rs` 搬出去的原因。

### 4. 实机验证(硬 gate)

单测覆盖不到路由表、socket scoping 和真实出口。这一步不通过,后面一步都不能做。

```bash
fork/mini.sh gate        # build → rsync 到测试机 → 重启 core → 等打洞 → 11 项验证
fork/mini.sh stop        # 测完停掉
```

**gate 跑在专用测试机 mini 上,不跑在开发机上**:出口节点暴露的子网就是开发机所在的 LAN,
从开发机永远分不出流量走的是 LAN 还是隧道。拓扑、日常流程、每项检查的含义、排障、
一次性准备、基线,全部见 `fork/local/TESTBOX.md`(本机相关,不入库)。

**任何一项 FAIL 都不要往下走。**

没有测试机时也可以在开发机本地起全隧道后直接跑 `fork/verify-fulltunnel.sh`
(`fork/verify-fulltunnel.env` 放参数),但那只能当冒烟,不能当 gate。

### 5. 已知的既有失败测试

下列 13 个在本机(macOS、非 root、无 IPv6 出口)**改动前就失败**,与补丁无关,
不必每次重新归因。基线核对于 2026-08-11。

```
common::dns::tests::test_socket_addrs
instance::dns_server::system_config::darwin::tests::set_dns_test
instance::dns_server::tests::test_magic_dns_runner
instance::dns_server::tests::test_magic_dns_server_instance
instance::dns_server::tests::test_magic_dns_update_replaces_records_for_same_client
instance::instance::tests::validate_public_ipv6_patch_ignores_runtime_auto_ipv6_cache
instance::instance::tests::validate_public_ipv6_patch_rejects_non_global_prefix
instance::virtual_nic::tests::tun_test                 # 建 TUN 需要 root
tunnel::quic::tests::ipv6_domain_pingpong
tunnel::tcp::tests::ipv6_domain_pingpong
tunnel::udp::tests::ipv6_domain_pingpong
tunnel::udp::tests::test_v6_hole_punch_packet
tunnel::wireguard::tests::ipv6_domain_pingpong
```

另外 `tunnel::udp::tests::*` 用固定端口(5556 等),并行跑时会互相抢端口报 `AddrInUse`,
哪个先抢到哪个过——偶发挂的是 `udp_pingpong` / `test_conn_counter` 之类,单独跑必过,
不算新失败(2026-09-03 归因)。

出现**不在这个清单里**的失败:先在 rebase 前的 commit 上跑同一条测试对照,
确认是上游带来的还是补丁带来的,再决定。清单本身也要随上游变化更新。

### 6. 发布

只有第 4 步全绿才做这一节。

```bash
git tag mac-base-$(date +%Y%m%d)                    # 必须:见下
git push origin --tags
git push --force-with-lease -u origin HEAD          # 当前分支,如 releases/v2.7.0
```

**tag 不是可选项。** easytier-mac 用 submodule pin 本 fork 的 commit。换版本时旧分支原样保留,
旧 pin 自然可达;但同一基点上修改补丁要 force-push,旧 commit 会在远端变成不可达对象,
GitHub GC 之后 submodule 就拉不到了。tag 保证被 pin 的 commit 始终可达,也是回滚点。

然后在 easytier-mac 里 bump submodule:

```bash
cd ~/Developer/Joy/easytier-mac/vendor/EasyTier
git fetch origin && git checkout origin/releases/v2.7.0    # 当前分支
cd ../.. && # 重建 + 验证,再提交 gitlink
```

### 7. 回滚

push 之前:`git rebase --abort`,或 `git reset --hard <before>`
(`follow-upstream.sh` 开头打印了 rebase 前的 commit)。

push 之后:上一个 `mac-base-*` tag 就是可用的回退点,easytier-mac 的 submodule
指回那个 commit 即可。

## 上游发了新版本时

比如上游从 `v2.6.4` 走到 `v2.7.0`。先做第 1 步(含同步 `main`),再确认新 tag 存在:

```bash
git tag -l 'v*' --sort=-v:refname | head -3
```

从当前分支新开一条同名分支,把补丁 rebase 到新 tag 上,然后走第 2–6 步:

```bash
git switch -c releases/v2.7.0 releases/v2.6.4
fork/follow-upstream.sh v2.7.0
```

旧的 `releases/v2.6.4` 本地和远端都保留,不改名、不删。发布后同步改 easytier-mac 里
submodule 记录的 branch,以及本文和 `CLAUDE.md` 里出现的分支名。

## 平时:在 `upstream/main` 上预演

上游 `main` 在两次发版之间会不断改补丁依赖的文件。等新版本出来再一次性处理,
冲突最集中。隔一段时间预演一次,只检查、不发布。先按第 1 步同步 `main`,然后:

```bash
git log --oneline <当前 tag>..upstream/main -- easytier/src/tunnel/common.rs \
  easytier/src/common/stun.rs easytier/src/common/global_ctx.rs \
  easytier/src/common/ifcfg/darwin.rs easytier/src/connector \
  easytier/src/instance/virtual_nic.rs easytier/src/arch/mod.rs   # 接入点文件的上游改动

git switch -C canary releases/v2.6.4     # 每次从当前发布分支重建
fork/follow-upstream.sh upstream/main
git switch releases/v2.6.4
```

- 冲突按第 3 节解决,`git rebase --continue` 后 `rerere` 会记下解法;换版本时同样的冲突自动重放。
- `canary` 只在本地,不 push、不跑实机 gate、不给 easytier-mac 用。
- 预演中守卫或单测失败,说明上游 `main` 已经动了补丁依赖的机制,提前读懂那个改动,
  别等到换版本时再处理。

## 什么时候该重新审视这套东西

- 上游改了 socket 创建路径(新的 tunnel 类型、连接器重构)→ 重新确认 `SCANNED` 覆盖面还够
- 上游支持了 IPv6 全隧道 → `underlay_udp_bind.rs` 里那三条 IPv6 豁免要重新评估
- 上游自己实现了 macOS 全隧道 → 补丁可以整体删掉,回到跟随上游零成本
