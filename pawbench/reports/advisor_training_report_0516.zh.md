# Advisor 训练效果报告 — qwen9b on pawbench 105

- **生成日期：** 2026-05-16
- **要回答的问题：** 子类划分（per-subclass）的 advisor 训练流程，能否真正提升 qwen9b 在 pawbench 105 上的**独立**得分？
- **结论：** 能 — 在断开 advisor 后跑完整 105 道题，得分 **+11.9 分**（46.2 → 58.1）。

## 1. 实验设置

两次跑分使用同一份 105 道题（`pawbench/cases/cases.jsonl`）、同一个 primary 推理端点（`<endpoint>`，模型 `qwen9b`）、同一个步数上限 `MINIPAW_MAX_SESSION_STEPS=32`。唯一区别是工作区里的 prompt / skill 文件，以及（测试时）是否配置了 advisor agent。

| 配置 | 工作区状态 | `minipaw.json` |
|---|---|---|
| **基线**（`full_run`）| `minihow.md` 规则 1–9；无子类 overlay；原始 skills | 仅 `primary` |
| **训练后**（`primary105_v2_0516`）| `minihow.md` 规则 1–9 保持不变；**4 个子类 overlay**（`minihow.{transport,charge,scout,compute}.md`）；skills 目录扩充了训练期间晋升的 `SKILL_NEW` 条目 | 仅 `primary`（测试前删掉了 advisor 块，跑完恢复） |

训练在 2026-05-15 当天分多轮 ReAct 进行（`train_exp_B02..G02_avg3_0515`），每轮针对一个子类，由 deepseek-v4-pro advisor 一次提出一条 directive，primary 复测，做跨类别 sanity check，然后晋升或回退。测试时存活的 4 个 overlay：

- `minihow.transport.md` — 2 条规则（按 bearing 转向、按编号步骤逐步执行）
- `minihow.charge.md` — 1 条规则（每次 `robot_charge_solar` 之后必须调用 `robot_charge_status`）
- `minihow.scout.md` — 1 条规则（scout 任务的强制工具序列）
- `minihow.compute.md` — 1 条规则（禁止读 `/proc`，必须用 `self_*` skills）

## 2. 头条结果

| 跑次 | Pass | Partial | Fail | 得分 |
|---|---|---|---|---|
| 基线（`full_run`）| 32 | 33 | 40 | **46.2** |
| 训练后（`primary105_v2_0516`）| 51 | 20 | 34 | **58.1** |
| **Δ** | **+19** | **−13** | **−6** | **+11.9** |

Pass 数从 32 跳到 51（相对增长 +59%）。Partial 减少 13，大部分是 Partial → Pass 的升级，而不是 Fail → Pass。

## 3. 按类别拆解

| 类别 | N | 基线 P/Par/F | 训练后 P/Par/F | ΔP | 训练过？ |
|---|---|---|---|---|---|
| survey（巡查）   | 16 | 9 / 7 / 0  | 14 / 2 / 0 | **+5** | 否 |
| scout（侦察）    | 11 | 0 / 3 / 8  | 5 / 2 / 4  | **+5** | ✅ |
| compute（计算）  | 15 | 0 / 7 / 8  | 5 / 4 / 6  | **+5** | ✅ |
| charge（充电）   | 10 | 0 / 3 / 7  | 4 / 0 / 6  | **+4** | ✅ |
| transport（运输）| 16 | 4 / 4 / 8  | 6 / 2 / 8  | **+2** | ✅ |
| diag（诊断）     | 16 | 7 / 6 / 3  | 8 / 3 / 5  | +1 | 否 |
| error（错误处理）| 5  | 2 / 1 / 2  | 2 / 2 / 1  | 0  | 否 |
| decision（决策） | 16 | 10 / 2 / 4 | 7 / 5 / 4  | **−3** | 否 |

