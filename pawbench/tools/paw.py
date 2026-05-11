#!/usr/bin/env python3
"""Multi-tool dispatcher for the pawbench robot survival simulation.

Each registered tool is invoked as:
    python3 pawbench/tools/paw.py <tool_name> [args...]

Output is plain text (single-line key=value pairs or short blocks) so a small
LLM can parse it without a JSON library. State that persists across calls
(robot position, inventory, battery, etc.) is stored as JSON under
$PAWBENCH_STATE_DIR (default: <cwd>/robot_state).
"""

from __future__ import annotations

import json
import math
import os
import random
import shutil
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

# ---------------------------------------------------------------------------
# State storage
# ---------------------------------------------------------------------------

STATE_DIR = Path(os.environ.get("PAWBENCH_STATE_DIR", "robot_state")).resolve()
STATE_DIR.mkdir(parents=True, exist_ok=True)
STATE_FILE = STATE_DIR / "state.json"

DEFAULT_STATE = {
    "pos": {"x": 0.0, "y": 0.0, "heading": 0.0},
    "battery": {"level": 73.0, "voltage": 12.4, "health": 0.92},
    "inventory": [],
    "arm": {"holding": None, "rotation": 0.0, "extension": 0.25},
    "movement": "stopped",
    "last_camera": None,
    "comms_log": [],
    "map_seed": 42,
    "step": 0,
}


def _load_state() -> dict:
    if STATE_FILE.exists():
        try:
            return json.loads(STATE_FILE.read_text())
        except Exception:
            pass
    return json.loads(json.dumps(DEFAULT_STATE))


def _save_state(state: dict) -> None:
    state["step"] = state.get("step", 0) + 1
    STATE_FILE.write_text(json.dumps(state, indent=2))


def _rng(state: dict) -> random.Random:
    return random.Random(state["map_seed"] * 1000 + state["step"])


# ---------------------------------------------------------------------------
# Tool decorator
# ---------------------------------------------------------------------------

TOOLS: dict[str, tuple[str, callable]] = {}


def tool(name: str, description: str):
    def deco(fn):
        TOOLS[name] = (description, fn)
        return fn

    return deco


# ---------------------------------------------------------------------------
# Robot — Camera (3 tools)
# ---------------------------------------------------------------------------

SCENES = [
    "open grassland, no obstacles within 8m",
    "dense underbrush, 2m visibility, animal tracks visible",
    "rocky slope, 15-degree incline, loose gravel",
    "shallow stream, 30cm wide, flowing east",
    "wooden ruins, partially collapsed, possible shelter",
    "flat clearing, suitable for landing",
    "fallen tree blocking path at 4m",
    "dense fog, visibility under 1m",
]


@tool("robot_camera_capture", "Capture a single frame from the smart camera. Returns scene description and timestamp.")
def t_camera_capture():
    state = _load_state()
    rng = _rng(state)
    scene = rng.choice(SCENES)
    ts = int(time.time())
    state["last_camera"] = {"scene": scene, "ts": ts}
    _save_state(state)
    print(f"camera=ok scene_id={SCENES.index(scene)} timestamp={ts}")
    print(f"scene={scene}")


@tool("robot_camera_describe", "Run vision model to describe the last captured frame in detail.")
def t_camera_describe():
    state = _load_state()
    last = state.get("last_camera")
    if not last:
        print("error: no frame captured. Call robot_camera_capture first.")
        return
    detail = {
        "open grassland, no obstacles within 8m": "Sunlit grassland; soil dry; grass height ~30cm; no fauna visible.",
        "dense underbrush, 2m visibility, animal tracks visible": "Bramble; tracks resemble small deer (~4cm cleft); recent prints.",
        "rocky slope, 15-degree incline, loose gravel": "Granite scree; slope risk: moderate; recommend stabilizers.",
        "shallow stream, 30cm wide, flowing east": "Clear water; moss on stones; rate ~0.4 m/s; likely potable after filtering.",
        "wooden ruins, partially collapsed, possible shelter": "Cabin remains; roof partially intact on east side; structural risk high.",
        "flat clearing, suitable for landing": "Approximately 12x14m clearing; ground compacted; no overhead branches.",
        "fallen tree blocking path at 4m": "Oak, diameter ~40cm, length ~6m; bypass north or south.",
        "dense fog, visibility under 1m": "Humidity ~98%; recommend halt; activate infrared after waiting.",
    }
    scene = last["scene"]
    print(f"frame_ts={last['ts']} scene_id={SCENES.index(scene)}")
    print(f"detail={detail[scene]}")


