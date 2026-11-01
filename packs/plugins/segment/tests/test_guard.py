"""三道守卫（K07-FIX-N `t_5c968bc9`）：已白底跳过 / 掩码低置信度拒绝 / 主体损失红线。

三条都**带反向验证**（去掉保护逻辑，正向用例必须变红）——不然「守卫存在」只是断言，
不是证据。语料口径的阈值标定在包 README「守卫」一节（用 v1.1 语料实测，不用合成图自证）。
"""
from __future__ import annotations

import os
import unittest
from unittest import mock

import _support
import pngio

plugin = _support.load_plugin()
import composite  # noqa: E402  （_support 已把 plugin 目录塞进 sys.path）


def _judge(tmp, path, options):
    opts = dict(options)
    opts.setdefault("workdir", os.path.join(tmp, "out"))
    return plugin.dispatch({"method": "run",
                            "params": {"job_id": "job-guard",
                                       "asset": {"id": "asset-1", "path": path},
                                       "options": opts}})["rows"][0]


def _workdir_files(tmp):
    out = os.path.join(tmp, "out")
    return sorted(os.listdir(out)) if os.path.isdir(out) else []


class AlreadyWhiteSkipTest(unittest.TestCase):
    """守卫②：目标白底 + 输入已白底 → 跳过（不产生产物，也不跑模型）。"""

    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_white_fixture()
        _support.install_stub(plugin, self.gw)
        plugin._REPORTS.clear()

    def test_skips_without_artifact_and_without_model(self):
        row = _judge(self.tmp, self.src, {"bgMode": "white"})
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["skipReason"], "already_white")
        self.assertEqual(row.get("artifacts") or [], [],
                         "跳过就不能产出新工作版本")
        self.assertEqual(_workdir_files(self.tmp), [], "跳过时工作目录里不许留任何文件")
        # 省下的三段算力：模型 / 裁块 / 编码一次都没发
        self.assertEqual(self.gw.count("/detect/u2net"), 0)
        self.assertEqual(self.gw.count("/op/crop"), 0)
        self.assertEqual(self.gw.count("/report/emit"), 0, "没有产物就不该上报")
        ev = row["evidence"]
        self.assertEqual(ev["inputBorderDev"], 0.0)
        self.assertLessEqual(ev["bgBlockMeanDev"], plugin.BG_BLOCK_MAX_MEAN_DEV)
        self.assertEqual(ev["guard"], "already_white")

    def test_reverse_guard_off_must_paint(self):
        """反向验证：把「已白底」判据关掉（阈值 -1 = 永不高过），同一张图必须被重刷出产物。

        正向用例（上面那条）之所以 PASS，就是因为守卫在挡——不是别的原因。
        """
        with mock.patch.object(plugin, "ALREADY_WHITE_MAX_DEV", -1.0):
            row = _judge(self.tmp, self.src, {"bgMode": "white"})
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("skipReason", row, "守卫关掉后不该再跳过")
        self.assertEqual(len(row["artifacts"]), 1, "守卫关掉后必须落产物（说明真被重刷了）")
        self.assertEqual(self.gw.count("/detect/u2net"), 1)

    def test_color_mode_with_pure_white_target_also_skips(self):
        row = _judge(self.tmp, self.src, {"bgMode": "color", "color": "#ffffff"})
        self.assertEqual(row.get("skipReason"), "already_white")

    def test_chain_action_reports_skipped_not_ok(self):
        """链内（带 `options.action`）必须报 `skipped`：本步没产物，链要沿用上一步的图。

        为什么不能沿用普通口径的 `ok`：`compliance_chain.rs` 认为「ok 却没 outPath」
        是 `action_no_product`（整链失败）。第二次对已白底产物施加白底本来就不该动，
        报 ok 会把「不需要动」变成「跑坏了」。
        """
        row = _judge(self.tmp, self.src, {"bgMode": "white", "action": "white_bg_fill"})
        self.assertEqual(row["result"], "skipped")
        self.assertEqual(row["skipReason"], "already_white")
        self.assertEqual(row.get("artifacts") or [], [], "链内跳过同样零产物")
        self.assertEqual(row["evidence"]["chainStep"], "skipped_no_product")
        self.assertEqual(self.gw.count("/detect/u2net"), 0, "跳过的判据没变：模型一次都不跑")

    def test_color_mode_with_brand_color_never_skips(self):
        row = _judge(self.tmp, self.src, {"bgMode": "color", "color": "#3366cc"})
        self.assertNotIn("skipReason", row, "指定色底必须真的重做")
        self.assertEqual(len(row["artifacts"]), 1)

    def test_transparent_mode_never_skips(self):
        row = _judge(self.tmp, self.src, {"bgMode": "transparent"})
        self.assertNotIn("skipReason", row, "透明底是另一件事，即使输入白底也必须出 alpha PNG")
        self.assertTrue(row["artifacts"][0]["path"].endswith(".png"))


