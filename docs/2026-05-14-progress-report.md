# tree-advisor 分支：第一晚进展汇报

**学生**：姜博文（Tree）
**日期**：2026-05-14
**作业来源**：余樵 5/9 22:21 群里布置（minipaw advisor 分支作业）
**分支**：`tree-advisor`（基于 main checkout）

---

## 一、做了什么

### 1.1 设计与实现

在 `tree-advisor` 分支上设计并实现了一个**与 advisor 分支思路不同的** advisor 能力，命名 **Trajectory-Critic Retry**：

- **advisor 分支原设计**（余樵 + 团队）：shadow-run + Jaccard 相似度比较 + adjust-meta 让 advisor 决定 `NO_CHANGE / PROMPT_RULE_APPEND / SKILL_NEW`，属 *offline 学习*（修改 prompt 库和 skill 库）
- **我的设计**：critic 模型读 primary 第一遍完整 trajectory + must_tools 规则，输出 ≤80 字 hint；primary 把 hint 拼到 input 再跑一遍 retry，属 *online steering*（实时修正本次任务）

### 1.2 不抄 advisor 代码的边界

| 文件 | 是否 copy | 说明 |
|---|---|---|
| `src/advisor.rs` / `adjustments.rs` / `prompts.rs` | ❌ 一行没抄 | advisor 分支核心 Rust 逻辑 |
| `src/minicore.rs` advisor 注入段 | ❌ 没改 | 我的 src/ 与 main 分支完全一致 |
| `prompts/*.md` | ❌ 没用 | critic 用我自写 prompt（在 `pawbench/run_with_critic.py` 内） |
| `pawbench/` 基础设施 | ✅ checkout 自 advisor 分支 | 这是 benchmark 测试 fixture（cases/run.py/workspace 等），非 advisor 设计代码 |
| `target/release/minipaw` | ✅ 用 advisor 分支编 | 因 main 分支硬编码 minihow prompt 不教模型 `paw.py <skill>`，跑 baseline 0 分没参考意义 |

**所有 advisor 逻辑都在 Python 层（`pawbench/run_with_critic.py`）**，约 200 行；Rust 源码层面 `tree-advisor` 分支 == `main` 分支。

### 1.3 模型与基础设施

- primary：致远一号 `deepseek-chat`（685B，OpenAI 兼容 API）
- critic：致远一号 `deepseek-chat`（最初用 `deepseek-reasoner` 但 v1 跑里 2/2 critic 返回 null content，换 chat 后 hint 质量稳定）
- benchmark：`pawbench/`（A01-A05 forage_pick 5 case 子集，避免本机 60s/case 跑全量太长）

---

## 二、实验结果（**诚实版**）

5 case A01-A05 forage_pick：

| 跑次 | Pass | Score | 关键解释 |
|---|---|---|---|
| baseline（primary 单独跑）| 3/5 | 60.0/100 | A03/A05 漏 `robot_inventory_list`，丢 inventory/scene 到最终输出 |
| critic-retry v1（critic=reasoner）| 4/5 | 80.0/100 ⚠️ | **数字虚的** — 2/2 critic 调用返回 null content 或 timeout，hint 全是 `<critic-error>` 字符串，"提升"实为 deepseek-chat 随机 noise |
| critic-retry v2（critic=chat）| 2/5 | 40.0/100 | critic hint 质量好（v2 的 3 条 hint 都准确诊断了 missing tool / missing inventory / scene_id 透传缺陷）但 **3 个 fail case 在 retry 阶段被致远一号 30s connection timeout 中断**（curl error 28/35），未能 measure hint 实际效果 |

### 已经能确认的事

✅ 设计端到端跑通：A01_forage_pick 在 critic-retry v2 拿出完整 8 工具调用 + 正确 final output
✅ critic 能输出对症 hint（人读了觉得指出的缺陷正确）

### 未能确认的事

