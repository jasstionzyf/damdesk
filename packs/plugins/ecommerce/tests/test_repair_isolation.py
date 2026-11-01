"""修复链的「产物隔离」与「短边门槛」单测 —— 对应 K07 A4 的两个真实失败样本。

抓的是宿主 workdir 被整个包共用带来的两类事故（实测见
`damdesk-repo/references/fix-policy-and-a4.md` K07-A4）：

1. **同名产物互相覆盖**：f002/f003 的 `r1-1crop.png` 被上一资产的内容占着（crop 应答
   2388×2372、落盘 816×818）；掩码也串成 818×818 对 1600×1600 的图 → `/op/inpaint` 400。
   → 修复链的中间产物/掩码/交付产物全部带资产前缀，两个资产共用一个 workdir 也不覆盖。
2. **短边不达标**：f003 预测短边 1601、实际产物 1113（裁剪/补白把预测值吃掉），R1 行还是
   首判的 pass → 产物短边 <1600 就交付了。
   → 放大倍数按**实际画布**重算、尺寸一律以 `/io/read` 为准，短边不足再补一次。
"""
from __future__ import annotations

import os
import unittest

import _support

import ruleset
from behavorkit import FakeImageGateway, Image, mask_has_paint, png_size

PLUGIN = _support.load_plugin()

SHORT_MIN = int(ruleset.THRESHOLDS["R1_size"]["short_side_min"])
TEXT = [("SALE", [40, 40, 300, 120])]


class ShrinkingCrop(Image):
    """裁剪产物比请求的画布小 —— K07 A4 f003 的故障签名（预测短边 1601、实际 1113）。"""

    FACTOR = 0.7

    def cropped(self, box):
        base = Image.cropped(self, box)
        factor = self.FACTOR
        return self._spawn(max(1, int(round(base.width * factor))),
                           max(1, int(round(base.height * factor))),
                           tuple(int(round(v * factor)) for v in base.subject))


class MeasuredOnce(Image):
    """首判用**真机实测占比**（掩码只圈到主体的一部分，与「紧框面积」不等），
    变换后的产物回到面积口径（紧裁后掩码就是主体本身）—— K07 真语料就是这个形态：
    例如 a004 实测 0.3487 而紧框面积占比 0.568。"""

    def __init__(self, width, height, subject, fg_ratio):
        super().__init__(width, height, subject)
        self._fg = float(fg_ratio)

    def fg_ratio(self):
        return self._fg

    def _spawn(self, width, height, subject):
        return Image(width, height, subject)


class HalfUpscale(Image):
    """放大只给一半 —— 逼出「几何要跑第二轮」（第二轮才可能把已修好的白底弄坏）。"""

    FACTOR = 0.5

    def upscaled(self, scale):
        return Image.upscaled(self, float(scale) * self.FACTOR)


class Base(unittest.TestCase):
    def setUp(self):
        self.root = _support.tempdir("pack-repair-iso-")
        self.workdir = os.path.join(self.root, "work")
        os.makedirs(self.workdir, exist_ok=True)
        self._orig_url = PLUGIN._GATEWAY_URL
        self._orig_token = PLUGIN._GATEWAY_TOKEN
        PLUGIN._CANCEL.clear()

    def gateway(self, initial, **kwargs):
        gw = FakeImageGateway(self.root, initial, **kwargs).start()
        PLUGIN._GATEWAY_URL = gw.base_url
        PLUGIN._GATEWAY_TOKEN = gw.token
        self.addCleanup(gw.stop)
        self.addCleanup(self._restore)
        return gw

    def _restore(self):
        PLUGIN._GATEWAY_URL = self._orig_url
        PLUGIN._GATEWAY_TOKEN = self._orig_token
        PLUGIN._CANCEL.clear()

    def seed(self, gw, name, image):
        return gw.register(os.path.join(self.root, "assets", name), image)

    def repair(self, path, asset_id, rules=None, **options):
        return PLUGIN.repair("job-%s" % asset_id, {"id": asset_id, "path": path},
                             rules, dict(options, workdir=self.workdir))


