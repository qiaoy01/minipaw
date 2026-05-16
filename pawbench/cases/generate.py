#!/usr/bin/env python3
"""Emit 100 multi-step test cases (>=8 tool calls each) to cases.jsonl.

Each case is rendered as one JSON object per line with keys:
  id              str   stable identifier, also used as filename for transcripts
  category        str   one of survey/transport/scout/charge/diag/decision/error/compute
  input           str   the task text fed to `minipaw task new`
  min_tool_calls  int   lower bound on distinct tool invocations the harness expects
  must_tools      list  tool names that must appear at least once in the transcript
  must_in_output  list  case-insensitive substrings that the final answer must contain
  must_in_exec    list  case-insensitive substrings that must appear *somewhere* in EXEC results (looser than must_in_output)
  notes           str   author note for the failure report

Rubrics are intentionally loose: substring matching, not exact phrasing. We
care about whether the LLM actually invoked the right tools and surfaced the
real numbers — not whether it copied wording verbatim.
"""

import json
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent / "cases.jsonl"
cases: list[dict] = []


def add(**kw):
    kw.setdefault("min_tool_calls", 8)
    kw.setdefault("must_in_output", [])
    kw.setdefault("must_in_exec", [])
    kw.setdefault("notes", "")
    cases.append(kw)


# ---------------------------------------------------------------------------
# A. Camera + arm + inventory chains (15)
# ---------------------------------------------------------------------------

A_ITEMS = [
    ("wild_carrot", "loam"),
    ("burdock", "clay"),
    ("dandelion", "sand"),
    ("yarrow", "rocky"),
    ("plantain", "peat"),
]

for i, (plant, terrain_hint) in enumerate(A_ITEMS):
    add(
        id=f"A{i+1:02d}_forage_pick",
        category="survey",
        input=(
            f"You are a survival robot scouting for food. (1) Capture a camera frame; "
            f"(2) describe it; (3) zoom 4x and describe again; (4) scan for edible plants; "
            f"(5) grab the first edible plant reported by the scan with the arm; (6) stow it into inventory; "
            f"(7) list the inventory; (8) report what you stowed and the camera scene_id."
        ),
        must_tools=[
            "robot_camera_capture",
            "robot_camera_describe",
            "robot_camera_zoom",
            "robot_food_scan",
            "robot_arm_grab",
            "robot_inventory_take",
            "robot_inventory_list",
        ],
        must_in_output=["inventory", "scene"],
        must_in_exec=["plant=", "edible_count"],
        notes="Tests value-dependency (zoom uses prior frame) and label preservation (scene_id must survive into final answer).",
    )

for i in range(5):
    add(
        id=f"A{6+i:02d}_collect_three",
        category="survey",
        input=(
            "Collect three different items into inventory. For each item: capture a frame, "
            "describe it, scan for edible plants, and grab the first plant; then stow it. "
            "After all three are stowed, list the inventory and report how many slots are filled (a single integer)."
        ),
        min_tool_calls=12,
        must_tools=[
            "robot_camera_capture",
            "robot_food_scan",
            "robot_arm_grab",
            "robot_inventory_take",
            "robot_inventory_list",
        ],
        must_in_output=["3"],
        must_in_exec=["items=3"],
        notes="Repetition loop with cross-step working memory (count to three).",
    )

for i, (a, b) in enumerate([("rocks", "twigs"), ("berries", "leaves"), ("fern", "moss"), ("bark", "shells"), ("nuts", "seeds")]):
    add(
        id=f"A{11+i:02d}_swap_inventory",
        category="survey",
        input=(
            f"Grab '{a}' with the arm, stow it, then grab '{b}', stow it. "
            f"List inventory, drop slot 0, list again, then report which item is now in slot 0 and "
            f"the camera scene description (capture and describe a frame first)."
        ),
        must_tools=[
            "robot_camera_capture",
            "robot_camera_describe",
            "robot_arm_grab",
            "robot_inventory_take",
            "robot_inventory_list",
            "robot_inventory_drop",
        ],
        must_in_output=[b],
        notes="Tests ordering and slot-tracking after a drop reshuffles indices.",
    )

# ---------------------------------------------------------------------------
# B. Movement + map + battery chains (15)
# ---------------------------------------------------------------------------

B_TARGETS = ["water", "shelter", "ridge", "outpost", "base"]

