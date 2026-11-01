"""每包 venv 的创建/装依赖（裁决 2026-10-20：依赖环境 = 每包独立 venv）。

为什么不是 `sys.path` + `vendor/` 覆盖：
    - 共享 runtime 只当**解释器**（不含第三方库，禁止 `--system-site-packages`）；
    - 手工 sys.path 不等于环境隔离：包自带 numpy 1.26、宿主 site 半套要求 2.x 时，
      原生库 ABI/DLL/OpenMP 会串（实际踩过），且这种混合环境**没人测过**；
    - venv 是干净边界：`rm -rf <pack>/venv` 即卸净，不写系统 site-packages。

装/卸都由宿主调用（协议 §4.0），本模块给宿主与包自带自检脚本一份同一实现：

    from dampack.venv import ensure_venv, venv_python
    venv_dir = ensure_venv(pack_dir)          # 建 venv + pip install -r requirements.txt
    print(venv_python(venv_dir))              # Windows 自动取 Scripts\\python.exe
"""
from __future__ import annotations

import os
import subprocess
import sys

VENV_DIRNAME = "venv"
REQUIREMENTS = "requirements.txt"


def venv_dir_of(pack_dir):
    return os.path.join(os.path.abspath(pack_dir), VENV_DIRNAME)


def venv_python(venv_dir):
    """venv 里的解释器路径（Windows 是 `Scripts\\python.exe`）。"""
    if os.name == "nt":
        return os.path.join(venv_dir, "Scripts", "python.exe")
    return os.path.join(venv_dir, "bin", "python3")


def venv_pip(venv_dir):
    python = venv_python(venv_dir)
    return [python, "-m", "pip"]


def ensure_venv(pack_dir, python=None, requirements=REQUIREMENTS, recreate=False,
                pip_cache_dir=None, offline_wheels=None, extra_pip_args=()):
    """建包内 venv 并装 `requirements.txt`，返回 venv 目录。

    参数：
        python         建 venv 用的解释器（默认当前解释器；宿主传内置 runtime 的 python）
        recreate       已存在则删掉重建（升级/修复环境用）
        pip_cache_dir  宿主 cache-dir（共享 wheel 缓存，省重复下载）
        offline_wheels 本地 wheel 目录；给了就 `--no-index --find-links`，断网可复现
    返回 venv 目录；任一步失败抛 `RuntimeError`（宿主据此报 `plugin_load_failed`）。
    """
    pack_dir = os.path.abspath(pack_dir)
    if not os.path.isdir(pack_dir):
        raise RuntimeError("包目录不存在：%s" % pack_dir)
    if not os.path.isfile(os.path.join(pack_dir, "manifest.json")):
        raise RuntimeError("缺 manifest.json：%s" % pack_dir)

    target = venv_dir_of(pack_dir)
    if recreate and os.path.isdir(target):
        import shutil
        shutil.rmtree(target, ignore_errors=True)

    if not os.path.isfile(venv_python(target)):
        created = _run([python or sys.executable, "-m", "venv", "--clear", target],
                       cwd=pack_dir, what="建 venv")
        if created != 0:
            raise RuntimeError("建 venv 失败（exit=%d）" % created)

    req = os.path.join(pack_dir, requirements)
    if not os.path.isfile(req):
        return target      # 零依赖包：requirements.txt 可有可无

    args = venv_pip(target) + ["install", "--disable-pip-version-check", "-r", req]
    if offline_wheels:
        args += ["--no-index", "--find-links", os.path.abspath(offline_wheels)]
    args += list(extra_pip_args)

    env = dict(os.environ)
    env["PIP_DISABLE_PIP_VERSION_CHECK"] = "1"
    if pip_cache_dir:
        env["PIP_CACHE_DIR"] = os.path.abspath(pip_cache_dir)
    code = _run(args, cwd=pack_dir, what="pip install", env=env)
    if code != 0:
        raise RuntimeError("pip install -r %s 失败（exit=%d）" % (requirements, code))
    return target


def probe_isolation(venv_dir, modules=("numpy", "PIL", "cv2", "requests")):
    """回「这个 venv 里可见的第三方库」清单（自检/报告用；空 = 干净隔离）。"""
    python = venv_python(venv_dir)
    script = (
        "import importlib.util, json, sys\n"
        "print(json.dumps([m for m in %r if importlib.util.find_spec(m) is not None]))\n"
        % (list(modules),)
    )
    proc = subprocess.run([python, "-c", script], capture_output=True, text=True,
                          encoding="utf-8", errors="replace", timeout=120)
    if proc.returncode != 0:
        raise RuntimeError("venv 自检失败：%s" % (proc.stderr.strip().splitlines()[-1:],))
    import json
    return json.loads(proc.stdout.strip() or "[]")


def _run(args, cwd=None, what="", env=None):
    sys.stderr.write("[dampack.venv] %s: %s\n" % (what, " ".join(args)))
    sys.stderr.flush()
    try:
        return subprocess.call(args, cwd=cwd, env=env)
    except OSError as exc:
        raise RuntimeError("%s 无法执行：%s" % (what, exc))