class BackgroundColorBlockTest(unittest.TestCase):
    """守卫②的第二条：边框全白但背景有色块（白框包彩色照片）→ 不许跳过。"""

    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_frame_fixture()
        _support.install_stub(plugin, self.gw)

    def test_white_frame_with_color_block_is_processed(self):
        row = _judge(self.tmp, self.src, {"bgMode": "white"})
        self.assertNotIn("skipReason", row, "白框里有彩色照片，跳过就等于没抠图")
        self.assertEqual(len(row["artifacts"]), 1)
        ev = row["evidence"]
        self.assertEqual(ev["inputBorderDev"], 0.0, "边框带确实是白的")
        self.assertGreater(ev["bgBlockMeanDev"], plugin.BG_BLOCK_MAX_MEAN_DEV,
                           "48px 带均值必须把色块量出来")


class WhiteStatsUnavailableTest(unittest.TestCase):
    """守卫② 的降级口径：宿主没这项能力 → 照常处理，且不假装测过。"""

    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_white_fixture()
        self.gw.white_stats_available = False
        _support.install_stub(plugin, self.gw)

    def test_unavailable_degrades_to_processing(self):
        row = _judge(self.tmp, self.src, {"bgMode": "white"})
        self.assertNotIn("skipReason", row, "量不到就不能跳过")
        self.assertEqual(len(row["artifacts"]), 1)
        self.assertEqual(row["evidence"]["inputWhiteProbe"], "unavailable")


class MaskConfidenceTest(unittest.TestCase):
    """守卫①：掩码退化（整图算主体 / 只抓到噪点）→ skipped + 原因，不落产物。"""

    def _row_with_mask(self, mask_box, fg_ratio):
        tmp, gw, _box, src = _support.make_fixture("seg-lowconf-")
        gw.box = mask_box
        gw.fg_ratio = fg_ratio
        _support.mask_png(gw.mask_path, gw.width, gw.height, mask_box)
        _support.install_stub(plugin, gw)
        return _judge(tmp, src, {"bgMode": "white"}), gw, tmp

    def test_whole_image_as_subject_is_skipped(self):
        row, gw, tmp = self._row_with_mask((0, 0, 320, 240), 0.9999)
        self.assertEqual(row["result"], "skipped")
        self.assertEqual(row["skipReason"], "mask_low_confidence")
        self.assertEqual(row.get("artifacts") or [], [])
        self.assertEqual(_workdir_files(tmp), [])
        self.assertEqual(gw.count("/op/crop"), 0, "掩码不可信就别裁块刷图")
        self.assertEqual(row["evidence"]["fgRatio"], 0.9999)
        self.assertEqual(row["evidence"]["fgRatioRange"],
                         [plugin.MIN_FG_RATIO, plugin.MAX_FG_RATIO])

    def test_noise_only_mask_is_skipped(self):
        row, _gw, _tmp = self._row_with_mask((0, 0, 1, 1), 0.0001)
        self.assertEqual(row["skipReason"], "mask_low_confidence")

    def test_reverse_confidence_guard_off_must_paint(self):
        """反向验证：把置信度区间放到 (0, 1] 之外，同一张退化掩码图必须被刷出产物。"""
        tmp, gw, _box, src = _support.make_fixture("seg-lowconf-rev-")
        gw.box = (0, 0, 320, 240)
        gw.fg_ratio = 0.9999
        _support.mask_png(gw.mask_path, gw.width, gw.height, gw.box)
        _support.install_stub(plugin, gw)
        with mock.patch.object(plugin, "MAX_FG_RATIO", 1.0), \
                mock.patch.object(plugin, "MIN_FG_RATIO", 0.0):
            row = _judge(tmp, src, {"bgMode": "white"})
        self.assertNotIn("skipReason", row)
        self.assertEqual(len(row["artifacts"]), 1)

    def test_empty_mask_still_reports_no_subject(self):
        """fg_ratio=0 是「图里没主体」，不是「掩码不可信」——两个原因不许混。"""
        tmp, gw, _box, src = _support.make_fixture("seg-nosubj-")
        gw.fg_ratio = 0.0
        _support.install_stub(plugin, gw)
        row = _judge(tmp, src, {"bgMode": "white"})
        self.assertEqual(row["skipReason"], "no_subject")


class SubjectLossTest(unittest.TestCase):
    """守卫③：主体区域被改动 > 5% → failed + subject_loss，不落产物。"""

    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_fixture()
        _support.install_stub(plugin, self.gw)

    def test_normal_run_keeps_subject_untouched(self):
        row = _judge(self.tmp, self.src, {"bgMode": "white"})
        self.assertEqual(row["evidence"]["subjectChangedPixels"], 0,
                         "alpha=255 的主体像素必须原样保留")
        self.assertGreater(row["evidence"]["subjectOpaquePixels"], 0)

    def test_reverse_shifted_composite_is_rejected(self):
        """反向验证：让合成整体错位 8 像素（坐标回归的典型形态），守卫必须拒收且不留半成品。"""
        real = composite.blend_rgb
        shift = 8 * 3

        def shifted(src, width, height, alpha, bg):
            out = bytearray(real(src, width, height, alpha, bg))
            return out[shift:] + out[:shift]

        with mock.patch.object(composite, "blend_rgb", shifted):
            row = _judge(self.tmp, self.src, {"bgMode": "white"})
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "subject_loss")
        self.assertEqual(row.get("artifacts") or [], [])
        self.assertEqual(_workdir_files(self.tmp), [], "守卫命中后连半成品都不许留")
        self.assertGreater(row["evidence"]["subjectChangedRatio"],
                           plugin.MAX_SUBJECT_CHANGE_RATIO)


if __name__ == "__main__":
    unittest.main()
