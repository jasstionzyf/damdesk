"""夹具：import 成功、但 serve() 永久阻塞且不读 stdin —— health 握手必然超时。

对应协议 §2 的「握手挂死 → 进冷却」路径；宿主应记 `plugin_health_timeout`。
"""
import time


def serve():
    # 不读 stdin、不回任何帧：宿主 2s 内收不到 health 应答
    while True:
        time.sleep(0.2)
