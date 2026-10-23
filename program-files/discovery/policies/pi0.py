#!/usr/bin/env python3
"""pi_0: the initial exploration policy, generation 0, as a policy program.

The same rule as the discovery harness's hand-written ``InitialPolicy``
(program-files/discovery), so the compiled-in Rust policy and this program make
identical decisions:

- only the root revealed (round 1): ``W`` copies of the root, node id 0 --
  open ``W`` branches;
- later, over the continuable nodes ordered by (score desc, depth asc, id
  asc): a node whose ``check.status`` is ``invalid`` is never continued. A
  hollow node is: under the tool loop its file may restate the theorem
  while its workspace and transcript hold the work behind it, and continuing
  it resumes that work where re-rooting throws it away; the score ordering
  already puts credited sketches first. One slot is reserved for
  the child with the best repair prospects when one exists -- an ``errors``
  leaf whose ``errors_obs.repairable`` is true, the fewest errors first,
  then the latest ``first_error_fraction``, then the ordering; else the
  ``sorries`` leaf with the most ``sketch.closed`` steps, ties by the
  ordering -- which goes first; the remaining slots follow the ordering,
  each node once.

A missing observation field means unknown: not invalid, not repairable,
nothing closed. It never returns an empty batch, so it never
stops on its own; stopping is the driver's (solved, K rounds, budget, no
continuation left).

Protocol version 1 (program-files/discovery/src/policy/process.rs): one JSON request per
line on stdin, one JSON reply per line on stdout, until EOF. To try it by hand,
feed it one request line; this answers ``{"batch": [0, 0, 0]}``:

    echo '{"v":1,"round":1,"w":3,"revealed":[{"id":0,"score":0.0,"parent":null,"depth":0}],"continuable":[0]}' | python3 program-files/discovery/policies/pi0.py

This program holds no capability and needs none: stdlib only, no imports
beyond json and sys, no files, no network.
"""

import json
import sys


def field(node, *keys):
    """A nested request field, or None when any level is missing."""
    for key in keys:
        node = node.get(key) if isinstance(node, dict) else None
    return node


def decide(request):
    w = int(request["w"])
    revealed = request["revealed"]
    if len(revealed) <= 1:
        return [0] * w
    nodes = {node["id"]: node for node in revealed}

    def info(v):
        return nodes.get(v) or {}

    ordered = sorted(
        request["continuable"],
        key=lambda v: (-float(info(v).get("score", 0.0)), int(info(v).get("depth", 0)), v),
    )
    eligible = [
        v
        for v in ordered
        if v == 0 or field(info(v), "check", "status") != "invalid"
    ]
    repair, sketch = [], []
    for position, v in enumerate(eligible):
        if v == 0:
            continue
        node = info(v)
        status = field(node, "check", "status")
        if status == "errors" and field(node, "errors_obs", "repairable") is True:
            fraction = field(node, "errors_obs", "first_error_fraction")
            latest = -float(fraction) if isinstance(fraction, (int, float)) else 1.0
            repair.append((int(field(node, "check", "errors") or 0), latest, position, v))
        elif status == "sorries":
            sketch.append((-int(field(node, "sketch", "closed") or 0), position, v))
    reserved = min(repair)[-1] if repair else (min(sketch)[-1] if sketch else None)
    batch = [reserved] if reserved is not None else []
    batch.extend(v for v in eligible if v != reserved)
    return batch[:w]


def main():
    for line in iter(sys.stdin.readline, ""):
        line = line.strip()
        if not line:
            continue
        request = json.loads(line)
        if request.get("v") != 1:
            sys.stderr.write("pi0: unsupported protocol version %r\n" % (request.get("v"),))
            sys.exit(2)
        sys.stdout.write(json.dumps({"batch": decide(request)}) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