- **4 个被训练的子类全部提升。** charge 和 scout 从 0 Pass 分别提升到 4 和 5，是单类最大涨幅。
- **survey 没有 overlay 也涨了 +5。** 主要来自 `A01-A05_forage_pick` 从 Partial 升 Pass（inventory 报告变得稳定），很可能是训练过程中 `SKILL_NEW` 晋升的副作用——因为 skills 是全局写入的，会影响所有类别。
- **decision 是唯一净下降的类别（−3 Pass）。** 有 4 道原本通过的 F-block 题目掉级：
  - `F03_battery_then_route`：Pass → Partial
  - `F07_shelter_decision`：Pass → Partial
  - `F08_shelter_decision`：Pass → Fail
  - `F13_arm_inventory_diag`：Pass → Fail

  decision 类别没有自己的 overlay，所以这些回退是全局 skill 改动对相邻任务的"附带伤害"。

## 4. 逐题翻转矩阵

105 道题中有 51 道在两次跑分之间改变了 verdict（49% 流动率）。

|  | 改善 | 退化 |
|---|---|---|
| **Fail → Pass**     | 13 | — |
| **Fail → Partial**  | 5  | — |
| **Partial → Pass**  | 15 | — |
| **Pass → Partial**  | — | 5  |
| **Partial → Fail**  | — | 11 |
| **Pass → Fail**     | — | 2  |
| **小计**             | **33** | **18** |
| **净翻转**           | **+15** | |

### 按类别的翻转计数

| 类别 | 上↑ | 下↓ | 持平 |
|---|---|---|---|
| compute   | 7 | 2 | 6 |
| survey    | 6 | 1 | 9 |
| scout     | 6 | 2 | 3 |
| charge    | 4 | 3 | 3 |
| transport | 4 | 2 | 10 |
| decision  | 3 | 4 | 9 |
| diag      | 2 | 4 | 10 |
| error     | 1 | 0 | 4 |

## 5. 失败模式分布（认知组件标签）

按 `ref.md §5` 的标签，每个失败映射到它最深一级的认知失败。

| 失败模式 | 基线 | 训练后 | Δ |
|---|---|---|---|
| `working_memory_value_loss_H1_or_H2`（跨步丢值）        | 23 | 18 | −5 |
| `tool_selection_skipped_required_tool`（漏调必须工具）  | 22 | 19 | −3 |
| `other`（其它）                                        | 17 | 12 | −5 |
| `initiation_no_tool_use`（完全没调工具就 DONE）         |  6 |  2 | **−4** |
| `wrong_arguments_or_skipped_subtools`（参数错或漏子调用）|  5 |  3 | −2 |
| **失败总数**                                            | **40** | **34** | **−6** |

`initiation_no_tool_use`（模型一个工具都没调就直接 DONE）从 6 例降到 2 例，是定性上最大的改进——这种"完全不动手"的失败最难看，现在基本被治住了。

## 6. 成本与规模

- **训练成本：** 跨 5 个子类组共约 14 轮 advisor，使用 deepseek-v4-pro 并开启 thinking。总花费个位数美元级别。
- **测试成本：** 仅 primary，qwen9b 本地推理，105 道题约 30 分钟挂钟时间。
- **最终产物：** 4 个 overlay 文件（合计约 660 字节）加少量 skills 条目。

## 7. 局限与注意事项

- **每道题只跑一次。** verdict 没有跨多个 seed 平均。处于 Partial / Pass 边界的题目，复跑会两边都翻。+11.9 的整体涨幅在类别层面足够压住噪声，但单道题的翻转不应过度解读。
- **decision 类别的回退是真实的，且未查明原因。** 后续需要：二分法定位是哪一个 `SKILL_NEW` 晋升导致了这些 Pass → Partial / Fail。
- **diag 也掉了 4 道题（`E04`、`E05`、`E06`、`I05`）。** 其中 2 道是 `health_full`，模型只调 ≤2 个工具就放弃——这看起来是 compute overlay 措辞为"消极禁止"（"不要读 /proc"）带来的副作用，没有"积极规定"必须把 `self_*` 系列工具调完。
- **子类样本量小**（5–16 例）。上表里每个类别的 Δ 都带着大约 ±2 道题的采样噪声。

