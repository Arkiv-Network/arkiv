#!/usr/bin/env python3
"""Tear down the kurtosis enclave. Usage: scripts/kurtosis/down.py [enclave]"""

import subprocess
import sys
from datetime import datetime, timezone

enclave = sys.argv[1] if len(sys.argv) > 1 else "arkiv-harness"
timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
print(f"[{timestamp}] Removing Kurtosis enclave: {enclave}", flush=True)
subprocess.run(["kurtosis", "enclave", "rm", "-f", enclave], check=True)
print(f"[{timestamp}] Kurtosis enclave removed: {enclave}", flush=True)
