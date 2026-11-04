# /// script
# requires-python = "==3.12.*"
# dependencies = ["pyyaml==6.0.1"]
# ///
"""Reads cloud-init seeds the way cloud-init does (PyYAML, SafeLoader) and reports what PyYAML read. Compares nothing.

Usage: seed_pyyaml.py CASES.json OUT.json

CASES.json is a list of {meta_data, user_data}: the text of the two seed files.

OUT.json is a list with one entry per case, in order. An entry is one of:

- an object with the keys "meta hostname", "user hostname", "client_public", "host_private" and "host_public". Each
  value is what PyYAML read there: a string as the list of its code points (ints, so a surrogate that PyYAML makes from
  a "\\ud83d" escape travels like any other code point), anything else as {"type": <type name>, "repr": <ASCII-escaped
  repr>};
- {"error": "<exception type>: <first line of its message>"}, ASCII-escaped, when any exception is raised while
  loading or navigating the case's documents (a file PyYAML cannot load, a key the document lacks, a value of the wrong
  shape).

OUT.json is therefore ASCII-safe JSON whatever the seed holds.

Exits 0 after writing OUT.json. Any other exit, including a missing key in a case, means this script did not run to
completion.
"""

import json
import sys

import yaml


def load(text):
    return yaml.load(text, Loader=yaml.SafeLoader)


def escaped(text):
    return text.encode("ascii", "backslashreplace").decode("ascii")


def describe(value):
    if isinstance(value, str):
        return [ord(c) for c in value]
    return {"type": type(value).__name__, "repr": escaped(repr(value))}


def read_back(meta_data, user_data):
    meta = load(meta_data)
    user = load(user_data)
    return {
        "meta hostname": describe(meta["local-hostname"]),
        "user hostname": describe(user["hostname"]),
        "client_public": describe(user["users"][0]["ssh_authorized_keys"][0]),
        "host_private": describe(user["ssh_keys"]["ed25519_private"]),
        "host_public": describe(user["ssh_keys"]["ed25519_public"]),
    }


def main(cases_path, out_path):
    with open(cases_path, encoding="utf-8") as f:
        cases = json.load(f)
    results = []
    for case in cases:
        meta_data = case["meta_data"]
        user_data = case["user_data"]
        try:
            results.append(read_back(meta_data, user_data))
        except Exception as e:
            first_line = (str(e).splitlines() or [""])[0]
            results.append({"error": escaped(f"{type(e).__name__}: {first_line}")})
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(results, f)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