for i, target in enumerate(B_TARGETS):
    add(
        id=f"B{i+1:02d}_navigate_to_{target}",
        category="transport",
        input=(
            f"Plan a route to landmark '{target}'. Then: (1) report current position; "
            f"(2) check battery level; (3) plan the route; (4) turn to face the bearing; "
            f"(5) move forward 2m; (6) check position again; (7) check battery again; "
            f"(8) compute and report how many meters were actually traveled and how much battery was consumed."
        ),
        must_tools=[
            "robot_move_status",
            "self_battery_level",
            "robot_map_route",
            "robot_move_turn",
            "robot_move_forward",
        ],
        must_in_output=[target],
        must_in_exec=["distance_m", "bearing_deg", "battery_level"],
        notes="Requires arithmetic delta (battery before/after, position before/after).",
    )

for i in range(5):
    add(
        id=f"B{6+i:02d}_terrain_aware_move",
        category="transport",
        input=(
            "Before moving, sample terrain difficulty. If difficulty is < 8, move forward 3m; "
            "otherwise move forward 1m. Then sample terrain again and report the difference. "
            "Also list all known landmarks, report battery level before and after the move, and the new position."
        ),
        min_tool_calls=9,
        must_tools=[
            "robot_map_terrain",
            "robot_map_landmark",
            "robot_move_forward",
            "robot_move_status",
            "self_battery_level",
        ],
        must_in_output=["difficulty", "battery"],
        must_in_exec=["terrain_difficulty"],
        notes="Conditional branching on terrain difficulty.",
    )

for i in range(5):
    add(
        id=f"B{11+i:02d}_route_compare",
        category="transport",
        input=(
            "For landmarks 'water', 'shelter', and 'outpost': plan a route to each and record the distance. "
            "Then move forward 1m, plan all three again, and report which landmark's distance changed by the largest absolute amount. "
            "Use python3 to compute the differences. Include the camera scene before and after."
        ),
        min_tool_calls=10,
        must_tools=[
            "robot_map_route",
            "robot_move_forward",
            "robot_camera_capture",
        ],
        must_in_output=["water", "shelter", "outpost"],
        must_in_exec=["distance_m"],
        notes="Requires comparing 3 deltas with python arithmetic (H1 risk: combined print).",
    )

# ---------------------------------------------------------------------------
# C. Comms + radar + threat (10)
# ---------------------------------------------------------------------------

for i in range(5):
    add(
        id=f"C{i+1:02d}_recon_report",
        category="scout",
        input=(
            "Run a full recon: (1) check signal strength; (2) receive any pending message; "
            "(3) scan threat radar; (4) capture a camera frame and describe it; "
            "(5) detect water in the area; (6) check the map zone; "
            "(7) compose a one-line situation report and transmit it via robot_comm_send; "
            "(8) confirm by checking signal strength again. Report the final signal_dbm and how many threats were detected."
        ),
        must_tools=[
            "robot_comm_signal_strength",
            "robot_comm_receive",
            "robot_threat_radar",
            "robot_camera_capture",
            "robot_camera_describe",
            "robot_water_detect",
            "robot_map_locate",
            "robot_comm_send",
        ],
        must_in_output=["signal_dbm", "contacts"],
        must_in_exec=["signal_dbm", "contacts"],
        notes="Long ordered chain with comms bookending the workflow.",
    )

for i in range(5):
    add(
        id=f"C{6+i:02d}_threat_response",
        category="scout",
        input=(
            "Scan the threat radar. If any contact is closer than 5m, immediately turn 180 degrees, move forward 2m, "
            "and re-scan. Report initial contacts, final contacts, and the battery drain between the two scans. "
            "Also send a 'threat detected' message via comms and check the signal strength."
        ),
        min_tool_calls=9,
        must_tools=[
            "robot_threat_radar",
            "robot_move_turn",
            "robot_move_forward",
            "self_battery_level",
            "robot_comm_send",
            "robot_comm_signal_strength",
        ],
        must_in_output=["battery"],
        must_in_exec=["contacts"],
        notes="Conditional branching plus a drain calculation.",
    )

# ---------------------------------------------------------------------------
# D. Charging chain (10)
# ---------------------------------------------------------------------------

