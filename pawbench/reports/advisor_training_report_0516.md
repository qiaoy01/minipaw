# Advisor Training Report — qwen9b on pawbench 105

- **Generated:** 2026-05-16
- **Question being answered:** Does the subclass-scoped advisor training loop actually improve qwen9b's *standalone* score on pawbench 105?
- **Short answer:** Yes — **+11.9 points** (46.2 → 58.1) on the full 105-case benchmark with the advisor disconnected at measurement time.

## 1. Setup

Both runs were measured against the same 105-case manifest (`pawbench/cases/cases.jsonl`), same primary endpoint (`<endpoint>`, model `qwen9b`), same `MINIPAW_MAX_SESSION_STEPS=32`. The only differences are the workspace prompt/skill state and (for measurement) whether an advisor agent was configured.

| Configuration | Workspace state | `minipaw.json` |
|---|---|---|
| **Baseline** (`full_run`) | `minihow.md` rules 1–9; no per-subclass overlays; stock skills | `primary` only |
| **After training** (`primary105_v2_0516`) | `minihow.md` rules 1–9 unchanged; **four per-subclass overlays** (`minihow.{transport,charge,scout,compute}.md`); skills directory expanded by training-promoted `SKILL_NEW` entries | `primary` only (advisor block removed for the measurement run, then restored) |

Training was performed across 2026-05-15 in multiple ReAct rounds (`train_exp_B02..G02_avg3_0515`), each round targeting one subclass with the deepseek-v4-pro advisor proposing one directive at a time, primary retesting, cross-category sanity check, and promote-or-revert. The 4 surviving overlays at measurement time were:

- `minihow.transport.md` — 2 rules (turn-to-bearing, transport-step execution)
- `minihow.charge.md` — 1 rule (always call `robot_charge_status` after `robot_charge_solar`)
- `minihow.scout.md` — 1 rule (mandatory tool sequence for scout tasks)
- `minihow.compute.md` — 1 rule (forbid `/proc` reads; use `self_*` skills)

## 2. Headline result

| Run | Pass | Partial | Fail | Score |
|---|---|---|---|---|
| Baseline (`full_run`) | 32 | 33 | 40 | **46.2** |
| After training (`primary105_v2_0516`) | 51 | 20 | 34 | **58.1** |
| **Δ** | **+19** | **−13** | **−6** | **+11.9** |

Pass count jumped from 32 to 51 (+59% relative). The Partial bucket shrank by 13 — most of the conversion was Partial → Pass, not Fail → Pass.

## 3. Per-category breakdown

| Category | N | Baseline P/Par/F | After P/Par/F | ΔP | Trained? |
|---|---|---|---|---|---|
| survey    | 16 | 9 / 7 / 0  | 14 / 2 / 0 | **+5** | no |
| scout     | 11 | 0 / 3 / 8  | 5 / 2 / 4  | **+5** | ✅ |
| compute   | 15 | 0 / 7 / 8  | 5 / 4 / 6  | **+5** | ✅ |
| charge    | 10 | 0 / 3 / 7  | 4 / 0 / 6  | **+4** | ✅ |
| transport | 16 | 4 / 4 / 8  | 6 / 2 / 8  | **+2** | ✅ |
| diag      | 16 | 7 / 6 / 3  | 8 / 3 / 5  | +1 | no |
| error     | 5  | 2 / 1 / 2  | 2 / 2 / 1  | 0  | no |
| decision  | 16 | 10 / 2 / 4 | 7 / 5 / 4  | **−3** | no |

- **All 4 trained subclasses improved.** Charge and scout went from 0 Pass to 4 and 5 respectively — the largest single-category gains.
- **Survey gained +5 despite having no overlay.** Most of these came from `A01-A05_forage_pick` flipping Partial → Pass (consistent inventory reporting), likely a side effect of `SKILL_NEW` promotions during training, which are written globally.
- **Decision is the only net regression (−3 Pass).** Four F-block cases that previously passed dropped a tier:
  - `F03_battery_then_route`: Pass → Partial
  - `F07_shelter_decision`: Pass → Partial
  - `F08_shelter_decision`: Pass → Fail
  - `F13_arm_inventory_diag`: Pass → Fail
  No decision overlay exists, so these regressions are collateral damage from global skill additions affecting model behavior on adjacent tasks.