❌ hint 是否真被 model 采纳（v2 三个目标 case 都在 retry 阶段网络超时）
❌ critic-retry 相对 baseline 的真实增益（5 case 太少 + deepseek-chat 同 input 跑 Pass 率漂 40-60% noise 大）

---

## 三、撞过的墙

1. **main 分支硬编码 minihow system prompt 不教模型 `paw.py <skill>`**：deepseek-chat 全程用 `python3 -c "print(...)"` 假装机器人，零工具调用，5 case baseline 全 timeout。→ 切到 advisor 分支编出来的 binary 解决（PromptStore 加载 advisor 分支 prompts/minihow.md，含 skill 列表）
2. **minipaw `src/http.rs` 30s connect timeout 且不 retry**：致远一号 deepseek-chat 偶发响应慢就被切断，critic-retry pass 2 经常死在这里
3. **deepseek-chat 同 input noise 大**（40-60% Pass 率漂）：5 case 样本不足 detect critic-retry 真实增益，需要 ≥50 case 才有统计意义

---

## 四、下周打算（5/14-5/21）

| # | 任务 | 估时 | 依赖 |
|---|---|---|---|
| 1 | 修 `src/http.rs` 加 EAGAIN retry + connect timeout 拉到 60s | 半天 | — |
| 2 | hint 注入位置从 user message 改成 system prompt 前缀，看是否提高采纳率 | 半天 | — |
| 3 | 用云端机跑全量 105 case baseline vs critic-retry 对比 | 1 天 | 找 Sober 要机器 |
| 4 | error-mode-targeted hint 分桶（按 4 类难度 + 失败模式 tailored hint）| 2 天 | 全量结果 |
| 5 | critic 模型 ablation（deepseek-chat vs minimax-m2.5 vs qwen3coder）| 1 天 | 全量结果 |
| 6 | 把 critic-retry 集成进 Rust（`src/critic.rs` 自设计模块）| 3 天 | 上面跑通 |

### 与 sprint 的关系

姚老师 5/13 13:01 FutureX sprint 召集 @Sober @樵新宇 @梦醒时分（未点名我），但姚老师列的 7 个技术 lever 中 #2 "critic agent（Multiple hypothesis 验证后再 commit）"和我的设计同向。5/14 听陈哲文 research plan 后看能否把 critic-retry 并入 sprint 主路径，或作为团队 lever 之一被独立 measure。

---

## 五、文件清单（本次 commit 内容）

```
docs/2026-05-14-progress-report.md          # 本汇报文档
docs/2026-05-14-tree-advisor-design.md      # 设计文档（更详细，含未删的踩坑记录）
pawbench/run_with_critic.py                 # critic-retry runner（200 行 Python，自写）
pawbench/results/tree_baseline_5/           # baseline 5 case 结果
pawbench/results/tree_advisor_5/            # critic-retry v1 结果（含 critic-error 记录）
pawbench/results/tree_advisor_5_v2/         # critic-retry v2 结果（含 hint 内容）
.gitignore                                  # 保护 minipaw.json（含 API key）和 runtime state
```

源码 `src/` 一行没改，避免污染 main 分支后续合并。所有 advisor 能力都在 Python 层实现。

`pawbench/` 测试基础设施（cases/run.py/workspace/skills 等）未 commit 进 tree-advisor 分支，使用方式：

```bash
git clone <repo> && cd minipaw
git checkout tree-advisor
git checkout origin/advisor -- pawbench/   # 拉测试 fixture
cp /path/to/your/minipaw.json pawbench/workspace/  # 自己配 API key
cargo build --release
python3 pawbench/run.py --limit 5 --name baseline_5         # baseline
python3 pawbench/run_with_critic.py --limit 5 --name critic_5  # critic-retry
```

需要老师 / 余樵 / 陈哲文反馈：
- (a) Trajectory-Critic Retry 思路是否值得继续做，还是合入 sprint #2 critic agent lever
- (b) 全量 105 case 跑结果出来前，5 case demo 数字够不够定方向
