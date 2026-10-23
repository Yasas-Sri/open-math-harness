#!/usr/bin/env python3
"""Keep pi0 continuations and add fresh root attempts: batches of 2, 4, 8, then 16.

A task selects it with `"policy": "pi_doubling.py"`. Round 1 is two root
attempts; rounds 2, 3 and 4 onwards hold 4, 8 and 16, half by the pi0 rule and
half appended as fresh root attempts, even when high-scoring parents could
fill every slot. Pi0 itself may choose the root too; its ordering and repair
preference are unchanged. Slots of pi0's half that it cannot fill with
distinct eligible nodes also become root attempts, so while the root is
continuable every batch has the schedule's size. When the driver offers a
smaller `w`, the batch is capped at `w` and an odd slot goes to pi0's half;
while the root is not continuable no root is added and round 1 is empty.

The driver supplies the decision counter `round`, so the policy keeps no state of its own. A
resumed rollout starts a new policy process, whose schedule starts again at
round 1.

It imports pi0.py from its own directory (Python puts a program's directory
first on its module path, even with an empty environment), so the two ship
together in the runtime's share/policies, which the VM composition binds at
/opt/nucleus/share/policies. Its tests are tools/test_pi_doubling.py, outside
this directory because everything here ships, and a policy test of
tests/tests/discovery.rs.
"""
import json
import sys

# Policies are mounted read-only and run with an empty environment.
sys.dont_write_bytecode = True
from pi0 import decide as choose_nodes


def decide(request):
    round_number, width = request["round"], request["w"]
    if type(round_number) is not int or round_number < 1:
        raise ValueError("round must be a positive integer")
    if type(width) is not int or width < 0:
        raise ValueError("w must be a nonnegative integer")
    effective = min(width, 2 ** min(round_number, 4))
    root_available = 0 in request["continuable"]
    if round_number == 1:
        return [0] * effective if root_available else []
    # A budget-limited odd batch gives its extra slot to the original rule.
    # Never replace a missing fresh-root slot with a fabricated node, nor
    # silently spend that reserved fresh quota on more inherited selections.
    fresh = effective // 2
    original = choose_nodes({**request, "w": effective - fresh})
    if not root_available:
        return original
    # pi0.py leaves slots empty when too few distinct nodes are eligible;
    # they are root attempts too, like the fresh quota after them.
    return original + [0] * (effective - len(original))


def main():
    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        if request.get("v") != 1:
            raise ValueError("unsupported policy protocol version")
        sys.stdout.write(json.dumps({"batch": decide(request)}) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError) as error:
        sys.stderr.write(f"pi_doubling: {error}\n")
        sys.exit(2)
