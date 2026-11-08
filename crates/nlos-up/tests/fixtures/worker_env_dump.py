"""W82-C gate-letter fixture: dump full environ, then hold a grandchild.

argv[1] = output basename: writes <base>.env / <base>.pid / <base>.grandpid.
"""
import os
import subprocess
import sys
import time

base = sys.argv[1]
with open(base + ".env", "w", encoding="utf-8") as handle:
    for key in sorted(os.environ):
        handle.write(f"{key}={os.environ[key]}\n")
with open(base + ".pid", "w", encoding="utf-8") as handle:
    handle.write(str(os.getpid()))
child = subprocess.Popen(["sleep", "300"])
with open(base + ".grandpid", "w", encoding="utf-8") as handle:
    handle.write(str(child.pid))
while child.poll() is None:
    time.sleep(0.5)
