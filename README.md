# co-worktree

**Git-worktree 式的隔离、审查、合并能力——给任意应用程序的配置与数据目录。**

在主环境运行程序时，其所有文件/配置副作用被透明重定向到隔离层；你可以审查变更、
选择性合并回主环境，或一键丢弃。**不是容器，不是 VM，不是安全沙箱。**

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
cargo install --path crates/cowt --bin cowt   # 或从 Releases 下载
bash scripts/macos/install-fuse-t.sh          # 一次性安装 FUSE-T + 链接 libfuse
cowt run vscode -- code

# Windows: 需要 WinFsp（签名内核驱动 + 用户态 DLL）
choco install winfsp                  # 或从 https://winfsp.dev 安装
cowt run vscode -- code              # 无需管理员
```

`cowt doctor` 在任何平台上报告后端可用性。

## 快速上手

```sh
# 1. Fork：为 VS Code 的配置目录创建隔离工作树（仅元数据快照，不复制文件）
cowt fork ~/.config/Code --name vscode

# 2. Run：在虚拟合并视图中运行程序，写操作全部落入隔离层
cowt run vscode -- code

# 3. Diff：审查隔离层相对 fork 快照的变更（文件级 / 行级 / JSON-YAML 键级）
cowt diff vscode
cowt diff vscode --content        # 含 Myers 行级 diff 与键级 diff
cowt diff vscode --json           # 机器可读

# 4a. Apply：三路合并（base / current / worktree）回主环境，原子提交
cowt apply vscode --dry-run       # 预览操作与冲突
cowt apply vscode                 # 无冲突才写入；有冲突零污染中止

# 4b. Drop：一键丢弃，宿主零残留
cowt drop vscode                  # 进程在跑会拒绝；--force 先杀进程再清理
```

## 工作原理

```
┌───────────────────── 宿主目录 ~/.config/Code ─────────────────────┐
│  fork 时: 生成 base manifest（路径 + BLAKE3 哈希，纯元数据）        │
│  run 时:  fuse-overlayfs 挂载合并视图到原路径                        │
│           读 → 透传 lower（宿主目录）                               │
│           写 → 重定向到 upper（~/.local/state/cowt/<id>/upper）      │
│  diff:   base manifest ⊗ upper → added / modified / deleted       │
│  apply:  base ⊗ current ⊗ worktree 三路合并                        │
│          base==current 且 worktree 变 → 应用                        │
│          三者皆不同 → 冲突，零写入，输出结构化报告                     │
│  drop:   卸载 + 原子删除隔离层                                      │
└──────────────────────────────────────────────────────────────────┘
```

## 架构

| 层 | 内容 |
| --- | --- |
| `cowt-core` | 纯 Rust、跨平台：Manifest（并行 BLAKE3 扫描）、Diff（Myers 行级 / JSON·YAML 键级）、三路 Merge（暂存区 + rename 原子提交） |
| `cowt` CLI | 平台后端 Trait：Linux overlayfs 三种模式、macOS 内核 union mount、Windows WinFsp 用户态文件系统（见下） |

### 平台后端

| 平台 | 后端 | 要求 | 特点 |
| --- | --- | --- | --- |
| Linux | kernel overlayfs（root）/ kernel overlayfs+userns（非 root）/ fuse-overlayfs（兜底） | fuse-overlayfs 包 | 运行时自动探测；删除落为 whiteout（char dev 0:0 原名 / `.wh.` 前缀零大小文件两种编码均支持解析） |
| macOS | FUSE-T 用户态 CoW 文件系统（`fuser` 绑定，经 NFS 实现，kext-less、无特权） | 安装 FUSE-T（`scripts/macos/install-fuse-t.sh`） | 无需 root；无内核扩展（macFUSE kext 无法在 CI 无人值守批准；Apple 移除了内核 union mount）；运行期宿主目录移到 `state/<id>/real`，原路径变 symlink → 挂载视图；删除落为 `.wh.` whiteout。注意：FUSE-T 的 NFS 挂载在无头 CI runner 上不可用（fuse_mount 返回但挂载不生效），核心测试仍跑、挂载用例自动跳过 |
| Windows | WinFsp 用户态 CoW 文件系统（`winfsp` 绑定） | 安装 WinFsp（choco / 官网） | 运行期宿主目录移到 `state/<id>/real`，WinFsp 直接挂载到原路径；删除落为 `.wh.` whiteout（大小写规范化处理）；用户态 I/O 写路径开销高于内核后端 |

运行时探测，无需配置；`cowt doctor` 显示当前后端。三种后端共用同一套 whiteout 编码，
diff / merge / apply 逻辑完全一致。

设计决策：同步 I/O 无 async runtime（FUSE 回调模型是同步的）；零网络服务、零容器运行时，
完全离线可用；MVP 不隔离 Windows 注册表（现代应用配置已文件化）；macOS/Windows 后端不处理
符号链接语义。

## 边界声明

- **不隔离运行时**：不限制 CPU、内存、网络、进程间通信
- **不防恶意软件**：只防"副作用污染"，不防提权/内核漏洞/驱动注入
- **只隔离用户级目录**：默认拒绝 `$HOME` 之外的路径（`--force-path` 可覆盖）
- **Windows 注册表**：MVP 不隔离，仅文件级配置
- **崩溃恢复**：`cowt run` 被强杀（kill -9 / 断电）后，残留的挂载与 pidfile 由下一次
  `cowt run` / `cowt diff` / `cowt apply` / `cowt drop --force` 自动识别并恢复——仅当
  该 worktree 自己的 pidfile 已失效时才清理，外来挂载一律拒绝叠加
- **Windows 同卷限制**：状态目录（`COWT_HOME`，默认 `%LOCALAPPDATA%\cowt`）必须与目标
  应用目录在同一卷（Windows 不能跨卷 rename）；跨卷时 `cowt run` 给出明确报错

## 性能（验收标准 + 实测）

| 指标 | 标准 | 实测 |
| --- | --- | --- |
| 空 worktree fork | < 500ms | ~5ms |
| manifest 扫描 | 支持 10k+ 文件 | 10k 文件 ~215ms |
| 顺序读写开销 | < 20% | kernel-overlay ~9%（CI 实测）；fuse-overlayfs 在普通 SSD 上 ~4–7% |
| 10k 文件 diff | < 3s | ~20–200ms |

## 开发与验证

```sh
cargo test --workspace                # 单元测试 + CLI 集成测试（后端可用时含真实挂载）
cargo test --test e2e -- --ignored --test-threads=1   # 端到端验收（root / WinFsp）
```

CI（GitHub Actions）：rustfmt、clippy `-D warnings`、三平台全量测试、三平台真实后端
E2E（Linux kernel-overlay / macOS FUSE-T / Windows WinFsp）、Windows 交叉编译检查、
三平台 release 构建产物上传。

## License

MIT；Windows 后端通过 GPL-3.0 的 `winfsp`/`winfsp-sys` 绑定链接 WinFsp。WinFsp 本体为
GPLv3 + FLOSS 例外（允许 FLOSS 项目链接其 DLL），本项目的 MIT 许可不受影响；绑定层代码
随 Windows 二进制以 GPL-3.0 分发。
