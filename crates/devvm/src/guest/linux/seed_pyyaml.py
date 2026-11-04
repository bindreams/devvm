# /// script
# requires-python = "==3.12.*"
# dependencies = ["pyyaml==6.0.1"]
# ///
"""Reads cloud-init seeds the way cloud-init does (PyYAML, SafeLoader), compares what PyYAML read with what the seed
should hold, and reports each difference as one line of ASCII text.

Usage: seed_pyyaml.py CASES.json OUT.json

CASES.json is a list of cases: {label, meta_data, user_data, expected}. meta_data and user_data are the text of the two
seed files. expected is a list of five [field, value] pairs, the fields being "meta hostname", "user hostname",
"client_public", "host_private" and "host_public".

OUT.json is a list of strings, one per difference, empty when every field reads back as expected. The lines are:

- "<label>: does not read back: <Type>: <first line of the message>" when an exception is raised while loading or
  navigating the case's documents (a file PyYAML cannot load, a key the document lacks, a value of the wrong shape);
- "<label>: <field> reads back as <type> <repr>" when the value PyYAML read is not a str;
- "<label>: <field> differs at code point <i> (read <n> code points, expected <m>): <window> for <window>" when it is a
  str that differs. <i> is the first code point that differs. A window is the 4 code points before <i> and the 8 from
  <i> on, space-separated; a code point from "!" to "~" is shown as itself and any other (a space included) as U+XXXX.

An error message or repr longer than 60 characters is cut there and followed by "(... is N characters long)", N
counting the unescaped text. Every line is escaped once, so OUT.json is ASCII whatever the seed holds.

Exits 0 after writing OUT.json. Any other exit, including a missing key in a case, means this script did not run to
completion.
"""

import json
import sys

import yaml

SHOWN = 60
BEFORE = 4
AFTER = 8


def load(text):
    return yaml.load(text, Loader=yaml.SafeLoader)


def read_back(meta_data, user_data):
    meta = load(meta_data)
    user = load(user_data)
    return {
        "meta hostname": meta["local-hostname"],
        "user hostname": user["hostname"],
        "client_public": user["users"][0]["ssh_authorized_keys"][0],
        "host_private": user["ssh_keys"]["ed25519_private"],
        "host_public": user["ssh_keys"]["ed25519_public"],
    }


def shortened(text, what):
    if len(text) <= SHOWN:
        return text
    return f"{text[:SHOWN]}...(the {what} is {len(text)} characters long)"


def show(points):
    return " ".join(chr(p) if 0x21 <= p <= 0x7E else f"U+{p:04X}" for p in map(ord, points))


def difference(label, field, got, want):
    if not isinstance(got, str):
        return f"{label}: {field} reads back as {type(got).__name__} {shortened(repr(got), 'repr')}"
    if got == want:
        return None
    at = next((i for i, (a, b) in enumerate(zip(got, want)) if a != b), min(len(got), len(want)))
    window = slice(max(at - BEFORE, 0), at + AFTER)
    return (
        f"{label}: {field} differs at code point {at} (read {len(got)} code points, expected {len(want)}): "
        f"[{show(got[window])}] for [{show(want[window])}]"
    )


def compare(case):
    label = case["label"]
    try:
        values = read_back(case["meta_data"], case["user_data"])
    except Exception as e:
        first_line = (str(e).splitlines() or [""])[0]
        return [f"{label}: does not read back: {type(e).__name__}: {shortened(first_line, 'message')}"]
    lines = (difference(label, field, values[field], want) for field, want in case["expected"])
    return [line for line in lines if line is not None]


def main(cases_path, out_path):
    with open(cases_path, encoding="utf-8") as f:
        cases = json.load(f)
    lines = []
    for case in cases:
        lines.extend(line.encode("ascii", "backslashreplace").decode("ascii") for line in compare(case))
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(lines, f)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