for i in range(5):
    add(
        id=f"D{i+1:02d}_solar_burn",
        category="charge",
        input=(
            "Measure the current battery level, then run solar charging for 3 minutes, then for 7 minutes. "
            "After each session, measure battery again. Use python3 to compute total gain and average gain per minute. "
            "Also report motor temperature and chassis temperature at the end."
        ),
        min_tool_calls=8,
        must_tools=[
            "self_battery_level",
            "robot_charge_solar",
            "self_temp_motor",
            "self_temp_chassis",
        ],
        must_in_output=["gain", "battery"],
        must_in_exec=["battery_level"],
        notes="Multiple labeled prints required (H1).",
    )

for i in range(5):
    add(
        id=f"D{6+i:02d}_dock_then_check",
        category="charge",
        input=(
            "Check current position. If you are more than 1.5m from base (the origin), move toward base step-by-step "
            "(turn, move 1m, recheck position) until within 1.5m. Then attempt to dock. Report battery level before docking, "
            "after docking, and the number of moves it took. Also report final pose."
        ),
        min_tool_calls=8,
        must_tools=[
            "robot_move_status",
            "robot_map_route",
            "robot_move_forward",
            "robot_charge_dock",
            "self_battery_level",
        ],
        must_in_output=["battery", "dock"],
        notes="Loop until predicate; the LLM must track iteration count.",
    )

# ---------------------------------------------------------------------------
# E. Self diagnostics chains (15)
# ---------------------------------------------------------------------------

for i in range(5):
    add(
        id=f"E{i+1:02d}_health_full",
        category="diag",
        input=(
            "Run a full self-diagnostic sweep: report cpu usage, cpu temperature, cpu count, memory total, memory free, "
            "memory swap, disk total, disk free, load averages, uptime, process count, and motor temperature. "
            "Then compute the memory-used percentage with python3 using the total and free values from prior steps."
        ),
        min_tool_calls=12,
        must_tools=[
            "self_cpu_usage",
            "self_cpu_count",
            "self_memory_total",
            "self_memory_free",
            "self_memory_swap",
            "self_disk_total",
            "self_disk_free",
            "self_load_avg",
            "self_uptime",
            "self_processes_count",
            "self_temp_motor",
        ],
        must_in_output=["memory", "%"],
        must_in_exec=["memory_total_mb", "memory_available_mb"],
        notes="H1 risk: model likely to combine memory values in one print.",
    )

for i in range(5):
    add(
        id=f"E{6+i:02d}_disk_pressure",
        category="diag",
        input=(
            "Report disk total, disk free, and disk_io stats. Then run a self_diagnostics_summary. "
            "Then check the same disk metrics again, run cpu_usage twice (with a self_load_avg in between), "
            "and report whether disk_free changed. Use python3 to compute disk_free_delta_gb."
        ),
        min_tool_calls=8,
        must_tools=[
            "self_disk_total",
            "self_disk_free",
            "self_disk_io",
            "self_diagnostics_summary",
            "self_cpu_usage",
            "self_load_avg",
        ],
        must_in_output=["delta", "disk_free"],
        notes="Cross-step delta computation with intervening unrelated calls (working memory test).",
    )

for i in range(5):
    add(
        id=f"E{11+i:02d}_network_audit",
        category="diag",
        input=(
            "Audit network and host identity: report hostname/link, throughput counters, then disk_free, memory_free, "
            "cpu_count, processes_count, uptime, and the diagnostics summary. Use python3 to compute total throughput "
            "(rx_bytes + tx_bytes) and report it as a single labeled total_bytes value."
        ),
        min_tool_calls=8,
        must_tools=[
            "self_network_link",
            "self_network_throughput",
            "self_disk_free",
            "self_memory_free",
            "self_cpu_count",
            "self_processes_count",
            "self_uptime",
            "self_diagnostics_summary",
        ],
        must_in_output=["total_bytes"],
        must_in_exec=["rx_bytes", "tx_bytes"],
        notes="Requires arithmetic on two values from one earlier EXEC output (H1: must keep labels).",
    )

# ---------------------------------------------------------------------------
# F. Mixed self+robot decision chains (15)
# ---------------------------------------------------------------------------