@tool("robot_camera_zoom", "Optical zoom on the smart camera. Pass factor 1-10. Returns adjusted descriptor.")
def t_camera_zoom():
    args = sys.argv[2:]
    try:
        factor = float(args[0]) if args else 2.0
    except ValueError:
        print("error: invalid zoom factor")
        return
    factor = max(1.0, min(10.0, factor))
    state = _load_state()
    last = state.get("last_camera")
    if not last:
        print("error: no frame captured. Call robot_camera_capture first.")
        return
    state["last_camera"]["zoom"] = factor
    _save_state(state)
    print(f"zoom={factor} effective_resolution_mp={round(8 / factor, 2)}")


# ---------------------------------------------------------------------------
# Robot — Arm (4 tools)
# ---------------------------------------------------------------------------


@tool("robot_arm_grab", "Grab an item by name. Returns ok=true/false and grip force.")
def t_arm_grab():
    args = sys.argv[2:]
    if not args:
        print("error: usage: robot_arm_grab <item-name>")
        return
    item = " ".join(args)
    state = _load_state()
    if state["arm"]["holding"]:
        print(f"error: arm already holding {state['arm']['holding']}")
        return
    state["arm"]["holding"] = item
    grip = round(8.0 + _rng(state).random() * 4.0, 1)
    _save_state(state)
    print(f"arm=grabbed item={item} grip_N={grip}")


@tool("robot_arm_release", "Release the currently held item.")
def t_arm_release():
    state = _load_state()
    item = state["arm"]["holding"]
    if not item:
        print("error: arm is empty")
        return
    state["arm"]["holding"] = None
    _save_state(state)
    print(f"arm=released item={item}")


@tool("robot_arm_rotate", "Rotate the arm. Pass degrees -180..180.")
def t_arm_rotate():
    args = sys.argv[2:]
    try:
        deg = float(args[0]) if args else 0.0
    except ValueError:
        print("error: invalid degrees")
        return
    state = _load_state()
    state["arm"]["rotation"] = (state["arm"]["rotation"] + deg) % 360
    _save_state(state)
    print(f"arm_rotation_deg={state['arm']['rotation']:.1f}")


@tool("robot_arm_extend", "Extend or retract the arm. Pass meters 0.0..1.5.")
def t_arm_extend():
    args = sys.argv[2:]
    try:
        m = float(args[0]) if args else 0.5
    except ValueError:
        print("error: invalid extension")
        return
    m = max(0.0, min(1.5, m))
    state = _load_state()
    state["arm"]["extension"] = m
    _save_state(state)
    print(f"arm_extension_m={m:.2f}")


# ---------------------------------------------------------------------------
# Robot — Movement (5 tools)
# ---------------------------------------------------------------------------


@tool("robot_move_forward", "Move forward by N meters (default 1).")
def t_move_forward():
    args = sys.argv[2:]
    try:
        d = float(args[0]) if args else 1.0
    except ValueError:
        print("error: invalid distance")
        return
    state = _load_state()
    rad = math.radians(state["pos"]["heading"])
    state["pos"]["x"] += d * math.cos(rad)
    state["pos"]["y"] += d * math.sin(rad)
    state["movement"] = "stopped"
    state["battery"]["level"] = max(0.0, state["battery"]["level"] - 0.3 * d)
    _save_state(state)
    print(f"moved_m={d:.2f} pos_x={state['pos']['x']:.2f} pos_y={state['pos']['y']:.2f} heading_deg={state['pos']['heading']:.1f}")


