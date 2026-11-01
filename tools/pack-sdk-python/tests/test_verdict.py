"""Verdict 构造器：七字段齐全、status 闭集、undetermined 必须带 note。"""
from __future__ import annotations

import unittest

import _support                                   # noqa: F401
from dampack import (ERR_INVALID_PARAMS, STATUSES, VERDICT_FIELDS, RpcError,
                     failed, passed, skipped, undetermined, validate_verdict,
                     verdict)


class BuilderTest(unittest.TestCase):
    def test_all_fields_present(self):
        item = verdict("R1_size", "pass", score=1.0, label="短边")
        self.assertEqual(set(item), set(VERDICT_FIELDS))
        self.assertEqual(validate_verdict(item), [])
        self.assertTrue(item["fixed"] is False)
        self.assertIsNone(item["note"])
        self.assertEqual(item["evidence"], {})

    def test_helpers(self):
        self.assertEqual(passed("R1").get("status"), "pass")
        self.assertEqual(failed("R1").get("status"), "fail")
        self.assertEqual(skipped("R1", "no_measurement").get("note"), "no_measurement")
        self.assertEqual(undetermined("R1", "vl_undetermined").get("status"), "undetermined")

    def test_score_coerced_to_float(self):
        item = verdict("R1_size", "pass", score=2336)
        self.assertIsInstance(item["score"], float)
        self.assertEqual(item["score"], 2336.0)

    def test_fixed_flag(self):
        self.assertTrue(verdict("R1_size", "pass", fixed=True)["fixed"])

    def test_evidence_is_copied(self):
        source = {"k": 1}
        item = verdict("R1", "pass", evidence=source)
        source["k"] = 2
        self.assertEqual(item["evidence"], {"k": 1})


class GuardTest(unittest.TestCase):
    def test_bad_status(self):
        with self.assertRaises(RpcError) as ctx:
            verdict("R1", "maybe")
        self.assertEqual(ctx.exception.code, ERR_INVALID_PARAMS)
        self.assertEqual(set(STATUSES), {"pass", "fail", "undetermined", "skipped"})

    def test_undetermined_requires_note(self):
        with self.assertRaises(RpcError) as ctx:
            undetermined("R1", "")
        self.assertEqual(ctx.exception.code, ERR_INVALID_PARAMS)

    def test_validate_catches_missing_field(self):
        problems = validate_verdict({"ruleId": "R1", "status": "pass"})
        self.assertTrue(problems and problems[0].startswith("缺字段"), problems)

    def test_validate_catches_bad_types(self):
        item = verdict("R1", "pass")
        item["score"] = "high"
        item["evidence"] = []
        item["fixed"] = "yes"
        problems = validate_verdict(item)
        self.assertEqual(len(problems), 3, problems)


if __name__ == "__main__":
    unittest.main()