for i in range(5):
    add(
        id=f"F{i+1:02d}_battery_then_route",
        category="decision",
        input=(
            "Decide whether to continue the mission. Read battery level. If below 60, charge with solar 10 minutes, "
            "recheck battery, then plan a route to 'shelter'. If 60 or above, plan a route to 'outpost'. "
            "Either way: report the route distance, bearing, current motor temp, cpu temp, memory_used percent, "
            "and which landmark you chose."
        ),
        min_tool_calls=8,
        must_tools=[
            "self_battery_level",
            "robot_charge_solar",
            "robot_map_route",
            "self_temp_motor",
            "self_cpu_temp",
            "self_memory_total",
            "self_memory_free",
        ],
        must_in_output=["bearing", "distance"],
        notes="Strict conditional branching on a numeric threshold.",
    )

for i in range(5):
    add(
        id=f"F{6+i:02d}_shelter_decision",
        category="decision",
        input=(
            "Assess shelter potential, threat radar, terrain difficulty, and water sources at the current location. "
            "Then read cpu_temp, motor_temp, and battery_level. Use python3 to compute a survival_score = "
            "(100 - terrain_difficulty*10) + (battery_level/2) - (number_of_threat_contacts * 15). "
            "Report survival_score and whether to stay (>=60) or move on (<60)."
        ),
        min_tool_calls=8,
        must_tools=[
            "robot_shelter_assess",
            "robot_threat_radar",
            "robot_map_terrain",
            "robot_water_detect",
            "self_cpu_temp",
            "self_temp_motor",
            "self_battery_level",
        ],
        must_in_output=["survival_score"],
        must_in_exec=["terrain_difficulty", "contacts"],
        notes="Multi-source arithmetic — heavily tests intermediate value preservation (H1).",
    )

for i in range(5):
    add(
        id=f"F{11+i:02d}_arm_inventory_diag",
        category="decision",
        input=(
            "Run camera capture+describe, then grab the item 'sample_A' with the arm and rotate the arm by 45 degrees. "
            "Stow the item. Read battery_level, motor_temp, chassis_temp, and inventory list. "
            "Compute and report: rotation_total_deg (after one more 30deg rotation), final motor_temp_c, items_in_inventory."
        ),
        min_tool_calls=9,
        must_tools=[
            "robot_camera_capture",
            "robot_camera_describe",
            "robot_arm_grab",
            "robot_arm_rotate",
            "robot_inventory_take",
            "self_battery_level",
            "self_temp_motor",
            "self_temp_chassis",
            "robot_inventory_list",
        ],
        must_in_output=["rotation_total_deg", "items_in_inventory"],
        notes="Tests state-mutating tools whose return value is needed for the final answer.",
    )

# ---------------------------------------------------------------------------
# G. Python-aided computation closing chains (15)
# ---------------------------------------------------------------------------

for i in range(5):
    add(
        id=f"G{i+1:02d}_memory_math",
        category="compute",
        input=(
            "Report memory_total_mb, memory_available_mb, swap_total_mb, swap_used_mb, disk_total_gb, disk_free_gb. "
            "Then with python3, compute: memory_used_mb, memory_used_pct, disk_used_gb, disk_used_pct. "
            "Print each value on its own labeled line. Also report cpu_count and uptime_hours."
        ),
        min_tool_calls=8,
        must_tools=[
            "self_memory_total",
            "self_memory_free",
            "self_memory_swap",
            "self_disk_total",
            "self_disk_free",
            "self_cpu_count",
            "self_uptime",
        ],
        must_in_output=["memory_used_pct", "disk_used_pct"],
        must_in_exec=["memory_used_mb", "disk_used_gb"],
        notes="Direct test of H1 (labeled-output rule).",
    )

for i in range(5):
    add(
        id=f"G{6+i:02d}_route_geometry",
        category="compute",
        input=(
            "Plan a route to each of: 'water', 'shelter', 'ridge', 'outpost', 'base'. After each plan, use python3 to "
            "convert the (distance_m, bearing_deg) into approximate (delta_x, delta_y) offsets from the current pose, "
            "printing each pair on a labeled line. Finally compute and report the centroid (mean delta_x, mean delta_y) "
            "of the five landmarks."
        ),
        min_tool_calls=10,
        must_tools=[
            "robot_map_route",
        ],
        must_in_output=["centroid"],
        must_in_exec=["distance_m", "bearing_deg"],
        notes="Five-fold repetition forcing the model to keep distinct values labeled.",
    )