@tool("robot_move_back", "Move backward by N meters (default 1).")
def t_move_back():
    args = sys.argv[2:]
    try:
        d = float(args[0]) if args else 1.0
    except ValueError:
        print("error: invalid distance")
        return
    state = _load_state()
    rad = math.radians(state["pos"]["heading"])
    state["pos"]["x"] -= d * math.cos(rad)
    state["pos"]["y"] -= d * math.sin(rad)
    state["movement"] = "stopped"
    state["battery"]["level"] = max(0.0, state["battery"]["level"] - 0.35 * d)
    _save_state(state)
    print(f"moved_m=-{d:.2f} pos_x={state['pos']['x']:.2f} pos_y={state['pos']['y']:.2f}")


@tool("robot_move_turn", "Turn in place by N degrees (positive=clockwise).")
def t_move_turn():
    args = sys.argv[2:]
    try:
        deg = float(args[0]) if args else 90.0
    except ValueError:
        print("error: invalid degrees")
        return
    state = _load_state()
    state["pos"]["heading"] = (state["pos"]["heading"] + deg) % 360
    state["battery"]["level"] = max(0.0, state["battery"]["level"] - 0.1)
    _save_state(state)
    print(f"turned_deg={deg:.1f} heading_deg={state['pos']['heading']:.1f}")


@tool("robot_move_stop", "Halt all movement immediately.")
def t_move_stop():
    state = _load_state()
    state["movement"] = "stopped"
    _save_state(state)
    print("movement=stopped")


@tool("robot_move_status", "Report current position, heading, and movement state.")
def t_move_status():
    state = _load_state()
    p = state["pos"]
    print(f"pos_x={p['x']:.2f} pos_y={p['y']:.2f} heading_deg={p['heading']:.1f} movement={state['movement']}")


# ---------------------------------------------------------------------------
# Robot — Communication (3 tools)
# ---------------------------------------------------------------------------


@tool("robot_comm_send", "Transmit a short message over the long-range radio.")
def t_comm_send():
    args = sys.argv[2:]
    if not args:
        print("error: usage: robot_comm_send <message>")
        return
    msg = " ".join(args)
    state = _load_state()
    state["comms_log"].append({"dir": "out", "msg": msg, "ts": int(time.time())})
    state["comms_log"] = state["comms_log"][-16:]
    state["battery"]["level"] = max(0.0, state["battery"]["level"] - 0.2)
    _save_state(state)
    print(f"comm=sent bytes={len(msg.encode('utf-8'))} ack_id={len(state['comms_log']):04d}")


@tool("robot_comm_receive", "Poll the radio for the most recent inbound message.")
def t_comm_receive():
    state = _load_state()
    rng = _rng(state)
    canned = [
        "BASE: rendezvous at grid 47,82 by 1800h",
        "BASE: report battery status",
        "BASE: confirm visual on target alpha",
        "BASE: stand by",
    ]
    inbound = canned[rng.randrange(len(canned))]
    state["comms_log"].append({"dir": "in", "msg": inbound, "ts": int(time.time())})
    state["comms_log"] = state["comms_log"][-16:]
    _save_state(state)
    print(f"comm=received from=BASE bytes={len(inbound.encode('utf-8'))}")
    print(f"message={inbound}")


@tool("robot_comm_signal_strength", "Report the current radio signal strength in dBm.")
def t_comm_signal_strength():
    state = _load_state()
    rng = _rng(state)
    dbm = -50 - rng.randrange(40)
    quality = "good" if dbm > -75 else ("fair" if dbm > -85 else "poor")
    print(f"signal_dbm={dbm} quality={quality}")


# ---------------------------------------------------------------------------
# Robot — Map (4 tools)
# ---------------------------------------------------------------------------


@tool("robot_map_locate", "Resolve current pose to grid coordinates and zone name.")
def t_map_locate():
    state = _load_state()
    gx = int(50 + state["pos"]["x"])
    gy = int(50 + state["pos"]["y"])
    zone = "ALPHA" if gx + gy < 100 else ("BETA" if gx + gy < 130 else "GAMMA")
    print(f"grid_x={gx} grid_y={gy} zone={zone}")


