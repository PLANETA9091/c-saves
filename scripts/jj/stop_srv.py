#!/usr/bin/env python3
"""Graceful stop for the tps-bench server phases (RCON stop + drain)."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rcon import rcon

try:
    print("stop:", rcon("stop", timeout=30).strip()[:80], flush=True)
except Exception as e:
    print(f"stop ERR {type(e).__name__}: {e}", flush=True)
time.sleep(8)
print("stopped", flush=True)