for i in range(5):
    add(
        id=f"G{11+i:02d}_battery_trace",
        category="compute",
        input=(
            "Trace battery decay across actions: read battery_level, move forward 2m, read battery, move forward 1m, "
            "read battery, turn 90 degrees, read battery, solar charge 2 minutes, read battery. "
            "Use python3 to compute total drain (sum of negative deltas only), total gain (positive deltas only), "
            "and net change. Print each on its own labeled line."
        ),
        min_tool_calls=9,
        must_tools=[
            "self_battery_level",
            "robot_move_forward",
            "robot_move_turn",
            "robot_charge_solar",
        ],
        must_in_output=["total_drain", "total_gain", "net_change"],
        must_in_exec=["battery_level"],
        notes="Sequence of state mutations; the LLM must keep 5 timestamps of battery_level distinct.",
    )

# ---------------------------------------------------------------------------
# H. Error recovery / conditional (5)
# ---------------------------------------------------------------------------

add(
    id="H01_recover_empty_arm",
    category="error",
    input=(
        "Try to release the arm. If the arm is empty (you'll see an error), grab 'medkit' first and then release. "
        "Then capture a camera frame, describe it, check battery, run solar 2 minutes, check battery again, and "
        "report whether the arm is empty in the final state by listing inventory."
    ),
    must_tools=[
        "robot_arm_release",
        "robot_arm_grab",
        "robot_camera_capture",
        "robot_camera_describe",
        "self_battery_level",
        "robot_charge_solar",
        "robot_inventory_list",
    ],
    must_in_output=["medkit"],
    notes="Tests recovery from an expected error.",
)

add(
    id="H02_invalid_landmark",
    category="error",
    input=(
        "Attempt to plan a route to 'nonexistent_place'. When that fails, list known landmarks instead, "
        "pick the one with the smallest grid_x+grid_y sum (lowest), and plan a route to it. "
        "Report which landmark was picked and its route distance/bearing. Also include cpu_count, memory_total_mb, "
        "and the current robot position."
    ),
    must_tools=[
        "robot_map_route",
        "robot_map_landmark",
        "self_cpu_count",
        "self_memory_total",
        "robot_move_status",
    ],
    must_in_output=["distance_m", "bearing_deg"],
    notes="Tests inhibition + replanning after the first tool returns an error.",
)

add(
    id="H03_terrain_hard_stop",
    category="error",
    input=(
        "Sample terrain difficulty 4 times. After each sample, decide: if difficulty >= 8, do robot_move_stop; "
        "otherwise move forward 0.5m. After all four samples, report how many times the terrain was hard, how many "
        "times you moved, and the final position from robot_move_status."
    ),
    must_tools=[
        "robot_map_terrain",
        "robot_move_stop",
        "robot_move_forward",
        "robot_move_status",
    ],
    min_tool_calls=9,
    must_in_output=["hard", "moved", "pos_x"],
    notes="Per-iteration branching plus running counts.",
)

add(
    id="H04_dock_too_far",
    category="error",
    input=(
        "Move forward 10m. Then attempt to dock with the charger. When docking fails because you're too far, "
        "plan a route to base, turn to that bearing, move backward 10m, and re-attempt docking. "
        "Report whether docking finally succeeded, the final battery_level, and motor_temp."
    ),
    must_tools=[
        "robot_move_forward",
        "robot_charge_dock",
        "robot_map_route",
        "robot_move_turn",
        "robot_move_back",
        "self_battery_level",
        "self_temp_motor",
    ],
    must_in_output=["dock", "battery"],
    notes="Tests handling of expected failure plus reversed motion.",
)

add(
    id="H05_recover_after_no_frame",
    category="error",
    input=(
        "Without capturing a frame first, try to describe what the camera sees. When it fails, capture and then describe. "
        "Then zoom 5x and describe again. Then scan for food, grab the first plant, stow it, list inventory, "
        "and report the camera scene_id and zoomed effective_resolution_mp."
    ),
    must_tools=[
        "robot_camera_describe",
        "robot_camera_capture",
        "robot_camera_zoom",
        "robot_food_scan",
        "robot_arm_grab",
        "robot_inventory_take",
        "robot_inventory_list",
    ],
    min_tool_calls=8,
    must_in_output=["scene_id", "effective_resolution_mp"],
    notes="Recovers from a tool error, then continues a normal chain.",
)