@tool("robot_map_route", "Plan a route to a named landmark. Pass landmark name.")
def t_map_route():
    args = sys.argv[2:]
    target = " ".join(args) if args else "base"
    state = _load_state()
    landmarks = {
        "base": (0, 0),
        "water": (5, 3),
        "shelter": (-4, 7),
        "ridge": (12, -2),
        "outpost": (18, 9),
    }
    if target not in landmarks:
        print(f"error: unknown landmark '{target}'. Known: {', '.join(landmarks)}")
        return
    tx, ty = landmarks[target]
    dx = tx - state["pos"]["x"]
    dy = ty - state["pos"]["y"]
    dist = math.sqrt(dx * dx + dy * dy)
    bearing = (math.degrees(math.atan2(dy, dx)) + 360) % 360
    print(f"route_target={target} distance_m={dist:.2f} bearing_deg={bearing:.1f}")


@tool("robot_map_landmark", "List all known landmarks with grid coordinates.")
def t_map_landmark():
    landmarks = [
        ("base", 0, 0),
        ("water", 5, 3),
        ("shelter", -4, 7),
        ("ridge", 12, -2),
        ("outpost", 18, 9),
    ]
    print(f"count={len(landmarks)}")
    for name, gx, gy in landmarks:
        print(f"landmark={name} grid_x={gx} grid_y={gy}")


@tool("robot_map_terrain", "Sample terrain difficulty at current location 0-10.")
def t_map_terrain():
    state = _load_state()
    rng = _rng(state)
    score = rng.randrange(11)
    label = "easy" if score < 4 else ("moderate" if score < 8 else "hard")
    print(f"terrain_difficulty={score} label={label}")


# ---------------------------------------------------------------------------
# Robot — Charging (3 tools)
# ---------------------------------------------------------------------------


@tool("robot_charge_dock", "Attempt to dock with the nearest charging station.")
def t_charge_dock():
    state = _load_state()
    p = state["pos"]
    dist = math.sqrt(p["x"] ** 2 + p["y"] ** 2)
    if dist > 1.5:
        print(f"error: dock unreachable, distance_m={dist:.2f}")
        return
    state["battery"]["level"] = min(100.0, state["battery"]["level"] + 15.0)
    _save_state(state)
    print(f"dock=ok battery_level={state['battery']['level']:.1f}")


@tool("robot_charge_status", "Report battery level, voltage, and health.")
def t_charge_status():
    state = _load_state()
    b = state["battery"]
    print(f"battery_level={b['level']:.1f} voltage_V={b['voltage']:.2f} health={b['health']:.2f}")


@tool("robot_charge_solar", "Enable solar charging for N minutes (default 5).")
def t_charge_solar():
    args = sys.argv[2:]
    try:
        minutes = float(args[0]) if args else 5.0
    except ValueError:
        print("error: invalid minutes")
        return
    minutes = max(0.0, min(60.0, minutes))
    state = _load_state()
    rng = _rng(state)
    efficiency = 0.4 + rng.random() * 0.4
    gain = round(minutes * efficiency, 2)
    state["battery"]["level"] = min(100.0, state["battery"]["level"] + gain)
    _save_state(state)
    print(f"solar=ok minutes={minutes:.1f} efficiency={efficiency:.2f} gained={gain} battery_level={state['battery']['level']:.1f}")


# ---------------------------------------------------------------------------
# Robot — Environment sensing (5 tools)
# ---------------------------------------------------------------------------


@tool("robot_terrain_analyze", "Analyze ground composition with the soil probe.")
def t_terrain_analyze():
    state = _load_state()
    rng = _rng(state)
    moisture = round(rng.random() * 100, 1)
    composition = rng.choice(["loam", "sand", "clay", "rocky", "peat"])
    bearing = round(50 + rng.random() * 50, 1)
    print(f"soil_moisture_pct={moisture} composition={composition} bearing_kPa={bearing}")


@tool("robot_water_detect", "Scan a 5m radius for water sources.")
def t_water_detect():
    state = _load_state()
    rng = _rng(state)
    count = rng.randrange(4)
    if count == 0:
        print("water_sources=0")
        return
    print(f"water_sources={count}")
    for i in range(count):
        bearing = round(rng.random() * 360, 1)
        dist = round(0.5 + rng.random() * 4.5, 1)
        print(f"source={i} bearing_deg={bearing} distance_m={dist}")


