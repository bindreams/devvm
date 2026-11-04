# /// script
# requires-python = "==3.12.*"
# dependencies = ["pyyaml==6.0.1"]
# ///
"""Reads cloud-init seeds the way cloud-init does (PyYAML, SafeLoader), compares what PyYAML read with what the seed
should hold, and reports each difference as one line of text.

Usage: seed_pyyaml.py CASES.json OUT.json

CASES.json is a list of cases: {label, meta_data, user_data, expected}. meta_data and user_data are the text of the two
seed files. expected is a list of five [field, value] pairs, the fields being "meta hostname", "user hostname",
"client_public", "host_private" and "host_public".

OUT.json is a list of lines, one per difference, empty when every field reads back as expected. The lines are:

- "<field> is not a field this reader reads", once for each field name in CASES that is not one of FIELDS;
- "<label>: does not read back: <Type>: <first line of the message>" when an exception is raised while loading or
  navigating the case's documents (a file PyYAML cannot load, a key the document lacks, a value of the wrong shape);
- "<label>: <field> reads back as <type> <repr>" when the value PyYAML read is not a str;
- "<label>: <field> differs at code point <i> (read <n> code points, expected <m>): [<window>] for [<window>]" when it
  is a str that differs. <i> is the first code point that differs. A window is the 4 code points before <i>, then "|",
  then the 8 from <i> on, space-separated; a code point from "!" to "~" is shown as itself and any other (a space
  included) as U+XXXX.

An error message or repr is escaped to ASCII (a backslash doubled, any other non-ASCII code point as a Python escape),
so an escape in the line always stands for one code point. Escaped text longer than 60 characters is cut at the last
code point that fits and followed by "…(the message is N code points long)" or "…(the repr is N code points
long)", N counting the unescaped text; the marker is the only non-ASCII text in a line.

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


FIELDS = ("meta hostname", "user hostname", "client_public", "host_private", "host_public")


def read_back(meta_data, user_data):
    meta = load(meta_data)
    user = load(user_data)
    keys = user["ssh_keys"]
    values = (
        meta["local-hostname"],
        user["hostname"],
        user["users"][0]["ssh_authorized_keys"][0],
        keys["ed25519_private"],
        keys["ed25519_public"],
    )
    return dict(zip(FIELDS, values, strict=True))


def escaped(text, what):
    """`text` as ASCII, a backslash doubled and any other non-ASCII code point as a Python escape, cut between code
    points once SHOWN characters are shown."""
    pieces = [c.replace("\\", "\\\\").encode("ascii", "backslashreplace").decode("ascii") for c in text]
    width = 0
    for i, piece in enumerate(pieces):
        width += len(piece)
        if width > SHOWN:
            return f"{''.join(pieces[:i])}…(the {what} is {len(text)} code points long)"
    return "".join(pieces)


def show(points):
    return " ".join(chr(p) if 0x21 <= p <= 0x7E else f"U+{p:04X}" for p in map(ord, points))


def difference(label, field, got, want):
    if not isinstance(got, str):
        return f"{label}: {field} reads back as {type(got).__name__} {escaped(repr(got), 'repr')}"
    if got == want:
        return None
    at = next((i for i, (a, b) in enumerate(zip(got, want)) if a != b), min(len(got), len(want)))
    start = max(at - BEFORE, 0)

    def window(text):
        return f"[{show(text[start:at])} | {show(text[at : at + AFTER])}]"

    return (
        f"{label}: {field} differs at code point {at} (read {len(got)} code points, expected {len(want)}): "
        f"{window(got)} for {window(want)}"
    )


def compare(case):
    label = case["label"]
    try:
        values = read_back(case["meta_data"], case["user_data"])
    except Exception as e:
        first_line = (str(e).splitlines() or [""])[0]
        return [f"{label}: does not read back: {type(e).__name__}: {escaped(first_line, 'message')}"]
    lines = (difference(label, field, values[field], want) for field, want in case["expected"] if field in FIELDS)
    return [line for line in lines if line is not None]


def main(cases_path, out_path):
    with open(cases_path, encoding="utf-8") as f:
        cases = json.load(f)
    named = dict.fromkeys(field for case in cases for field, _ in case["expected"])
    lines = [f"{field} is not a field this reader reads" for field in named if field not in FIELDS]
    for case in cases:
        lines.extend(compare(case))
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(lines, f)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