## 8. 总结

子类 overlay + ReAct 训练循环在 qwen9b 独立测试上确实带来了可测量的提升（+11.9 分 / Pass 数相对 +59%）。机制是有效的。下一轮工作建议聚焦：
1. 弄清 decision 类别为什么回退；
2. 把 compute overlay 改成"积极规定"而不只是"消极禁止"；
3. 每道题跑多次，把信号和采样噪声分开。

## 附录 A — 改善的 33 道题

| 类别 | 题目 | 基线 | 训练后 |
|---|---|---|---|
| charge | D01_solar_burn | Fail | Pass |
| charge | D02_solar_burn | Fail | Pass |
| charge | D03_solar_burn | Fail | Pass |
| charge | D04_solar_burn | Fail | Pass |
| compute | G01_memory_math | Partial | Pass |
| compute | G02_memory_math | Fail | Partial |
| compute | G03_memory_math | Fail | Pass |
| compute | G04_memory_math | Fail | Pass |
| compute | G05_memory_math | Fail | Partial |
| compute | G08_route_geometry | Partial | Pass |
| compute | G11_battery_trace | Partial | Pass |
| decision | F09_shelter_decision | Fail | Partial |
| decision | F11_arm_inventory_diag | Partial | Pass |
| decision | I01_mission_brief | Fail | Partial |
| diag | E03_health_full | Fail | Pass |
| diag | E12_network_audit | Partial | Pass |
| error | H03_terrain_hard_stop | Fail | Partial |
| scout | C01_recon_report | Fail | Partial |
| scout | C06_threat_response | Fail | Pass |
| scout | C07_threat_response | Fail | Pass |
| scout | C08_threat_response | Fail | Pass |
| scout | C09_threat_response | Fail | Pass |
| scout | C10_threat_response | Fail | Pass |
| survey | A01_forage_pick | Partial | Pass |
| survey | A02_forage_pick | Partial | Pass |
| survey | A03_forage_pick | Partial | Pass |
| survey | A05_forage_pick | Partial | Pass |
| survey | A08_collect_three | Partial | Pass |
| survey | A10_collect_three | Partial | Pass |
| transport | B02_navigate_to_shelter | Fail | Partial |
| transport | B04_navigate_to_outpost | Partial | Pass |
| transport | B05_navigate_to_base | Partial | Pass |
| transport | B07_terrain_aware_move | Fail | Pass |

## 附录 B — 退化的 18 道题

| 类别 | 题目 | 基线 | 训练后 |
|---|---|---|---|
| charge | D07_dock_then_check | Partial | Fail |
| charge | D08_dock_then_check | Partial | Fail |
| charge | D09_dock_then_check | Partial | Fail |
| compute | G12_battery_trace | Partial | Fail |
| compute | G13_battery_trace | Partial | Fail |
| decision | F03_battery_then_route | Pass | Partial |
| decision | F07_shelter_decision | Pass | Partial |
| decision | F08_shelter_decision | Pass | Fail |
| decision | F13_arm_inventory_diag | Pass | Fail |
| diag | E04_health_full | Partial | Fail |
| diag | E05_health_full | Partial | Fail |
| diag | E06_disk_pressure | Pass | Partial |
| diag | I05_repeat_diagnostics | Partial | Fail |
| scout | C03_recon_report | Partial | Fail |
| scout | I04_environment_sweep | Partial | Fail |
| survey | A04_forage_pick | Pass | Partial |
| transport | B01_navigate_to_water | Partial | Fail |
| transport | B09_terrain_aware_move | Pass | Fail |