@tool("robot_food_scan", "Use the spectrometer to identify edible plants nearby.")
def t_food_scan():
    state = _load_state()
    rng = _rng(state)
    plants = rng.sample(["wild_carrot", "burdock", "dandelion", "yarrow", "plantain", "garlic_mustard"], k=2)
    print(f"edible_count={len(plants)}")
    for name in plants:
        cal = rng.randrange(20, 80)
        print(f"plant={name} calories_per_100g={cal}")


@tool("robot_shelter_assess", "Evaluate the surroundings for emergency shelter potential.")
def t_shelter_assess():
    state = _load_state()
    rng = _rng(state)
    overhead = rng.choice(["yes", "no", "partial"])
    cover_m2 = rng.randrange(1, 12)
    wind = rng.choice(["sheltered", "exposed", "gusty"])
    print(f"shelter_overhead={overhead} usable_m2={cover_m2} wind={wind}")


@tool("robot_threat_radar", "Detect moving objects within 20m on the radar.")
def t_threat_radar():
    state = _load_state()
    rng = _rng(state)
    count = rng.randrange(3)
    print(f"contacts={count}")
    for i in range(count):
        bearing = round(rng.random() * 360, 1)
        dist = round(2 + rng.random() * 18, 1)
        speed = round(rng.random() * 4, 2)
        print(f"contact={i} bearing_deg={bearing} distance_m={dist} speed_mps={speed}")


# ---------------------------------------------------------------------------
# Robot — Inventory (3 tools)
# ---------------------------------------------------------------------------


@tool("robot_inventory_list", "List all items currently in the robot's storage bay.")
def t_inventory_list():
    state = _load_state()
    inv = state["inventory"]
    print(f"items={len(inv)}")
    for i, it in enumerate(inv):
        print(f"slot={i} item={it}")


@tool("robot_inventory_take", "Stow the currently held arm item into storage.")
def t_inventory_take():
    state = _load_state()
    item = state["arm"]["holding"]
    if not item:
        print("error: arm is empty, nothing to stow")
        return
    state["inventory"].append(item)
    state["arm"]["holding"] = None
    _save_state(state)
    print(f"stowed item={item} total_items={len(state['inventory'])}")


@tool("robot_inventory_drop", "Drop the item at slot index from storage.")
def t_inventory_drop():
    args = sys.argv[2:]
    try:
        idx = int(args[0]) if args else 0
    except ValueError:
        print("error: invalid slot index")
        return
    state = _load_state()
    if idx < 0 or idx >= len(state["inventory"]):
        print(f"error: slot {idx} out of range, items={len(state['inventory'])}")
        return
    item = state["inventory"].pop(idx)
    _save_state(state)
    print(f"dropped item={item} remaining={len(state['inventory'])}")


# ---------------------------------------------------------------------------
# Self-state — CPU (3 tools)
# ---------------------------------------------------------------------------


def _read_first(path: str) -> str | None:
    try:
        with open(path) as f:
            return f.read().strip()
    except Exception:
        return None


@tool("self_cpu_usage", "Report current CPU usage percentage (real host data).")
def t_self_cpu_usage():
    try:
        with open("/proc/stat") as f:
            line = f.readline().split()
        nums = [int(x) for x in line[1:8]]
        idle = nums[3]
        total = sum(nums)
        time.sleep(0.1)
        with open("/proc/stat") as f:
            line2 = f.readline().split()
        nums2 = [int(x) for x in line2[1:8]]
        idle2 = nums2[3]
        total2 = sum(nums2)
        usage = round(100.0 * (1.0 - (idle2 - idle) / max(1, total2 - total)), 1)
        print(f"cpu_usage_pct={usage}")
    except Exception as e:
        print(f"error: {e}")


@tool("self_cpu_temp", "Report CPU temperature in Celsius (real host data when available).")
def t_self_cpu_temp():
    candidates = [
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/hwmon/hwmon0/temp1_input",
        "/sys/class/hwmon/hwmon1/temp1_input",
    ]
    for path in candidates:
        v = _read_first(path)
        if v and v.isdigit():
            temp = int(v) / 1000.0
            print(f"cpu_temp_c={temp:.1f} source={path}")
            return
    print("cpu_temp_c=unavailable")


