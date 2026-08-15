# Decision Chain

<!--
  结对决策审计的决策链。只追加，不修改旧条目。
  新决策通过 decision_add 工具追加（自动编号），审计者只读本文件。
  修订旧决策 = 新条目声明 Supersedes: D-00X。
-->

## D-001: macOS 后端采用内核 union mount（mount -t union）而非 macFUSE [Accepted]
- Context: macFUSE kext 在 GitHub Actions 运行器上无法无人值守批准（macos-11+ 已知问题，macfuse#1039 / runner-images#4731）；macOS 内核自带 BSD 风格 union mount（mount_union，mount -t union <upper> <mountpoint>，挂载点原内容即 lower）。需要 root。xnu 的 union 删除生成 .wh. 前缀 whiteout，与 cowt-core 现有解析兼容。
- Decision: macOS 后端 = 内核 union mount（backend/macos.rs），根权限运行（CI 用 sudo），无第三方驱动；cowt doctor 通过真实挂载探测报告可用性。
- Rationale: CI 上唯一不需要 kext 批准的可行路径；删除语义与现有 whiteout 解析一致。代价：Apple 文件接口（Finder 等）对 union 支持不佳——已文档化，POSIX 程序正常。
- Alternatives: macFUSE + fuser crate（否决：CI kext 批准不可行）；macFUSE 5 FSKit（否决：挂载点限于 /Volumes，且需系统扩展批准）
- Confidence: high
- Date: 2026-08-15T00:10:52.582Z

## D-002: Windows 后端采用 WinFsp 用户态 CoW 文件系统 + junction 移动-恢复编排 [Accepted]
- Context: Windows 无 bind/union 挂载；WinFsp 是唯一可用用户态文件系统驱动（choco 可装，GH Actions windows 运行器管理员）。winfsp crate（0.12.6）提供 FileSystemContext trait + FileSystemHost，支持目录挂载点，bundled import lib 使交叉编译检查无需安装。junction 创建无需特权。
- Decision: backend/winfsp.rs：实现 CoW passthrough FS（写打开时 copy-up、删除落 .wh. whiteout、rename 跨层处理）；运行期把宿主目录移到 state/&lt;id&gt;/real，原路径变 junction → WinFsp 视图（state/&lt;id&gt;/view）；卸载时恢复。悬空 junction + real 由下次 run / drop --force 自动恢复。
- Rationale: junction 方案让挂载点位于状态目录内（WinFsp 卸载时删除挂载目录的语义不触及宿主路径），且读取 lower 用普通路径即可，避免 WinFsp 挂载覆盖下的自递归。
- Alternatives: WinFsp 直接挂载到目标路径（否决：卸载会删挂载目录 + lower 读取递归）；卷 GUID 路径绕过挂载（否决：复杂且依赖 \\?\Volume 语义）
- Confidence: medium
- Date: 2026-08-15T00:10:52.585Z

## D-003: E2E 套件用 Rust 集成测试重写并删除 bash 版 [Accepted]
- Context: 原 scripts/e2e.sh 是 bash 套件（mktemp/date %N/dd fdatasync//proc/self/mounts/fusermount3/kill -9 等 unix 专有），无法移植到 Windows；macOS 的 BSD date 缺 %N。Rust 集成测试可用 CARGO_BIN_EXE_cowt 定位二进制，同一套代码三平台跑。
- Decision: E2E 重写为 crates/cowt/tests/e2e.rs（#[ignore] 标记，CI 用 cargo test --test e2e -- --ignored 运行），配套 src/bin/e2e-helper.rs 充当"被隔离的应用"（sleep/crash/perf 模式）；删除 scripts/e2e.sh。
- Rationale: 一套代码三平台验证（bash 套件在 Windows 上不可行）；无 shell 依赖（helper 二进制代替 sleep/kill）；性能预算用 std::time::Instant 计时。
- Alternatives: PowerShell 版 Windows 套件（否决：两套件漂移）；msys bash（否决：路径转换噩梦）
- Confidence: high
- Date: 2026-08-15T00:11:03.802Z

## D-004: 接受 GPL-3.0 winfsp 绑定并文档化许可边界 [Accepted]
- Context: winfsp/winfsp-sys crate 均为 GPL-3.0；WinFsp 本体是 GPLv3 + FLOSS 例外（明确允许 FLOSS 项目链接其 DLL）。项目 MIT 许可。
- Decision: 接受 GPL-3.0 绑定依赖，README 新增 License 说明：MIT 不受影响（WinFsp FLOSS 例外覆盖 DLL 链接）；Windows 二进制随绑定层以 GPL-3.0 分发。
- Rationale: Windows 后端没有可行的非 GPL 替代驱动；项目本身开源，GPL-3.0 分发可接受且已文档化。若未来需要闭源分发，可替换为手写 FFI 绑定（隔离在 backend/winfsp.rs）。
- Alternatives: 手写 WinFsp FFI（否决：~800 行 unsafe 接口结构体，风险高收益低）
- Confidence: medium
- Date: 2026-08-15T00:11:03.805Z

## D-005: stale-mount 自动卸载以残留 run.pid 为鉴别（审计修复） [Accepted]
- Context: 审计（run-17816）指出：stale-mount 自动卸载若无鉴别，会在 Linux/macOS 上裸 umount 任意挂载点（可能不是 cowt 的），且 mount→write_pidfile 间隙存在并发 run 竞态窗口。
- Decision: stale 挂载清理以 State::stale_run（run.pid 文件存在且 pid 已死）为唯一判据：仅当该 worktree 自己的 run 崩溃过才允许自动卸载；无 stale pidfile 的挂载一律拒绝叠加（保留旧语义）。winfsp mount() 失败回滚宿主目录。
- Rationale: run.pid 由 run_isolated 在 spawn 后写入、正常退出时清除——「pidfile 存在但进程死」精确刻画「我们的 run 崩了」；并发窗口内 pidfile 未写 → stale_run=false → 拒绝，竞态关闭。
- Confidence: high
- Date: 2026-08-15T00:22:16.156Z
