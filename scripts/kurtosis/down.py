#!/usr/bin/env python3
"""Tear down the kurtosis enclave. Usage: scripts/kurtosis/down.py [enclave]"""

import subprocess
import sys

enclave = sys.argv[1] if len(sys.argv) > 1 else "arkiv-harness"
subprocess.run(["kurtosis", "enclave", "rm", "-f", enclave], check=True)
