"""启动器：sys.path 装配顺序 / 退出码 / --probe，以及**与宿主 K02 启动器同契约**。

「同契约」是硬要求：宿主 `pack-host` spawn 的是 `python -m dampack_launcher`，
如果 SDK 版与宿主版对同一包的装配结果/退出码不同，就会出现「宿主能加载、自测不能」
这类无法归因的差异。
"""
from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import unittest

import _support
from dampack import launcher


def _load_host_launcher():
    path = os.path.join(_support.REPO_ROOT, "tools", "dampack_launcher.py")
    spec = importlib.util.spec_from_file_location("host_dampack_launcher", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class AssembleTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.pack_dir, _ = _support.make_fixture_pack()

    def test_order_puts_pack_first(self):
        extras = launcher.assemble(self.pack_dir)
        self.assertEqual(extras[0], self.pack_dir)
        self.assertEqual(extras[1], os.path.join(self.pack_dir, "plugin"))
        # SDK 自身必须可 import：要么在 extras 里，要么已在 sys.path（本测试进程已导入 SDK）
        self.assertIn(launcher.SDK_ROOT, [os.path.abspath(p) for p in sys.path])

    def test_vendor_and_resources_when_present(self):
        os.makedirs(os.path.join(self.pack_dir, "vendor"), exist_ok=True)
        os.makedirs(os.path.join(self.pack_dir, "resources"), exist_ok=True)
        try:
            extras = launcher.assemble(self.pack_dir)
        finally:
            os.rmdir(os.path.join(self.pack_dir, "vendor"))
            os.rmdir(os.path.join(self.pack_dir, "resources"))
        self.assertIn(os.path.join(self.pack_dir, "vendor"), extras)
        self.assertIn(os.path.join(self.pack_dir, "resources"), extras)

    def test_shared_site_is_last(self):
        shared = os.path.join(self.pack_dir, "shared-site")
        os.makedirs(shared, exist_ok=True)
        saved = os.environ.get("DAMPACK_SHARED_SITE_DIR")
        try:
            os.environ["DAMPACK_SHARED_SITE_DIR"] = shared
            launcher.assemble(self.pack_dir)
            self.assertEqual(sys.path[-1], shared)
        finally:
            os.rmdir(shared)
            if saved is None:
                os.environ.pop("DAMPACK_SHARED_SITE_DIR", None)
            else:
                os.environ["DAMPACK_SHARED_SITE_DIR"] = saved

    def test_entry_module_from_manifest(self):
        self.assertEqual(launcher.entry_module(self.pack_dir), "plugin")


class ExitCodeTest(unittest.TestCase):
    """退出码必须与宿主契约一致：0 正常 / 2 用法错 / 3 加载失败。"""

    def _run(self, args, cwd=None):
        return subprocess.run([sys.executable, "-m", "dampack.launcher"] + args,
                              capture_output=True, text=True, encoding="utf-8",
                              cwd=cwd or _support.SDK_DIR,
                              env=dict(os.environ, PYTHONPATH=_support.SDK_DIR,
                                       PYTHONUNBUFFERED="1"), timeout=60)

    def test_missing_args_is_usage_error(self):
        proc = self._run([])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("用法", proc.stderr)
        self.assertEqual(proc.stdout, "", "用法错误也不许往 stdout 写东西")

    def test_missing_manifest_is_usage_error(self):
        proc = self._run([_support.SDK_DIR])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("manifest.json", proc.stderr)

    def test_missing_plugin_entry_is_load_error(self):
        import tempfile
        pack_dir = tempfile.mkdtemp(prefix="no-plugin-")
        with open(os.path.join(pack_dir, "manifest.json"), "w", encoding="utf-8") as fh:
            json.dump({"entrypoint": {"cmd": "python3", "args": []}}, fh)
        proc = self._run([pack_dir])
        self.assertEqual(proc.returncode, 3)
        self.assertIn("plugin/__init__.py", proc.stderr)

    def test_probe_prints_one_json_line(self):
        # 用**真样例包**（不是夹具）：probe 顺带证明 SDK 样例包经启动器可以 import 出来
        pack_dir = os.path.join(_support.SDK_DIR, "examples", "ecommerce_py")
        proc = self._run([pack_dir, "--probe", "plugin"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = [ln for ln in proc.stdout.splitlines() if ln.strip()]
        self.assertEqual(len(lines), 1, proc.stdout)
        payload = json.loads(lines[0])
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["pack_id"], "com.soujpg.pack.ecommerce_py")


class HostParityTest(unittest.TestCase):
    """SDK 启动器 vs 宿主 K02 启动器：同一包必须得到同一装配前缀与同一退出码。"""

    def test_same_prefix_and_codes(self):
        host = _load_host_launcher()
        pack_dir, _ = _support.make_fixture_pack()

        sdk_prefix = [os.path.abspath(p) for p in launcher.assemble(pack_dir)][:4]
        host_prefix = [os.path.abspath(p) for p in host.assemble(pack_dir)][:4]
        self.assertEqual(sdk_prefix, host_prefix)

        sdk_entry = launcher.entry_module(pack_dir)
        host_entry = host.entry_module(pack_dir)
        self.assertEqual(sdk_entry, host_entry)

        self.assertEqual((launcher.EXIT_OK, launcher.EXIT_USAGE, launcher.EXIT_LOAD),
                         (host.EXIT_OK, host.EXIT_USAGE, host.EXIT_LOAD))


if __name__ == "__main__":
    unittest.main()
