# 界面只由 winduiweb 承担，egui 窗口下线

日期：2026-09-27
分支：`feat/inline-playback`（集成目标 `perf/algorithms`）
状态：已确认；第 1、2 步已落地，第 3 步（删源码）明确不做

## 背景

托盘打开哪个窗口是一个决定，不是一个字符串的巧合——这一点写在 `windcap/supervisor/src/native.rs` 的 `pub const INTERFACE: &str = "winduiweb"` 和它的注释里。决定做出之时的现状是：`windui`（egui 那个）仍然被构建、仍然被打包、仍然可以双击 `bin\windui.exe` 打开；下面第 1、2 步要改的就是后两项，第 3 步不动它被构建这一项。

于是最近这一批功能出现了两份真相：

* AI 提示词面板只在 egui 窗口里（`windcap/windui/src/view.rs:1924-2032`，函数在 `windui/src/ai.rs:1043-1150`），而 `winduiweb` 全仓 `prompt` 零命中。用户按 `bin\windai.exe prompts` 结尾那句 "The settings screen edits these same files" 去找，找不到的正是他们每天用的那个窗口。
* 表单是声明驱动的：`commands.rs:288/:374/:574` 把 `Field::ALL`/`RField::ALL`/`AField::ALL` 迭代成 DTO，`Forms.tsx:233-350` 只按 `kind` 分支。加一行声明就两个窗口同时出现——这本来是最省的一面，但 prompt 是文件不是配置键，进不了这条通道，只能在 web 侧新搭命令。

只要两个窗口并存，每个新能力就要付两遍代价，而且第二遍永远不来。用户的判定是：不再付两遍。

## 决定

`winduiweb.exe` 是这个产品唯一的界面。egui 窗口从"用户可达"降级为"内部工具"，分三步，每一步都可单独回退：

1. 不再由托盘启动（`native.rs:45` 已经是 `winduiweb`，且无回退）；从 `native.rs:35` 的 `BINARIES` 七元组里摘掉 `windui`，让 `windsetup doctor` 不再把它算作安装完整性的一部分。
2. 不再进发布暂存（`windcap/release.ps1:294-297` 那一条 `windui.exe`，以及 `:855` 的 `--SkipUi` 名单、`:748-749` 的发布说明措辞）。
3. 源码暂时保留并继续编译。`windui/src/{backend.rs,ai.rs,settings.rs,record.rs}` 是这些能力的**唯一实现**，web 侧只是它们的第二个消费者；在 prompt 编辑、帧读取、遮罩这些逻辑确认已经被 web 侧覆盖之前删掉它们，等于删功能。

## 后果

* 界面能力不再欠账：prompt 编辑器、后处理时间节点、当日总结栏、右侧栏条目，只做一份，落在真正被打开的那个窗口上。
* `render_tests.rs` 那套钉住 egui 页面的断言（`:221-223` 的 `Search`/`OneDay` 字面量、`:1642` 的 `Field::ALL.len()`）从此只约束一个不再面向用户的窗口。它们继续跑，但不再被当作"用户看到的对不对"的证据；用户看到的对不对，改由 web 侧的命令契约和 `tsc --noEmit` 加人工核验来保证。
* 三份 i18n 泄漏不再需要修两遍：`view.rs` 里 33 处从未走目录的 `RichText::new("<English>")`、以及 `ai_group_prompts`、`windui_frame_actual_size`、`windui_frame_gone` 这三个三语全无的键，随窗口下线一并失去用户可见性；目录全量补齐的范围因此收在 `winduiweb` 这一侧。
* 一个明确的遗留，范围要说清楚：`bin\windui.exe` 仍在，仍可双击——但那说的是**这个开发树**。`build.ps1` 继续编译它（`wind_ui` 这个库目标是帧读取、提示词面板和表单字段声明的唯一实现），只是不再 `-Stage` 它；`release.ps1` 的暂存表里已经没有这一行，所以任何用户解压出来的安装都不含这个文件，`windsvc doctor` 也不再把它算作安装完整性的一部分。"仍在 `bin\`"与"发布包里有它"是两件不同的事，现在只有前者成立。真正的删除需要一个后续决定，前提是那时没有任何能力只活在它里面。
* 第 1、2 步已经落地：`native.rs` 的 `BINARIES` 已从七元组变为六元组，`release.ps1` 不再暂存 `bin\windui.exe`，`build.ps1` 不再把它 `-Stage` 进 `bin\`，并由 `native.rs` 的一条测试钉住"不得再被加回发布集"。第 3 步——删源码——按本 ADR 的明令**没有做**。
