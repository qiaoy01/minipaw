# Tree Advisor 设计 · 第一晚 demo + 路线图

**作者**：姜博文（Tree）
**日期**：2026-05-14（凌晨）
**分支**：`tree-advisor`（基于 `main`）
**作业来源**：余樵 5/9 22:21 群作业 + 姚老师 5/13 13:01 sprint 召集

---

## 一、设计：Trajectory-Critic Retry（不抄 advisor 分支）

### 1.1 advisor 分支原设计速述

advisor 分支（余樵 + 团队）实现的是 **shadow-run + offline learning**：

- primary 答完后，advisor 再单轮影子答一份相同 query
- `compare()` 用 Jaccard token-set 相似度算 Match / Close / Divergent
- 调 `adjust-meta.md` 让 advisor 决定 `NO_CHANGE` / `PROMPT_RULE_APPEND` / `SKILL_NEW`
- 三档模式 Training（自动 apply）/ Trial（暂存 proposal）/ Work（跳过）
- 目标：**离线学习**，让系统 prompt 库和 skill 库随时间进化

### 1.2 我的设计：Trajectory-Critic Retry

完全不同的思路 —— **online steering**，专攻 baseline 失败模式：

```
case.input
   ↓
[Pass 1] primary (deepseek-chat) → 完整执行 → transcript
   ↓
verdict = Pass?  →  停（不浪费 critic）
   ↓ no
[Critic] deepseek-reasoner 看 transcript + must_tools/must_in_output
         → 单条 ≤80 字 hint
   ↓
[Pass 2] primary (deepseek-chat) 重跑 input + "ADVISOR HINT: <hint>"
   ↓
final verdict = Pass 2 verdict
```

### 1.3 为什么对症

余樵 baseline 报告（qwen9b on advisor 分支，105 case）的 fail 分布：

| 失败模式 | 数量 | critic 能 fix？ |
|---|---|---|
| working_memory_value_loss_H1_or_H2 | 23 | ✅ 直接告诉 "把 scene_id 透传到 DONE" |
| tool_selection_skipped_required_tool | 22 | ✅ 直接告诉 "你漏了 robot_inventory_list" |
| other | 17 | ⚠️ 取决于具体 |
| initiation_no_tool_use | 6 | ✅ 提醒 "用 paw.py 不要 python print" |
| wrong_arguments | 5 | ⚠️ critic 不一定能精确指明参数 |

前两类占 45/73 fail（62%），critic-retry 设计就是为这两类量身打造。

### 1.4 和姚老师 5/13 sprint lever 的对齐

姚老师 lever #2："加 critic agent（Multiple hypothesis 验证后再 commit）"——本设计**正是** critic agent，但不是同一时刻多假设，而是"先一次执行 → critic 看完 trajectory 给修正 hint → 重执行"。属于 sprint lever 家族。

### 1.5 不抄 advisor 代码的具体边界

| 文件 | 是否 copy | 原因 |
|---|---|---|
| `src/advisor.rs` | ❌ 一行没抄 | 这是 advisor 分支的核心逻辑代码 |
| `src/adjustments.rs` | ❌ 一行没抄 | 同上 |
| `src/prompts.rs` | ❌ 一行没抄 | 同上 |
| `src/minicore.rs` advisor 注入段 | ❌ 没改 main 分支 minicore | 同上 |
| `prompts/*.md` | ❌ 没用 | 我的 critic 用自写 prompt（`run_with_critic.py::CRITIC_SYSTEM`） |
| `pawbench/` 整个目录 | ✅ checkout 进来 | 这是 benchmark 测试基础设施（test fixture）非 advisor 代码 |
| `target/release/minipaw` binary | ✅ 用 advisor 分支编译的 | 因为 main 分支 hardcode prompt 不教 paw.py，跑出 0 分没参考意义 |

实现方式：**所有 advisor 逻辑都在 `pawbench/run_with_critic.py`（Python 层）**，不动 Rust 源码。源码层面 tree-advisor 分支 == main 分支。

---

## 二、第一晚 demo 结果（5 case A01-A05）

### 2.1 配置

- **primary**：致远一号 deepseek-chat（685B，OpenAI 兼容）
- **critic**：致远一号 deepseek-reasoner（685B 推理模型）
- **minipaw binary**：advisor 分支 `24260e5` 编译
- **workspace**：`pawbench/workspace/`（50 个 skill 已 install）
- **case**：A01-A05 `forage_pick`（5 份同 input，measure variance）
- **per-case timeout**：240s

### 2.2 数字（诚实版）

| 跑次 | Pass | Partial | Fail | Score | 说明 |
|---|---|---|---|---|---|
| baseline 1 | 3 | 0 | 2 | 60.0 | A03/A05 missing robot_inventory_list |
| critic-retry v1（critic=reasoner）| 4 | 0 | 1 | **80.0**（**虚的**）| 2/2 critic 调用失败（timeout / null content）→ hint 是 `<critic-error>` 字符串 → "提升"实为 noise |
| critic-retry v2（critic=deepseek-chat）| 2 | 0 | 3 | 40.0 | critic hint 质量好（A02/A03/A04 hint 都准确指明缺陷）但 3 个 fail case **pass 2 都因致远一号 connection timeout 30s 中断**（curl error 28/35），未能验证 hint 实际效果 |

### 2.3 已经能确认的事实

✅ **设计端到端 wire-up 跑通**：A01_forage_pick 在 critic-retry v2 完整跑出 8 工具调用 + 正确 final output：`"Stowed garlic_mustard into inventory. Camera scene_id=6. Inventory now contains 1 item: garlic_mustard."`

