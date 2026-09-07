#!/usr/bin/env python3
"""Measure an instrumented release binary using disposable state and SQLite rows.

Usage: python3 docs/benchmarks/session-restoration.py /absolute/path/to/suru
Requires the `Session restoration completed` debug event added for #279.
Uses only the Python standard library; no user database or config is touched.
"""

import json
import os
from pathlib import Path
import re
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid


def measure(binary, count, shape):
    with tempfile.TemporaryDirectory(prefix="suru-restoration-") as temporary:
        root = Path(temporary).resolve()
        config = root / "config"
        config.mkdir()
        (config / "suru.json").write_text(json.dumps({
            "provider": {name: {"enabled": False} for name in ("codex", "copilot", "claude")},
            "serving": {"enabled": False},
        }), encoding="utf-8")
        env = dict(os.environ, SURU_STATE_DIR=str(root / "state"),
                   SURU_DATA_DIR=str(root / "data"), SURU_CONFIG_DIR=str(config),
                   SURU_CHANNEL="release", SURU_LOG="suru::sessions=debug")

        def command(action):
            return subprocess.run([binary, "server", action], env=env, check=True,
                                  capture_output=True, text=True, timeout=120)

        try:
            command("start")  # Let the binary create and migrate its database.
            command("stop")
            database = root / "data" / "suru.db"
            ids = [str(uuid.uuid4()) for _ in range(count)]
            with sqlite3.connect(database) as connection:
                for index, session_id in enumerate(ids):
                    parent = None if shape == "roots" or index == 0 else ids[
                        0 if shape == "wide" else index - 1 if shape == "deep" else (index - 1) // 2
                    ]
                    connection.execute("""INSERT INTO sessions
                        (id, title, created_at, updated_at, workspace,
                         agent_selection_availability, status, revision, parent_session_id)
                        VALUES (?, ?, 1, 2, ?, '"unavailable"', '"idle"', 7, ?)""",
                        (session_id, "benchmark", json.dumps({"path": str(root)}), parent))
                    if shape != "roots":
                        # Four disjoint historical intervals per Session, with
                        # one open leaf Turn. Deep trees must retain components
                        # without repeatedly copying their whole history.
                        for turn_index in range(4):
                            start = 10 + index * 20 + turn_index * 3
                            active = index == count - 1 and turn_index == 3
                            payload = {"agent": None, "status": "active" if active else "completed",
                                       "started_at": start, "settled_at": None if active else start + 1,
                                       "usage": {"output_tokens": 1}}
                            connection.execute("""INSERT INTO turns
                                (id, session_id, row_order, payload) VALUES (?, ?, ?, ?)""",
                                (str(uuid.uuid4()), session_id, turn_index, json.dumps(payload)))
            samples = []
            for _ in range(3):
                start = time.perf_counter()
                command("start")
                readiness = (time.perf_counter() - start) * 1000
                descriptor = json.loads((root / "state" / "runtime.json").read_text())
                request = urllib.request.Request(descriptor["base_url"] + "/v1/sessions",
                    headers={"Authorization": "Bearer " + descriptor["token"]})
                with urllib.request.urlopen(request, timeout=30) as response:
                    listing = json.load(response)
                assert len(listing) == (count if shape == "roots" else 1), listing
                if shape != "roots":
                    assert listing[0]["summary"]["total_usage"]["output_tokens"] == count * 4
                command("stop")  # Flush log output before extracting this process's timing.
                logs = list((root / "state" / "log").glob(f"*-server-{descriptor['pid']}.log"))
                text = "\n".join(path.read_text() for path in logs)
                line = next(line for line in text.splitlines() if "Session restoration completed" in line)
                values = {name: float(re.search(rf"\b{name}=([0-9.eE+-]+)", line)[1])
                          for name in ("projection_ms", "restoration_ms")}
                samples.append(dict(readiness_ms=readiness, **values))
            return {"shape": shape, "sessions": count, "samples": samples,
                    "median_ms": {key: statistics.median(row[key] for row in samples)
                                  for key in samples[0]}}
        finally:
            subprocess.run([binary, "server", "stop"], env=env, capture_output=True, timeout=120)


if __name__ == "__main__":
    binary = str(Path(sys.argv[1]).resolve())
    for count, shape in [(0, "roots"), (1000, "roots"), (5000, "roots"), (10000, "roots"),
                         (1000, "wide"), (512, "deep"), (1023, "balanced")]:
        print(json.dumps(measure(binary, count, shape)), flush=True)
