"""Keyless tests of the shipped pi_doubling.py policy, including its line protocol.

They live here, not beside the policy, because everything in
program-files/discovery/policies ships in the VM runtime's share/.
"""
import json
from pathlib import Path
import subprocess
import sys
import unittest

POLICIES = Path(__file__).resolve().parent.parent / "program-files/discovery/policies"
sys.dont_write_bytecode = True  # never leave __pycache__ in a shipped directory
sys.path.insert(0, str(POLICIES))
import pi0
import pi_doubling


def request(round_number=1, width=16):
    return {"v": 1, "round": round_number, "w": width,
            "revealed": [{"id": 0, "score": 0.0, "depth": 0, "parent": None}], "continuable": [0]}


def rich_frontier(round_number=2, width=16):
    """Twenty `sorries` leaves: pi0 fills any half from them without the root."""
    r = request(round_number, width)
    r["revealed"] += [{"id": n, "score": 1.0 - n / 100, "depth": 1, "parent": 0,
                       "check": {"status": "sorries"}} for n in range(1, 21)]
    r["continuable"] = list(range(1, 21)) + [0]
    return r


class DoublingPolicy(unittest.TestCase):
    def test_schedule_doubles_to_sixteen_within_the_affordable_width(self):
        self.assertEqual([len(pi_doubling.decide(request(n))) for n in range(1, 8)], [2, 4, 8, 16, 16, 16, 16])
        self.assertEqual(len(pi_doubling.decide(request(1000000, 32))), 16)
        for affordable in range(20):
            r = rich_frontier(5, affordable)
            half = min(affordable, 16) // 2
            with self.subTest(affordable=affordable):
                # An odd budget-limited batch gives its extra slot to pi0's half.
                self.assertEqual(pi_doubling.decide(r), pi0.decide({**r, "w": min(affordable, 16) - half}) + [0] * half)

    def test_fresh_roots_are_added_even_when_good_parents_could_fill_the_batch(self):
        for round_number, half in [(2, 2), (3, 4), (4, 8), (1000000, 8)]:
            r = rich_frontier(round_number)
            before = json.dumps(r, sort_keys=True)
            original = pi0.decide({**r, "w": half})
            self.assertNotIn(0, original)
            with self.subTest(round=round_number):
                self.assertEqual(pi_doubling.decide(r), original + [0] * half)
                self.assertEqual(json.dumps(r, sort_keys=True), before, "the request is not changed")

    def test_pi0_keeps_its_repair_preference_in_its_half(self):
        r = request(2)
        r["revealed"] += [
            {"id": 1, "score": 0.8, "depth": 1, "parent": 0, "check": {"status": "sorries"}},
            {"id": 2, "score": 0.2, "depth": 1, "parent": 0, "check": {"status": "errors", "errors": 1},
             "errors_obs": {"repairable": True, "first_error_fraction": 0.9}},
            {"id": 3, "score": 0.9, "depth": 1, "parent": 0, "check": {"status": "invalid"}},
        ]
        r["continuable"] = [3, 1, 2, 0]
        self.assertEqual(pi_doubling.decide(r), [2, 1, 0, 0])

    def test_slots_pi0_cannot_fill_become_root_attempts(self):
        r = request(3)
        r["revealed"].append({"id": 1, "score": 0.9, "depth": 1, "parent": 0})
        r["continuable"] = [1, 0]
        self.assertEqual(pi0.decide({**r, "w": 4}), [1, 0], "pi0 v3 names each node once")
        self.assertEqual(pi_doubling.decide(r), [1, 0, 0, 0] + [0, 0, 0, 0])

    def test_an_unavailable_root_is_never_inserted_and_its_slots_are_not_reallocated(self):
        r = rich_frontier(3)
        r["continuable"].remove(0)
        self.assertEqual(pi_doubling.decide(r), pi0.decide({**r, "w": 4}))
        self.assertEqual(pi_doubling.decide({**r, "round": 1}), [])
        self.assertEqual(pi_doubling.decide({**r, "continuable": []}), [])

    def test_bad_round_or_width_is_refused(self):
        for field, value in [("round", 0), ("round", True), ("round", "2"), ("w", -1), ("w", True)]:
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                pi_doubling.decide({**request(), field: value})

    def test_line_protocol_imports_pi0_without_pythonpath_or_environment(self):
        program = str(POLICIES / "pi_doubling.py")
        requests = [request(1)] + [rich_frontier(n) for n in [2, 2, 3, 4]]
        result = subprocess.run([sys.executable, "-B", program], input="".join(json.dumps(r) + "\n" for r in requests),
                                text=True, capture_output=True, env={}, cwd="/", timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        batches = [json.loads(line)["batch"] for line in result.stdout.splitlines()]
        self.assertEqual([len(b) for b in batches], [2, 4, 4, 8, 16], "the same request repeats its decision")
        self.assertEqual([b.count(0) for b in batches], [2, 2, 2, 4, 8])
        bad = subprocess.run([sys.executable, "-B", program], input=json.dumps({**request(), "v": 2}) + "\n",
                             text=True, capture_output=True, env={}, cwd="/", timeout=10)
        self.assertEqual((bad.returncode, bad.stdout), (2, ""))


if __name__ == "__main__":
    unittest.main()
