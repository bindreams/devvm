# /// script
# requires-python = ">=3.9"
# dependencies = ["pyyaml>=6,<7"]
# ///
"""Loads cloud-init seeds the way cloud-init does (PyYAML, SafeLoader) and checks every field reads back exactly.

Usage: seed_pyyaml.py CASES.json, a list of {where, meta_data, user_data, hostname, client_public, host_private,
host_public}. `where` names the case in a failure.
"""

import json
import sys

import yaml


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


def main(path):
    with open(path, encoding="utf-8") as f:
        cases = json.load(f)
    failures = []
    for case in cases:
        where = case["where"]
        try:
            got = read_back(case)
        except Exception as e:  # a seed that does not load at all
            failures.append(f"{where}: does not load: {type(e).__name__}: {e}".splitlines()[0])
            continue
        want = expected(case)
        for field in want:
            if got[field] != want[field]:
                at = next(
                    (i for i, (a, b) in enumerate(zip(got[field], want[field])) if a != b),
                    min(len(got[field]), len(want[field])),
                )
                failures.append(f"{where}: {field} differs at index {at}: {got[field][at : at + 8]!r} for {want[field][at : at + 8]!r}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(f"{len(cases)} cases read back exactly")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
