# /// script
# requires-python = "==3.12.*"
# dependencies = ["pyyaml==6.0.1"]
# ///
"""Loads cloud-init seeds the way cloud-init does (PyYAML, SafeLoader) and checks every field reads back exactly.

Usage: seed_pyyaml.py CASES.json, a list of {where, meta_data, user_data, hostname, client_public, host_private,
host_public}. `where` names the case in a failure.

Exits 0 when every case reads back exactly. A mismatch exits 3 with the line `mismatch` first on stdout and one failure
per line on stderr. Any other exit or output is not a mismatch: `uv` exits 1 or 2 when it cannot resolve PyYAML, 2 when
PyYAML cannot be imported, 1 on an uncaught exception.
"""

import json
import sys

try:
    import yaml
except ImportError as e:
    print(f"cannot import PyYAML: {e}", file=sys.stderr)
    sys.exit(2)


def load(text):
    return yaml.load(text, Loader=yaml.SafeLoader)


def read_back(case):
    meta = load(case["meta_data"])
    user = load(case["user_data"])
    return {
        "meta hostname": meta["local-hostname"],
        "user hostname": user["hostname"],
        "client_public": user["users"][0]["ssh_authorized_keys"][0],
        "host_private": user["ssh_keys"]["ed25519_private"],
        "host_public": user["ssh_keys"]["ed25519_public"],
    }


def expected(case):
    return {
        "meta hostname": case["hostname"],
        "user hostname": case["hostname"],
        "client_public": case["client_public"],
        "host_private": case["host_private"],
        "host_public": case["host_public"],
    }


def differences(case):
    """One line per field of `case` that does not read back exactly."""
    got = read_back(case)
    lines = []
    for field, want in expected(case).items():
        if got[field] == want:
            continue
        if not isinstance(got[field], str):
            lines.append(f"{field} reads back as {type(got[field]).__name__}: {got[field]!r:.40}")
            continue
        at = next(
            (i for i, (a, b) in enumerate(zip(got[field], want)) if a != b),
            min(len(got[field]), len(want)),
        )
        lines.append(f"{field} differs at index {at}: {got[field][at : at + 8]!r} for {want[at : at + 8]!r}")
    return lines


def main(path):
    with open(path, encoding="utf-8") as f:
        cases = json.load(f)
    failures = []
    for case in cases:
        where = case["where"]
        try:
            failures += [f"{where}: {line}" for line in differences(case)]
        except Exception as e:  # a seed that does not load or compare at all
            failures.append(f"{where}: does not read back: {type(e).__name__}: {e}".splitlines()[0])
    if failures:
        print("mismatch")
        print("\n".join(failures), file=sys.stderr)
        return 3
    print(f"{len(cases)} cases read back exactly")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