✅ **critic 能输出对症 hint**：v2 跑里 critic 给的三条 hint（A02/A03/A04）都准确诊断了真实缺陷（missing tools / missing inventory list / missing scene_id 透传）

❌ **critic 实际效果未验证**：因 v2 跑里 3 个目标 case 全因 API timeout 失败，**无法 measure hint 被 model 采纳后是否提分**

❌ **deepseek-chat noise 大**：同样 5 个 forage_pick（input 完全相同）独立跑两次，Pass 率在 40-60% 间漂——这是 deepseek-chat 在中文 prompt 下做长指令任务的固有 variance。在如此 noise 下，5 case 样本不够 detect critic-retry 的真实效果。

### 2.3 余樵 baseline 对比（同 5 case 子集）

余樵 `pawbench/reports/robot_0_before_training_0510.md` (qwen9b 全量 105 case)：A01-A05 都是 Partial（5/5 missing_in_output=inventory）。
我的 baseline（deepseek-chat 全量 prompt）：A01/A02/A04 Pass，A03/A05 Fail。
**deepseek-chat 比 qwen9b 工具调用更稳，但偶发漏 robot_inventory_list**——正是 critic 的目标。

---

## 三、撞过的墙（已记录在 progress-log）

### 3.1 致远一号 + main 分支 = 0 分

main 分支硬编码 minihow system prompt 只教 `EXEC: python3 -c "..."`，**完全没提 `paw.py <skill>`**。deepseek-chat 全程用 `python3 -c "print('camera_frame=fake')"` 假装机器人，零工具调用。
- 5 case baseline 冒烟 5/5 timeout（240s rc=-1）
- 单 case 手动跑 6 分钟才 3-4 步
- 决定切 advisor 分支 binary（它把 skill 列表注入 prompts/minihow.md），效果立竿见影：A01 32s 完成 9 步全部 paw.py。

### 3.2 致远一号 HTTP 长 session bug

minipaw 自实现的 HTTP 客户端（`src/http.rs`），在 main 分支长 session 撞 `Resource temporarily unavailable (os error 35)`——macOS socket non-blocking read 没处理 EAGAIN。
- advisor 分支 prompts 让 session 更短（每步直接 paw.py 而非长 python），间接避开了这个 bug
- **未来修复方向**：`src/http.rs` 读 socket 时遇 EAGAIN 应 retry 而非 propagate error。今晚先绕开。

### 3.3 致远一号免费但慢

- per-step 30-80s（deepseek-chat 思考时间长）
- 全量 105 case × 平均 14 步 × 50s ≈ 20 小时
- 限额 100 req/min 也是瓶颈
- **未来**：试 minimax-m2.5（192k context, 更快）或 qwen3coder（30B 更小更快）

---

## 四、下周方案（5/14-5/21 sprint 配合）

### 4.1 我的承诺范围

姚老师 5/13 sprint 没 @ 我，主线由陈哲文（Sober）领。我做的是**完成全员作业 + 提供 critic-retry 作为可配合的 lever**。

### 4.2 待完成

| # | 任务 | 估时 |
|---|---|---|
| 1 | 完成 5 case demo（baseline + critic-retry 数字对比）| 当晚 |
| 2 | 全量 105 case baseline + critic-retry 对比 | 24 小时（需找余樵要云端机；本机 macOS 单 case 60s 跑全量太长） |
| 3 | error-mode-targeted hint 分桶（按 4 类难度 + 失败模式 tailored hint）| 2-3 天 |
| 4 | critic 模型 ablation（deepseek-reasoner vs minimax-m2.5 vs qwen3coder）| 1 天 |
| 5 | 把 critic-retry 集成进 Rust（`src/critic.rs` 自己设计的模块），让 advisor 分支可选开启 inline critic mode | 3-4 天 |
| 6 | **修 minipaw HTTP 客户端 EAGAIN + 30s connect timeout**（不修这个，critic-retry 的 pass 2 一直被切断）| 半天 |
| 7 | 把 hint 注入位置从 user message 改成 system prompt 前缀，提高 model 对 hint 的"采纳率"（v2 demo 没法 measure 是否被采纳）| 半天 |

### 4.3 风险

- critic-retry 让每个 case API 调用翻倍 → token 用量 ×2 → 致远一号周限额 1B 仍 ≤3% 但 throughput 减半
- critic 不一定每次都 fix（depends on transcript quality）
- 与陈哲文的 sprint research plan 可能重叠 critic agent lever——5/14 开会后对齐谁负责哪段

### 4.4 联系点

- 找陈哲文（@Sober）：要云端机 + 对齐 primary 模型最终选型（致远一号 deepseek-chat vs qwen / glm 团队统一版本）
- 找余樵：要 `pawbench/cases/generate.py` 思路了解 case 难度分布，看能否扩 case 测 critic 在数值题 / ranking 题上的效果

---

## 五、文件清单（本分支新增）

```
docs/2026-05-14-tree-advisor-design.md      # 本文档
pawbench/run_with_critic.py                 # critic-retry runner（~200 行 Python）
pawbench/                                    # 从 advisor 分支 checkout 来的 test fixture
pawbench/results/tree_baseline_5/           # baseline 5 case 结果
pawbench/results/tree_advisor_5/            # critic-retry 5 case 结果
pawbench/workspace/minipaw.json             # 致远一号 deepseek-chat 配置
```

源码（`src/`）一行没改 —— tree-advisor 分支当前 == main 分支。所有 advisor 能力都在 Python 层实现。