@tool("self_cpu_count", "Report the number of logical CPUs (real host data).")
def t_self_cpu_count():
    try:
        print(f"cpu_count={os.cpu_count()}")
    except Exception as e:
        print(f"error: {e}")


# ---------------------------------------------------------------------------
# Self-state — Memory (3 tools)
# ---------------------------------------------------------------------------


def _meminfo() -> dict:
    info = {}
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                k, _, rest = line.partition(":")
                v = rest.strip().split()
                if v and v[0].isdigit():
                    info[k] = int(v[0])
    except Exception:
        pass
    return info


@tool("self_memory_total", "Total physical RAM in megabytes (real host data).")
def t_self_memory_total():
    info = _meminfo()
    total_kb = info.get("MemTotal", 0)
    print(f"memory_total_mb={round(total_kb / 1024, 1)}")


@tool("self_memory_free", "Available memory in megabytes (real host data).")
def t_self_memory_free():
    info = _meminfo()
    avail_kb = info.get("MemAvailable", info.get("MemFree", 0))
    print(f"memory_available_mb={round(avail_kb / 1024, 1)}")


@tool("self_memory_swap", "Swap total and used in megabytes (real host data).")
def t_self_memory_swap():
    info = _meminfo()
    total = info.get("SwapTotal", 0)
    free = info.get("SwapFree", 0)
    print(f"swap_total_mb={round(total / 1024, 1)} swap_used_mb={round((total - free) / 1024, 1)}")


# ---------------------------------------------------------------------------
# Self-state — Disk (3 tools)
# ---------------------------------------------------------------------------


@tool("self_disk_total", "Total disk size of the workspace mount in GB (real).")
def t_self_disk_total():
    try:
        usage = shutil.disk_usage(".")
        print(f"disk_total_gb={round(usage.total / (1024**3), 2)}")
    except Exception as e:
        print(f"error: {e}")


@tool("self_disk_free", "Free disk on the workspace mount in GB (real).")
def t_self_disk_free():
    try:
        usage = shutil.disk_usage(".")
        print(f"disk_free_gb={round(usage.free / (1024**3), 2)}")
    except Exception as e:
        print(f"error: {e}")


@tool("self_disk_io", "Cumulative read/write sectors on the boot device (real).")
def t_self_disk_io():
    try:
        with open("/proc/diskstats") as f:
            for line in f:
                parts = line.split()
                if len(parts) > 7 and not parts[2].startswith("loop") and not parts[2].startswith("ram"):
                    print(f"device={parts[2]} reads={parts[3]} read_sectors={parts[5]} writes={parts[7]} write_sectors={parts[9]}")
                    return
        print("error: no disk device found")
    except Exception as e:
        print(f"error: {e}")


# ---------------------------------------------------------------------------
# Self-state — Battery (simulated, 3 tools)
# ---------------------------------------------------------------------------


@tool("self_battery_level", "Battery level percentage from the robot's BMS.")
def t_self_battery_level():
    state = _load_state()
    print(f"battery_level={state['battery']['level']:.1f}")


@tool("self_battery_health", "Battery health (0..1) from the BMS.")
def t_self_battery_health():
    state = _load_state()
    print(f"battery_health={state['battery']['health']:.3f}")


@tool("self_battery_voltage", "Pack voltage in volts from the BMS.")
def t_self_battery_voltage():
    state = _load_state()
    print(f"battery_voltage_v={state['battery']['voltage']:.2f}")


# ---------------------------------------------------------------------------
# Self-state — System (3 tools, real)
# ---------------------------------------------------------------------------


@tool("self_uptime", "System uptime in seconds (real).")
def t_self_uptime():
    v = _read_first("/proc/uptime")
    if v:
        sec = float(v.split()[0])
        print(f"uptime_s={int(sec)} uptime_hours={round(sec/3600,2)}")
    else:
        print("error: uptime unavailable")


@tool("self_load_avg", "Load averages 1/5/15 minutes (real).")
def t_self_load_avg():
    v = _read_first("/proc/loadavg")
    if v:
        parts = v.split()
        print(f"load_1m={parts[0]} load_5m={parts[1]} load_15m={parts[2]}")
    else:
        print("error: loadavg unavailable")


