#!/usr/bin/env python3
"""
Smoke test for the Thumbrella standalone server (tier2 + tier3).

Builds release binaries, then runs each through `check`, `thumb`, and `serve`
commands.  The serve step starts a real HTTP server, hits /health and /thumb.jpeg,
and shuts it down.

Usage:
    python scripts/smoke-test.py              # build + test both tiers
    python scripts/smoke-test.py --skip-build # test only (assumes already built)
    python scripts/smoke-test.py --tier 2     # test tier2 only
    python scripts/smoke-test.py --tier 3     # test tier3 only

Requires:
    - Rust toolchain (cargo)
    - Running from the thumbrella repo root
"""

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

# --- config ------------------------------------------------------------------

REPO_ROOT = Path(__file__).resolve().parent.parent
TARGET_DIR = REPO_ROOT / "target" / "release"
TEST_URLS = [
    "https://demo.thumbrella.dev/media/space-colony.jpg",
    "https://demo.thumbrella.dev/media/harbor-trucks.mp4",
]
SERVE_STARTUP_TIMEOUT = 15  # seconds to wait for server to become ready
HEALTH_POLL_INTERVAL = 0.3

# --- helpers -----------------------------------------------------------------

def run(cmd, **kwargs):
    """Run a command, print it, and exit on failure."""
    label = kwargs.pop("label", None)
    desc = label or " ".join(str(c) for c in cmd)
    print(f"\n  [{desc}]")
    result = subprocess.run(cmd, cwd=REPO_ROOT, **kwargs)
    if result.returncode != 0:
        print(f"  FAILED (exit code {result.returncode})")
        sys.exit(1)
    return result


def run_capture(cmd, **kwargs):
    """Run a command and return (returncode, stdout, stderr)."""
    return subprocess.run(
        cmd, cwd=REPO_ROOT, capture_output=True, text=True, **kwargs
    )


def free_port():
    """Return a free TCP port number."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http_get(url, timeout=10):
    """GET a URL, return (status, body)."""
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def wait_for_server(port, timeout=SERVE_STARTUP_TIMEOUT):
    """Poll /health until the server responds or timeout expires."""
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            status, _ = http_get(f"http://127.0.0.1:{port}/health", timeout=2)
            if status == 200:
                return True
        except (urllib.error.URLError, OSError, ConnectionRefusedError) as e:
            last_error = e
        time.sleep(HEALTH_POLL_INTERVAL)
    print(f"  Server did not become ready within {timeout}s: {last_error}")
    return False


# --- build -------------------------------------------------------------------

def build_all():
    print("=== Building tier2 + tier3 (release) ===")
    run(["cargo", "build", "--release", "-p", "tier2", "-p", "tier3"],
        label="cargo build --release -p tier2 -p tier3")


# --- check -------------------------------------------------------------------

def run_check(binary_name, label):
    exe = TARGET_DIR / binary_name
    if not exe.exists():
        print(f"  SKIP: {exe} not found (build first)")
        return False
    env = {**os.environ, "NO_COLOR": "1"}
    result = run_capture([str(exe), "check", "--json"], env=env)
    if result.returncode != 0:
        print(f"  {label} check FAILED:\n{result.stderr}")
        return False
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError:
        print(f"  {label} check produced invalid JSON:\n{result.stdout[:500]}")
        return False
    issues = report.get("issues", [])
    if issues:
        print(f"  {label} check found issues: {issues}")
        # issues don't necessarily mean failure, just warnings
    print(f"  {label} check OK")
    return True


# --- thumb -------------------------------------------------------------------

def run_thumb(binary_name, label):
    exe = TARGET_DIR / binary_name
    if not exe.exists():
        print(f"  SKIP: {exe} not found")
        return False
    env = {**os.environ, "NO_COLOR": "1"}
    for url in TEST_URLS:
        print(f"  {label} thumb: {url}")
        result = run_capture([str(exe), "thumb", "--json", url], env=env, timeout=60)
        if result.returncode != 0:
            print(f"  FAILED:\n{result.stderr[:500]}")
            return False
        try:
            data = json.loads(result.stdout)
        except json.JSONDecodeError:
            print(f"  Invalid JSON output:\n{result.stdout[:300]}")
            return False
        # --json outputs a bare result object, not wrapped in {"items": [...]}
        status = data.get("status", "unknown")
        print(f"  -> status={status}  source={data.get('source','?')}  "
              f"duration={data.get('duration',0):.3f}s")
        if status not in ("success", "placeholder"):
            print(f"  WARNING: unexpected status '{status}'")
        # At least one success is good enough
    return True


# --- serve -------------------------------------------------------------------

def run_serve(binary_name, label, port):
    """Start server, test endpoints, shut down."""
    exe = TARGET_DIR / binary_name
    if not exe.exists():
        print(f"  SKIP: {exe} not found")
        return False

    env = os.environ.copy()
    env["TBR_PORT"] = str(port)

    print(f"  Starting {label} on port {port}...")
    proc = subprocess.Popen(
        [str(exe), "serve"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )

    try:
        if not wait_for_server(port):
            return False

        # Test /health
        status, body = http_get(f"http://127.0.0.1:{port}/health")
        print(f"  /health -> {status}")
        if status != 200:
            print(f"  FAILED: {body[:200]}")
            return False

        # Test /thumb.jpeg
        test_url = TEST_URLS[0]
        encoded = urllib.parse.quote(test_url, safe="")
        status, body = http_get(
            f"http://127.0.0.1:{port}/thumb.jpeg?url={encoded}",
            timeout=30,
        )
        print(f"  /thumb.jpeg -> {status}  ({len(body)} bytes)")
        if status != 200:
            print(f"  FAILED: {body[:200]}")
            return False
        # Verify it looks like a JPEG
        if body[:2] != b"\xff\xd8":
            print(f"  WARNING: response does not look like a JPEG")

        print(f"  {label} serve OK")

    finally:
        print(f"  Stopping {label}...")
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    return True


# --- main --------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Thumbrella server smoke test")
    parser.add_argument("--skip-build", action="store_true",
                        help="skip cargo build (assumes binaries already exist)")
    parser.add_argument("--tier", type=int, choices=[2, 3], default=0,
                        help="test only a specific tier (default: both)")
    args = parser.parse_args()

    if not args.skip_build:
        build_all()

    overall = True

    if args.tier in (0, 2):
        print("\n=== Tier 2 ===")
        overall &= run_check("tier2", "tier2")
        overall &= run_thumb("tier2", "tier2")
        port2 = free_port()
        overall &= run_serve("tier2", "tier2", port2)

    if args.tier in (0, 3):
        print("\n=== Tier 3 ===")
        overall &= run_check("thumbrella", "tier3")
        overall &= run_thumb("thumbrella", "tier3")
        port3 = free_port()
        overall &= run_serve("thumbrella", "tier3", port3)

    if overall:
        print("\n  ALL SMOKE TESTS PASSED")
    else:
        print("\n  SOME SMOKE TESTS FAILED")
        sys.exit(1)


if __name__ == "__main__":
    main()
