# Process Log

<!--
  意图轨迹：只记 assistant 回复中命中决策信号词的高信号摘要。
  不记工具调用/中间产物/调试输出（避免膨胀）。审计者对照它审产物。
-->

- 🤔 完成。所有任务已交付，本地验证全绿。  ## 交付内容  **后端（真正支持 win + mac）** - `crates/cowt/src/backend/macos.rs` — macOS 内核 union mount（`mount -t union`，BSD 风格）：零第三方驱动（macFUSE kext 无法在 CI 无人值守批准，这是选它的原因）、root 运行、删除落 `.wh.` w… <!--run:run-17816-ph5q9u-->

- 🤔 审计发现了一个真 blocker：run.rs 的 stale 自动卸载没有鉴别「这是 cowt 自己的残留」——可能卸载用户自己的挂载点，且与并发 run 有竞态窗口。修复方案明确：以**残留的 run.pid 文件**（pidfile 存在但进程已死）作为判据。顺带处理次要发现： <!--run:run-17816-ph5q9u-->
