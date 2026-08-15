# co-worktree

**Git-worktree 式的隔离、审查、合并能力——给任意应用程序的配置与数据目录。**

在主环境运行程序时，其所有文件/配置副作用被透明重定向到隔离层；你可以审查变更、
选择性合并回主环境，或一键丢弃。**不是容器，不是 VM，不是安全沙箱。**

![CI](https://github.com/Nuctori/co-worktree/actions/workflows/ci.yml/badge.svg)

> English version: [README.md](README.md).

## 特性

- **无拷贝隔离**：`fork` 只生成元数据快照（路径 + BLAKE3 哈希），不复制任何文件；
  `run` 在原始路径上挂载写时复制视图——程序读穿透到宿主目录，写只落入隔离层。
- **变更可审查**：`diff` 对比 fork 快照，输出文件级、Myers 行级、JSON/YAML 键级差异。
- **安全合并**：`apply` 做三路合并（base ⊗ current ⊗ worktree），有冲突零写入；
  `drop` 卸载并删除隔离层，宿主零残留。
- **跨平台**：Linux（kernel overlayfs，root 或 userns）、macOS（FUSE-T，无内核扩展、
  无特权）、Windows（WinFsp，无需管理员）——单一二进制，运行时自动探测后端，
  三端共用同一套 whiteout 编码。
- **崩溃恢复**：`cowt run` 被强杀（kill -9 / 断电）后，下一次 `run` / `diff` /
  `apply` / `drop --force` 通过过期 pidfile 自动识别残留——且仅当挂载可证明是
  本项目自己的残留；外来挂载一律拒绝卸载。
- **零守护进程、零网络、完全离线。**

## 安装

```sh
# Linux: 唯一的运行时依赖是 fuse-overlayfs（用户态，无需内核模块）
sudo apt-get install fuse-overlayfs   # Debian/Ubuntu
sudo dnf install fuse-overlayfs       # Fedora

cargo install --path crates/cowt --bin cowt   # 或从 Releases 下载单二进制
```

macOS 与 Windows：

```sh
# macOS: FUSE-T（kext-less FUSE，经 NFS 实现）——无内核扩展、无批准弹窗、无需 root
cargo install --path crates/cowt --bin cowt
bash scripts/macos/install-fuse-t.sh          # 一次性安装 FUSE-T + 链接 libfuse
cowt run vscode -- code

# Windows: 需要 WinFsp（签名内核驱动 + 用户态 DLL）
winget install --id WinFsp.WinFsp   # 或 choco install winfsp / https://winfsp.dev
cowt run vscode -- code             # 无需管理员
```

`cowt doctor` 在任何平台上报告后端可用性。

## 快速上手

```sh
# 1. Fork：为 VS Code 的配置目录创建隔离工作树（仅元数据快照，不复制文件）
cowt fork ~/.config/Code --name vscode

# 2. Run：VS Code 看到的是正常路径，写操作全部落入隔离层
cowt run vscode -- code

# 3. Diff：审查隔离层相对 fork 快照的变更
cowt diff vscode              # 文件级
cowt diff vscode --content    # + Myers 行级 diff 与 JSON/YAML 键级 diff
cowt diff vscode --json       # 机器可读

# 4a. Apply：三路合并回主环境；无冲突才写入，有冲突零污染
cowt apply vscode --dry-run   # 预览操作与冲突
cowt apply vscode

# 4b. Drop：一键丢弃，宿主零残留
cowt drop vscode              # 进程在跑会拒绝；--force 先杀进程再清理
```

## 工作原理

```
┌───────────────────── 宿主目录 ~/.config/Code ─────────────────────┐
│ fork  → base manifest（路径 + BLAKE3 哈希，纯元数据）               │
│ run   → 在原始路径上挂载合并视图                                    │
│         读 → 透传 lower（宿主目录）                                 │
│         写 → 重定向到 upper（隔离层）                               │
│ diff  → base manifest ⊗ upper → added / modified / deleted        │
│ apply → base ⊗ current ⊗ worktree 三路合并                         │
│         base==current 且 worktree 变 → 应用                        │
│         三者皆不同 → 冲突，零写入，输出结构化报告                     │
│ drop  → 卸载 + 原子删除隔离层                                       │
└──────────────────────────────────────────────────────────────────┘
```

删除在隔离层中落为 **whiteout**（内核风格 char dev 0:0 携带原名，或零大小的
`.wh.` 前缀文件）——重命名、删除后重建、大小写不敏感查找在三个后端上行为完全一致。

## 平台支持

| 平台 | 后端 | 要求 | 验证状态 |
| --- | --- | --- | --- |
| **Linux** | kernel overlayfs（root）/ overlayfs+userns（非 root）/ fuse-overlayfs（兜底） | fuse-overlayfs 包 | ✅ CI 完整 E2E + 真机 |
| **Windows** | WinFsp 用户态 CoW 文件系统（`winfsp` 绑定） | WinFsp（winget / choco / 官网） | ✅ CI 完整 E2E + 真机 |
| **macOS** | FUSE-T 用户态 CoW 文件系统（`fuser` 绑定，经 NFS 实现，kext-less、无特权） | FUSE-T（`scripts/macos/install-fuse-t.sh`） | ✅ CI 核心逻辑 E2E——挂载用例在无头 runner 自动跳过（FUSE-T NFS 挂载在 CI 上不生效，环境限制而非代码问题）。挂载路径待真机验收 |

macOS/Windows 后端运行期把宿主目录移到 `state/<id>/real`，再在原始路径上挂载视图
（macOS 为 symlink，Windows 为 WinFsp 直挂）。

CI 基线：**12/12 全绿** —— rustfmt、clippy `-D warnings`、三平台全量测试、三平台
真实后端 E2E、Windows 交叉编译检查、三平台 release 构建。

## 边界声明

- **非沙箱**：不限制 CPU、内存、网络、进程间通信；不防恶意软件——只防副作用污染
- **仅限用户级目录**：默认拒绝 `$HOME` 之外的路径（`--force-path` 可覆盖）
- **Windows 注册表**：MVP 不隔离，仅文件级配置
- **符号链接不隔离**：fork 目录内的链接会被合并视图跟随——通过它写入直接落到宿主
  目标，且 `cowt diff` 不可见；fork 检测到 symlink 时会打印警告（Windows/macOS
  后端经 copy-up 将写入包含进隔离层，但不保留链接语义）
- **apply 以冲突为门禁，非事务性**：单文件 rename 原子，但多文件 apply 中途中断
  （崩溃/断电）会留下已写入的文件——每个文件各自一致，无半截文件体；worktree
  运行中 apply 会被拒绝（规划前后各检查一次）
- **Windows 同卷限制**：状态目录（`COWT_HOME`，默认 `%LOCALAPPDATA%\cowt`）必须与
  目标目录同卷（Windows 不能跨卷 rename）；`cowt run` 会给出明确报错

## 性能

| 指标 | 标准 | 实测 |
| --- | --- | --- |
| 空 worktree fork | < 500ms | ~5ms |
| 10k 文件 manifest 扫描 | 支持 10k+ 文件 | ~215ms |
| 顺序写开销 | < 20% | kernel-overlay ~9%（CI 实测）；fuse-overlayfs ~4–7% |
| 10k 文件 diff | < 3s | ~20–200ms |

## 开发与验证

```sh
cargo test --workspace                                  # 单元测试 + CLI 集成测试
cargo test --test e2e -- --ignored --test-threads=1     # 真实后端 E2E（无头环境自动跳过挂载用例）
cargo clippy --workspace --all-targets -- -D warnings
```

结构：`cowt-core`（纯 Rust、跨平台的 manifest/diff/merge）+ `cowt`（CLI + 平台后端）。
每次 push 都会在 GitHub Actions 上跑完整测试矩阵。

## License

MIT；Windows 后端通过 GPL-3.0 的 `winfsp`/`winfsp-sys` 绑定链接 WinFsp。WinFsp 本体为
GPLv3 + FLOSS 例外（允许 FLOSS 项目链接其 DLL），本项目的 MIT 许可不受影响；绑定层代码
随 Windows 二进制以 GPL-3.0 分发。