class ArtifactIsolationTest(Base):
    """两个资产共用一个 workdir：产物名/掩码/中间文件互不覆盖。"""

    def test_two_assets_do_not_clobber_each_other(self):
        gw = self.gateway(Image(900, 900, (300, 300, 600, 600)))
        a = self.seed(gw, "a.png", Image(900, 900, (300, 300, 600, 600)))       # fg≈0.11
        b = self.seed(gw, "b.png", Image(1200, 800, (520, 200, 700, 620)))     # fg≈0.07

        ra = self.repair(a, "a")
        rb = self.repair(b, "b")

        paths_a = {x["path"] for x in ra["artifacts"]}
        paths_b = {x["path"] for x in rb["artifacts"]}
        self.assertTrue(paths_a and paths_b)
        self.assertFalse(paths_a & paths_b,
                         "两个资产的交付产物不许同名：%s" % (paths_a & paths_b))

        # 交付产物里的图像状态必须是**自己那份**：尺寸与 evidence 一致，且短边/占比达标
        for result in (ra, rb):
            for art in result["artifacts"]:
                state = gw.state_of(art["path"])
                self.assertEqual((art["width"], art["height"]), (state.width, state.height))
                self.assertGreaterEqual(min(state.width, state.height), SHORT_MIN)
                self.assertGreaterEqual(state.fg_ratio(),
                                        ruleset.THRESHOLDS["R4_subject_ratio"]["min"])

    def test_inpaint_mask_matches_its_own_image(self):
        """掩码必须与**正在补的那张图**同宽高（假网关照真网关口径校验，串了就 400）。"""
        # 文字框要**在裁剪后仍然可见**（假网关照真宿主口径搬坐标、裁掉画布外的字），
        # 且面积占主体 ≥8% → 判成水印（确定性 fail），才能走到 inpaint。
        text_a = [("SALE", [250, 250, 450, 450])]
        text_b = [("SALE", [400, 400, 600, 600])]
        gw = self.gateway(Image(800, 800, (200, 200, 600, 600)), overlay_lines=list(text_a))
        a = self.seed(gw, "a.png", Image(800, 800, (200, 200, 600, 600)))
        b = self.seed(gw, "b.png", Image(1200, 1000, (300, 300, 900, 900)))

        ra = self.repair(a, "a")
        gw.overlay_lines = list(text_b)    # 假网关成功补一次就会清空文字；第二个资产要重新摆上
        rb = self.repair(b, "b")

        self.assertTrue(ra["artifacts"], "A 的 R3 修复应有产物：%s" % ra.get("trail"))
        self.assertTrue(rb["artifacts"], "B 的 R3 修复应有产物：%s" % rb.get("trail"))

        inpaints = [body for path, body in gw.requests if path == "/op/inpaint"]
        self.assertEqual(len(inpaints), 2, "两个资产各应 inpaint 一次：%s" % inpaints)
        masks = set()
        for body in inpaints:
            mask = body["mask"]
            masks.add(mask)
            image = gw.state_of(body["path"])
            self.assertEqual(png_size(mask), (image.width, image.height),
                             "掩码尺寸必须等于被补图面 %dx%d" % (image.width, image.height))
        self.assertEqual(len(masks), 2, "两个资产的掩码不许同名：%s" % masks)


class ShortSideGateTest(Base):
    """宿主把画布钳小 / 应答尺寸不可信时，产物短边仍必须达到 R1 门槛。"""

    def test_shrunk_crop_still_reaches_short_side(self):
        """宿主只把**失败规则**交给 repair（生产实测 f003 的入参是 [R2,R4]，没有 R1）：
        裁剪把画布缩到短边 1113 也没人复判 R1 → 短边保障必须自己兜住，不能指望 R1 再判一轮。
        """
        gw = self.gateway(Image(1200, 800, (350, 250, 850, 650)))
        a = self.seed(gw, "a.png", ShrinkingCrop(1200, 800, (350, 250, 850, 650)))
        result = self.repair(a, "a", rules=["R4_subject_ratio"])

        ups = [body for path, body in gw.requests if path == "/op/upscale"]
        self.assertTrue(ups, "裁剪后短边不足必须触发 /op/upscale")
        self.assertTrue(result["artifacts"], "应有 R4 修复产物：%s" % result.get("trail"))
        for art in result["artifacts"]:
            state = gw.state_of(art["path"])
            self.assertGreaterEqual(min(state.width, state.height), SHORT_MIN,
                                    "短边没到 %d：%dx%d" % (SHORT_MIN, state.width, state.height))

    def test_dims_prefer_the_file_over_the_response(self):
        """产物尺寸以 `/io/read` 为准：宿主应答的 width/height 不可信。"""
        gw = self.gateway(Image(640, 480, (200, 160, 440, 400)))
        a = self.seed(gw, "a.png", Image(640, 480, (200, 160, 440, 400)))
        self.assertEqual(PLUGIN._dims_of(a, {"width": 4000, "height": 4000}), (640, 480))
        self.assertEqual(PLUGIN._dims_of(a), (640, 480))


