# co-worktree

**Git-worktree 式的隔离、审查、合并能力——给任意应用程序的配置与数据目录。**

在主环境运行程序时，其所有文件/配置副作用被透明重定向到隔离层；你可以审查变更、
选择性合并回主环境，或一键丢弃。**不是容器，不是 VM，不是安全沙箱。**

## 安装

```sh
# Linux: 唯一的运行时依赖是 fuse-overlayfs（用户态，无需内核模块）
sudo apt-get install fuse-overlayfs   # Debian/Ubuntu
sudo dnf install fuse-overlayfs       # Fedora

cargo install --path crates/cowt      # 或从 Releases 下载单二进制
```

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
|---|---|
| `cowt-core` | 纯 Rust、跨平台：Manifest（并行 BLAKE3 扫描）、Diff（Myers 行级 / JSON·YAML 键级）、三路 Merge（暂存区 + rename 原子提交） |
| `cowt` CLI | 平台后端 Trait；Linux 后端自动选择三种模式（见下）；Windows（WinFsp）/macOS（macFUSE）为规划中的后端桩 |

### Linux 后端自动选择

| 模式 | 条件 | 特点 |
|---|---|---|
| `kernel-overlay` | root | 内核 overlayfs 直接挂载，原生性能 |
| `kernel-overlay+userns` | 非 root + 可用 user namespace | 命名空间内内核 overlay，挂载随命名空间消亡，零残留 |
| `fuse-overlayfs` | 兜底（如 AppArmor 限制 userns 的 Ubuntu） | 用户态，无需特权 |

运行时探测，无需配置；`cowt doctor` 显示当前模式。删除操作在三种模式下均正确落为
whiteout（char dev 0:0 原名 / `.wh.` 前缀零大小文件两种编码均支持解析）。

设计决策：同步 I/O 无 async runtime（FUSE 回调模型是同步的）；零网络服务、零容器运行时，
完全离线可用；MVP 不隔离 Windows 注册表（现代应用配置已文件化）。

## 边界声明

- **不隔离运行时**：不限制 CPU、内存、网络、进程间通信
- **不防恶意软件**：只防"副作用污染"，不防提权/内核漏洞/驱动注入
- **只隔离用户级目录**：默认拒绝 `$HOME` 之外的路径（`--force-path` 可覆盖）
- **Windows 注册表**：MVP 不隔离，仅文件级配置

## 性能（验收标准 + 实测）

| 指标 | 标准 | 实测 |
|---|---|---|
| 空 worktree fork | < 500ms | ~5ms |
| manifest 扫描 | 支持 10k+ 文件 | 10k 文件 ~215ms |
| 顺序读写开销 | < 20% | kernel-overlay ~9%（CI 实测）；fuse-overlayfs 在普通 SSD 上 ~4–7% |
| 10k 文件 diff | < 3s | ~20–200ms |

## 开发与验证

```sh
cargo test --workspace          # 17 项核心单元测试 + 8 项 CLI 集成测试（含真实 FUSE）
bash scripts/e2e.sh ./target/release/cowt   # 34 项端到端验收
```

CI（GitHub Actions）：rustfmt、clippy `-D warnings`、全量测试、真实 fuse-overlayfs
E2E、Windows/macOS 交叉编译检查、release 构建产物上传。

## License

MIT
