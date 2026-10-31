随包分发的行业包启动器（dampack_launcher.py）落位点。

.py 由 scripts/stage_pack_runtime.py 从 tools/dampack_launcher.py 拷贝（**不重复维护**）；
出包门禁 scripts/pack_runtime_gate.sh 会 cmp 两份文件，分叉即拒绝出包。