class WhiteMarginTest(Base):
    """白底待修时几何要留「背景边」：主体贴边裁不出白边，换底也救不回边缘白度。"""

    def test_subject_bleeding_to_edge_gets_background_margin(self):
        # f002 实测：主体紧框 [1104,56,3492,2428] 直抵画面上沿、左右几乎贴满
        image = MeasuredOnce(3840, 2564, (1104, 56, 3492, 2428), 0.3608)
        gw = self.gateway(image, white_bg="fail")
        a = self.seed(gw, "f002.png",
                      MeasuredOnce(3840, 2564, (1104, 56, 3492, 2428), 0.3608))

        result = self.repair(a, "f002", rules=["R2_white_bg", "R4_subject_ratio"])
        verdicts = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(verdicts["R2_white_bg"]["status"], "pass", verdicts["R2_white_bg"])

        art = next(x for x in result["artifacts"] if x["rule_id"] == "R4_subject_ratio")
        state = gw.state_of(art["path"])
        sx0, sy0, sx1, sy1 = state.subject
        self.assertGreater(sx0, 0, "左边没有背景边 → 边缘白度必然 fail")
        self.assertGreater(sy0, 0, "上边没有背景边 → 边缘白度必然 fail")
        self.assertLess(sx1, state.width, "右边没有背景边")
        self.assertLess(sy1, state.height, "下边没有背景边")
        self.assertGreaterEqual(state.fg_ratio(), ruleset.THRESHOLDS[
            "R4_subject_ratio"]["min"])


class StaleOverlayBoxTest(Base):
    """文字框是**在哪个画布上量的**，就必须在哪个画布上擦。

    K07-FIX-G-B 实测（f002）：R3 evidence 的框量在 3840×2564 的原图上，几何裁剪把画布换成
    1688×1688 后 repair 还在复用旧框 → 框整块落在画布外 → 掩码被夹成**全黑** → 真网关
    `/op/inpaint` 500「掩码全空：无待补洞区域」→ 整条修复 aborted。
    画布变了必须重新量（或换算出新坐标），不许拿旧框硬擦。
    """

    def test_stale_overlay_boxes_are_remeasured_after_canvas_change(self):
        subject = (1104, 56, 3200, 2428)
        # 促销条贴在主体右侧之外（面积 0.6% < 2% → 走 VL 裁量；overlay_vl=fail 判成叠加文字）
        text = [("SALE", [3250, 1900, 3450, 2200])]
        gw = self.gateway(MeasuredOnce(3840, 2564, subject, 0.3608),
                          overlay_lines=list(text), overlay_vl="fail")
        a = self.seed(gw, "stale.png", MeasuredOnce(3840, 2564, subject, 0.3608))

        result = self.repair(a, "stale")

        inpaints = [body for path, body in gw.requests if path == "/op/inpaint"]
        self.assertTrue(inpaints, "R3 应走到 inpaint：%s" % result.get("trail"))
        for body in inpaints:
            self.assertTrue(mask_has_paint(body["mask"]),
                            "掩码全空 → 真网关会 500：%s" % body)
            state = gw.state_of(body["path"])
            self.assertEqual(png_size(body["mask"]), (state.width, state.height))
            self.assertTrue(result["artifacts"] or result.get("verdicts"),
                            "修复链不许整体 aborted：%s" % result)


class RealSampleRegressionTest(Base):
    """真实语料原图尺寸回归：f003 / a004（K07 重跑时这两张产物短边不达标）。"""

    def test_f003_and_a004_source_sizes_hit_all_three_invariants(self):
        # (名字, 宽, 高, 实测 dominant_bbox, 实测 fg_ratio)
        cases = [("f003", 3840, 2555, (936, 718, 2016, 1661), 0.113535),
                 ("a004", 2146, 2146, (207, 335, 1925, 1858), 0.348662)]
        for name, width, height, subject, fg in cases:
            with self.subTest(asset=name):
                gw = self.gateway(MeasuredOnce(width, height, subject, fg))
                a = self.seed(gw, name + ".png",
                              MeasuredOnce(width, height, subject, fg))
                result = self.repair(a, name, rules=["R4_subject_ratio"])
                arts = [x for x in result["artifacts"]
                        if x["rule_id"] == "R4_subject_ratio"]
                self.assertTrue(arts, "%s 未产出成品：%s" % (name, result.get("trail")))
                state = gw.state_of(arts[0]["path"])
                self.assertGreaterEqual(min(state.width, state.height), SHORT_MIN, name)
                self.assertGreaterEqual(state.fg_ratio(),
                                        ruleset.THRESHOLDS["R4_subject_ratio"]["min"], name)
                self.assertLessEqual(state.fg_ratio(),
                                     ruleset.THRESHOLDS["R4_subject_ratio"]["max"], name)


if __name__ == "__main__":
    unittest.main()