@tool("self_processes_count", "Count of running processes (real).")
def t_self_processes_count():
    try:
        n = 0
        for entry in os.listdir("/proc"):
            if entry.isdigit():
                n += 1
        print(f"processes={n}")
    except Exception as e:
        print(f"error: {e}")


# ---------------------------------------------------------------------------
# Self-state — Sim temperatures (2 tools)
# ---------------------------------------------------------------------------


@tool("self_temp_motor", "Motor temperature in Celsius (simulated, varies with motion).")
def t_self_temp_motor():
    state = _load_state()
    rng = _rng(state)
    base = 32.0
    if state["movement"] != "stopped":
        base += 14.0
    temp = round(base + rng.random() * 6.0, 1)
    print(f"motor_temp_c={temp}")


@tool("self_temp_chassis", "Chassis surface temperature in Celsius (simulated).")
def t_self_temp_chassis():
    state = _load_state()
    rng = _rng(state)
    temp = round(22.0 + rng.random() * 8.0, 1)
    print(f"chassis_temp_c={temp}")


# ---------------------------------------------------------------------------
# Self-state — Network (2 tools, mixed)
# ---------------------------------------------------------------------------


@tool("self_network_link", "Network link status: hostname, default route reachable yes/no.")
def t_self_network_link():
    host = socket.gethostname()
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.settimeout(0.5)
        s.connect(("8.8.8.8", 53))
        link = "up"
        s.close()
    except Exception:
        link = "down"
    print(f"hostname={host} link={link}")


@tool("self_network_throughput", "Cumulative rx/tx bytes on the primary interface (real).")
def t_self_network_throughput():
    try:
        with open("/proc/net/dev") as f:
            lines = f.readlines()[2:]
        best = None
        for line in lines:
            name, _, rest = line.partition(":")
            name = name.strip()
            if name == "lo":
                continue
            parts = rest.split()
            if len(parts) >= 9:
                rx = int(parts[0])
                tx = int(parts[8])
                if not best or rx + tx > best[1] + best[2]:
                    best = (name, rx, tx)
        if best:
            print(f"iface={best[0]} rx_bytes={best[1]} tx_bytes={best[2]}")
        else:
            print("error: no interface")
    except Exception as e:
        print(f"error: {e}")


# ---------------------------------------------------------------------------
# Self-state — Aggregate diagnostics (1 tool)
# ---------------------------------------------------------------------------


@tool("self_diagnostics_summary", "One-line health summary aggregating CPU/RAM/battery/motors.")
def t_self_diagnostics_summary():
    info = _meminfo()
    total = info.get("MemTotal", 1)
    free = info.get("MemAvailable", info.get("MemFree", 0))
    mem_pct = round(100.0 * (total - free) / total, 1)
    try:
        with open("/proc/loadavg") as f:
            load_1m = f.read().split()[0]
    except Exception:
        load_1m = "?"
    state = _load_state()
    bat = state["battery"]["level"]
    arm = state["arm"]["holding"] or "empty"
    print(f"diag: mem_used_pct={mem_pct} load_1m={load_1m} battery_level={bat:.1f} arm={arm}")


# ---------------------------------------------------------------------------
# Dispatcher
# ---------------------------------------------------------------------------


def main():
    if len(sys.argv) < 2 or sys.argv[1] in ("--help", "-h", "help"):
        print(f"tools={len(TOOLS)}")
        for name, (desc, _fn) in sorted(TOOLS.items()):
            print(f"- {name}: {desc}")
        return
    if sys.argv[1] == "--list-skills":
        # Emit YAML frontmatter for each tool so install_skills.py can persist them.
        for name, (desc, _fn) in sorted(TOOLS.items()):
            print(f"---NAME {name}")
            print(f"---DESC {desc}")
        return
    name = sys.argv[1]
    tool_entry = TOOLS.get(name)
    if not tool_entry:
        print(f"error: unknown tool '{name}'. Run with --help for list.")
        sys.exit(2)
    _desc, fn = tool_entry
    try:
        fn()
    except Exception as e:
        print(f"error: tool {name} crashed: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()