## 4. Per-case flip matrix

51 of 105 cases changed verdict between runs (49% churn).

|  | Improved | Regressed |
|---|---|---|
| **Fail → Pass**     | 13 | — |
| **Fail → Partial**  | 5  | — |
| **Partial → Pass**  | 15 | — |
| **Pass → Partial**  | — | 5  |
| **Partial → Fail**  | — | 11 |
| **Pass → Fail**     | — | 2  |
| **Subtotal**        | **33** | **18** |
| **Net flips**       | **+15** | |

### Flip counts by category

| Category | Up | Down | Same |
|---|---|---|---|
| compute   | 7 | 2 | 6 |
| survey    | 6 | 1 | 9 |
| scout     | 6 | 2 | 3 |
| charge    | 4 | 3 | 3 |
| transport | 4 | 2 | 10 |
| decision  | 3 | 4 | 9 |
| diag      | 2 | 4 | 10 |
| error     | 1 | 0 | 4 |

## 5. Failure-mode distribution (cognitive components)

Per `ref.md §5`, each fail is mapped to its deepest cognitive failure.

| Failure mode | Baseline | After | Δ |
|---|---|---|---|
| `working_memory_value_loss_H1_or_H2`     | 23 | 18 | −5 |
| `tool_selection_skipped_required_tool`   | 22 | 19 | −3 |
| `other`                                  | 17 | 12 | −5 |
| `initiation_no_tool_use`                 |  6 |  2 | **−4** |
| `wrong_arguments_or_skipped_subtools`    |  5 |  3 | −2 |
| **Total fails**                          | **40** | **34** | **−6** |

`initiation_no_tool_use` (the model issuing DONE without calling any tool at all) was cut from 6 to 2 — likely the biggest qualitative win, since these are the most embarrassing failures.

## 6. Cost and scale

- **Training cost:** ~14 advisor rounds across 5 subclass groups, deepseek-v4-pro with thinking enabled. Multi-dollar API spend (single-digit USD).
- **Measurement cost:** primary-only, qwen9b local, ~30 min wall time for 105 cases.
- **Net artifacts kept:** 4 overlay files (~660 bytes total) plus a handful of skills.

## 7. Caveats

- **Single run per case.** Verdicts are not averaged over multiple seeds. Cases sitting near a Partial/Pass boundary will show up as flips in either direction across re-runs. The +11.9 swing is large enough to dominate noise at category level, but individual case flips should not be over-interpreted.
- **Decision regression is real and unexplained.** Worth a follow-up: bisect which `SKILL_NEW` promotion is responsible.
- **Diag also dropped 4 cases (`E04`, `E05`, `E06`, `I05`).** Two are `health_full` cases where the model issued ≤2 tool calls before giving up — this looks like a behavioral side effect of the compute overlay's negative phrasing ("never read /proc"), which may discourage tool use without affirmatively prescribing the `self_*` battery.
- **Sample sizes are small per subclass** (5–16 cases). Each per-category Δ above carries roughly ±2 cases of sampling noise.

## 8. Bottom line

The subclass-scoped overlay + ReAct training loop produces a real, measurable lift on the standalone qwen9b benchmark (+11.9 points / +59% relative Pass count). The mechanism works. The next round of work should focus on (a) understanding the decision regression, (b) tightening the compute overlay to be prescriptive rather than only prohibitive, and (c) running each case multiple times to separate signal from sampling noise.

## Appendix A — Improved cases (33)

| Category | Case | Baseline | After |
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

## Appendix B — Regressed cases (18)

| Category | Case | Baseline | After |
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
