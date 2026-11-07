"""Real isolated Fleet smoke tests, including a TERM-resistant git hook.

Run with: python3 crates/sk1ll-gui/tests/test_dev_fleet.py
"""
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import time
import unittest

ROOT = Path(__file__).resolve().parents[3]


def processes():
    output = subprocess.check_output(["ps", "-eo", "pid=,ppid=,pgid=,sid=,stat="], text=True)
    return [tuple(line.split()) for line in output.splitlines()]


def session_members(sessions):
    return [row for row in processes() if int(row[3]) in sessions and not row[4].startswith("Z")]


class DevelopmentFleet(unittest.TestCase):
    def run_fleet(self, slow_hook=False, stop=signal.SIGINT):
        pid, terminal = pty.fork()
        if pid == 0:
            args = [str(ROOT / "scripts/dev-fleet.sh")]
            if stop is None:
                args += ["--", "true"]
            os.execv(args[0], args)
        output = ""
        fleet = None
        sessions = set()
        request = None
        exited = False
        try:
            deadline = time.monotonic() + 60
            while "Ctrl-C removes the temporary Fleet." not in output:
                self.assertLess(time.monotonic(), deadline, output)
                if select.select([terminal], [], [], 0.1)[0]:
                    try:
                        output += os.read(terminal, 65536).decode(errors="replace")
                    except OSError:
                        self.fail(output)
            fleet = Path(output.split("Files and logs: ", 1)[1].splitlines()[0].strip())
            # Service HOME and session identity must be isolated from this test and the real HOME.
            for row in processes():
                try:
                    env = Path(f"/proc/{row[0]}/environ").read_bytes().split(b"\0")
                except (FileNotFoundError, PermissionError, ProcessLookupError):
                    continue
                if any(value in env for value in [f"HOME={fleet / name}".encode() for name in ("hub", "alpha", "beta")]):
                    sessions.add(int(row[3]))
            if stop is not None:
                self.assertEqual(len(sessions), 3, output)
                self.assertNotIn(os.getsid(0), sessions)
            if slow_hook:
                marker = fleet / "hook-started"
                hook = fleet / "alpha/skills/.git/hooks/pre-push"
                hook.write_text(f"#!/bin/sh\ntrap '' TERM INT\nprintf '%s\\n' \"$$\" > '{marker}'\nsleep 300\n")
                hook.chmod(0o700)
                request = subprocess.Popen([
                    "curl", "--silent", "--show-error", "--max-time", "30", "--unix-socket",
                    str(fleet / "alpha/run/sk1ll/daemon.sock"), "--request", "POST",
                    "--header", "Content-Type: application/json",
                    "--data", json.dumps({"items": ["skill:local-only"], "message": "Slow hook cleanup test"}),
                    "http://localhost/promote",
                ], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                deadline = time.monotonic() + 15
                while not marker.exists():
                    self.assertLess(time.monotonic(), deadline, output)
                    time.sleep(0.05)
                hook_pid = marker.read_text().strip()
                hook_row = next(row for row in processes() if row[0] == hook_pid)
                self.assertIn(int(hook_row[3]), sessions)
                self.assertNotEqual(hook_row[2], hook_row[3], "git hook must escape the launch process group")
            started = time.monotonic()
            if stop == signal.SIGINT:
                os.write(terminal, b"\x03")  # Actual terminal Ctrl-C, not just a direct kill.
            elif stop is not None:
                os.kill(pid, stop)
            deadline = started + 10
            while True:
                done, status = os.waitpid(pid, os.WNOHANG)
                if done:
                    exited = True
                    break
                self.assertLess(time.monotonic(), deadline, "cleanup exceeded 10 seconds")
                if select.select([terminal], [], [], 0.05)[0]:
                    try:
                        output += os.read(terminal, 65536).decode(errors="replace")
                    except OSError:
                        pass
            self.assertEqual(os.waitstatus_to_exitcode(status), 128 + stop if stop else 0, output)
            self.assertFalse(fleet.exists(), "temporary Fleet was not removed")
            self.assertEqual(session_members(sessions), [], "processes survived cleanup")
            if slow_hook:
                self.assertGreaterEqual(time.monotonic() - started, 4.5, "TERM-resistant hook did not exercise SIGKILL")
        finally:
            if not exited:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            for sid in sessions:
                for row in session_members({sid}):
                    try:
                        os.kill(int(row[0]), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            if request:
                request.communicate(timeout=5)
            os.close(terminal)

    def test_start_and_ctrl_c_cleans_sessions(self):
        self.run_fleet()

    def test_ctrl_c_kills_slow_pre_push_hook_in_separate_process_group(self):
        self.run_fleet(slow_hook=True)

    def test_term_cleans_sessions(self):
        self.run_fleet(stop=signal.SIGTERM)

    def test_command_exit_cleans_sessions(self):
        self.run_fleet(stop=None)


class Workspace(unittest.TestCase):
    def test_gui_is_explicit_workspace_member_without_anchor(self):
        metadata = json.loads(subprocess.check_output([
            "cargo", "metadata", "--no-deps", "--format-version", "1",
        ], cwd=ROOT, text=True))
        packages = {p["name"]: p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]}
        self.assertEqual(set(packages), {
            "sk1ll-cli", "sk1ll-core", "sk1ll-daemon", "sk1ll-hub", "sk1ll-proto", "sk1ll-gui",
        })
        self.assertEqual(Path(packages["sk1ll-gui"]["manifest_path"]), ROOT / "crates/sk1ll-gui/src-tauri/Cargo.toml")
        self.assertFalse((ROOT / "crates/sk1ll-gui/Cargo.toml").exists())
        self.assertFalse((ROOT / "crates/sk1ll-gui/workspace.rs").exists())
        config = json.loads((ROOT / "crates/sk1ll-gui/src-tauri/tauri.conf.json").read_text())
        self.assertEqual((ROOT / "crates/sk1ll-gui/src-tauri" / config["build"]["frontendDist"]).resolve(),
                         ROOT / "crates/sk1ll-gui/ui/dist")


if __name__ == "__main__":
    unittest.main(verbosity=2)