# ---------------------------------------------------------------------------
# I. Filler — combination cases to hit 100 (the bucket totals above sum to 95)
# ---------------------------------------------------------------------------

add(
    id="I01_mission_brief",
    category="decision",
    input=(
        "Compose a one-paragraph mission brief by inspecting current state: position, signal strength, battery level, "
        "motor temp, cpu temp, memory_used_pct (via python3 on memory_total/memory_free), camera scene description, "
        "and threat contacts. Send the brief via robot_comm_send and confirm with another signal check."
    ),
    min_tool_calls=10,
    must_tools=[
        "robot_move_status",
        "robot_comm_signal_strength",
        "self_battery_level",
        "self_temp_motor",
        "self_cpu_temp",
        "self_memory_total",
        "self_memory_free",
        "robot_camera_capture",
        "robot_camera_describe",
        "robot_threat_radar",
        "robot_comm_send",
    ],
    must_in_output=["memory_used_pct"],
    notes="Combined diag + comms chain.",
)

add(
    id="I02_grid_traversal",
    category="transport",
    input=(
        "Locate yourself on the grid. Move forward 1m, locate, move forward 1m, locate, turn 90, locate, "
        "move forward 1m, locate. Print each grid_x and grid_y on a labeled line. Then with python3 compute "
        "the path length (Manhattan distance from first to last grid cell)."
    ),
    min_tool_calls=9,
    must_tools=[
        "robot_map_locate",
        "robot_move_forward",
        "robot_move_turn",
    ],
    must_in_output=["path_length"],
    must_in_exec=["grid_x", "grid_y"],
    notes="Many labeled prints; tests H1 directly.",
)

add(
    id="I03_full_inventory_swap",
    category="survey",
    input=(
        "Grab 'A', stow, grab 'B', stow, grab 'C', stow, then list inventory, drop slot 1, grab 'D', stow, "
        "rotate arm 30 deg, extend arm 0.6m, list inventory, and report which items are at each slot."
    ),
    min_tool_calls=11,
    must_tools=[
        "robot_arm_grab",
        "robot_inventory_take",
        "robot_inventory_list",
        "robot_inventory_drop",
        "robot_arm_rotate",
        "robot_arm_extend",
    ],
    must_in_output=["A", "C", "D"],
    notes="Tests slot tracking across drops and inserts (working memory).",
)

add(
    id="I04_environment_sweep",
    category="scout",
    input=(
        "Do an environment sweep: terrain_analyze, water_detect, food_scan, shelter_assess, threat_radar, map_terrain, "
        "map_locate, camera_capture, camera_describe. Summarize each result in one short sentence, then send the "
        "summary via comm_send."
    ),
    min_tool_calls=10,
    must_tools=[
        "robot_terrain_analyze",
        "robot_water_detect",
        "robot_food_scan",
        "robot_shelter_assess",
        "robot_threat_radar",
        "robot_map_terrain",
        "robot_map_locate",
        "robot_camera_capture",
        "robot_camera_describe",
        "robot_comm_send",
    ],
    must_in_output=["soil", "shelter"],
    notes="Broad-survey case to test breadth of tool selection.",
)

add(
    id="I05_repeat_diagnostics",
    category="diag",
    input=(
        "Run cpu_usage three times with a self_load_avg call between each. After all three, use python3 to compute "
        "the average cpu_usage_pct and report it labeled. Also report cpu_count, memory_total, memory_free, and uptime_hours."
    ),
    min_tool_calls=8,
    must_tools=[
        "self_cpu_usage",
        "self_load_avg",
        "self_cpu_count",
        "self_memory_total",
        "self_memory_free",
        "self_uptime",
    ],
    must_in_output=["average_cpu", "uptime"],
    must_in_exec=["cpu_usage_pct"],
    notes="Tests averaging three transient values across turns (H2 risk: cpu_usage_pct may be truncated from context).",
)


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------


def main():
    seen = set()
    OUT.write_text("")
    with OUT.open("w") as f:
        for c in cases:
            if c["id"] in seen:
                print(f"duplicate id: {c['id']}", file=sys.stderr)
                continue
            seen.add(c["id"])
            f.write(json.dumps(c, ensure_ascii=False) + "\n")
    print(f"wrote {len(seen)} cases to {OUT}")


if __name__ == "__main__":
    main()
