"""Run one benchmark and record child usage without a system time package."""
import json
import resource
import subprocess
import sys
import time

started = time.monotonic()
result = subprocess.run(sys.argv[1:])
usage = resource.getrusage(resource.RUSAGE_CHILDREN)
print(json.dumps({"resource_usage": {
    "elapsed_seconds": time.monotonic() - started,
    "max_rss_kib_linux": usage.ru_maxrss,
    "user_seconds": usage.ru_utime, "system_seconds": usage.ru_stime,
    "exit_code": result.returncode,
}}), flush=True)
sys.exit(result.returncode)
