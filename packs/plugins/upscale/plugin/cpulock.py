#!/usr/bin/env python3
"""CPU 耗时闸门：同机「模型档」并发上限（默认 1 槽），队列里看得见「等待中」。

为什么要包内闸门：超分包是 `engine=full` 的**重模型**（x4v3 单张 2~5s，峰值内存 ~700MB，
4MP 输入下 4× 输出 64MP）。宿主按任务并行拉起多个插件子进程时，N 个模型推理同时跑会把
CPU 与内存打成锯齿（实测 3 并发时单张耗时从 3s 涨到 11s）。协议里没有「耗时档」字段
（v1.1 只有 `timeout_secs`），所以闸门只能由包自己实现：**跨进程 flock**，同一台机上的
所有本包进程共享同一组锁文件。

口径：
    - 槽位数 = `DAMPACK_UPSCALE_CPU_SLOTS`（默认 1；强机可调）；
    - 锁文件目录 = `DAMPACK_UPSCALE_LOCK_DIR`（默认系统临时目录；测试用它隔离）；
    - 等待期间每 `NOTIFY_EVERY` 秒发一条 `progress{step:"waiting"}`，宿主任务中心据此显示
      「等待中」，不让人以为卡死；
    - `engine=light`（重采样档，毫秒级）**不进闸门** —— 卡住它没有收益。
"""
from __future__ import annotations

import errno
import os
import tempfile
import time

try:
    import fcntl
except ImportError:  # pragma: no cover - Windows 桌面壳走不到这里（插件跑在 POSIX 侧）
    fcntl = None

DEFAULT_SLOTS = 1
NOTIFY_EVERY = 2.0
POLL = 0.05


def slots_from_env():
    raw = os.environ.get("DAMPACK_UPSCALE_CPU_SLOTS")
    try:
        n = int(raw) if raw else DEFAULT_SLOTS
    except (TypeError, ValueError):
        n = DEFAULT_SLOTS
    return max(1, n)


def lock_dir_from_env():
    return os.environ.get("DAMPACK_UPSCALE_LOCK_DIR") or tempfile.gettempdir()


class CpuGate:
    """`with CpuGate(notify=..., job_id=...) as gate:` —— 退出时释放槽位。

    `wait_ms` 属性记录排队耗时（进结果行 evidence，验收靠它证「并发 ≤ 槽位」）。
    """

    def __init__(self, slots=None, lock_dir=None, notify=None, job_id=None, step="upscale"):
        self.slots = slots if slots is not None else slots_from_env()
        self.lock_dir = lock_dir or lock_dir_from_env()
        self.notify = notify
        self.job_id = job_id
        self.step = step
        self.wait_ms = 0
        self.slot = None
        self._fd = None

    def _path(self, slot):
        return os.path.join(self.lock_dir, "damdesk-upscale-cpu-%d.lock" % slot)

    def __enter__(self):
        started = time.time()
        if fcntl is None:  # pragma: no cover
            self.wait_ms = 0
            return self
        os.makedirs(self.lock_dir, exist_ok=True)
        fds = []
        try:
            for slot in range(self.slots):
                fd = os.open(self._path(slot), os.O_RDWR | os.O_CREAT, 0o600)
                fds.append(fd)
            last_notice = 0.0
            while True:
                for slot, fd in enumerate(fds):
                    try:
                        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except OSError as exc:
                        if exc.errno not in (errno.EAGAIN, errno.EACCES):
                            raise
                        continue
                    self.slot = slot
                    self._fd = fd
                    # 拿到槽位后其它 fd 关掉（锁跟 fd 走，关掉不影响已持有的那把）
                    for other, other_fd in enumerate(fds):
                        if other != slot:
                            os.close(other_fd)
                    self.wait_ms = int((time.time() - started) * 1000)
                    return self
                waited = time.time() - started
                if self.notify and waited - last_notice >= NOTIFY_EVERY:
                    last_notice = waited
                    self.notify({
                        "job_id": self.job_id,
                        "step": "waiting",
                        "done": 0,
                        "total": 1,
                        "note": "等待模型档空位（并发 %d）" % self.slots,
                        "waitedMs": int(waited * 1000),
                    })
                time.sleep(POLL)
        except BaseException:
            for fd in fds:
                try:
                    os.close(fd)
                except OSError:
                    pass
            raise

    def __exit__(self, exc_type, exc, tb):
        if self._fd is not None:
            if fcntl is not None:
                try:
                    fcntl.flock(self._fd, fcntl.LOCK_UN)
                except OSError:
                    pass
            try:
                os.close(self._fd)
            except OSError:
                pass
            self._fd = None
        return False
