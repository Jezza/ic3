#!/usr/bin/env python3
"""Regenerate crates/ic3-teams/src/emoji.tsv from Teams' public emoji metadata.

    python3 -I tools/teams-emoji/gen.py [METADATA_URL]

Teams names reactions by its own emoticon ids (`like`, `steamtrain`,
`1f3aa_circustent`, …). The metadata file maps each id to its Unicode emoji.
Its URL carries a hash that changes with Teams releases; find the current one
in teams-for-linux's cache (`grep -rao 'personal-expressions/v1/metadata/[^"?]*'
~/.config/teams-for-linux/Partitions`) or a capture.

Output: one `id<TAB>emoji<TAB>popularity rank` line per emoticon, sorted by id.
"""

import json
import os
import sys
import urllib.request

DEFAULT = (
    "https://teams.public.onecdn.static.microsoft/evergreen-assets/personal-expressions/"
    "v1/metadata/ec4576179210cde40ce5494513213583/en-us.json"
)
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.path.join(REPO, "crates", "ic3-teams", "src", "emoji.tsv")


def main(argv):
    url = argv[0] if argv else DEFAULT
    with urllib.request.urlopen(url, timeout=30) as r:
        data = json.load(r)
    rows = {}
    for cat in data["categories"]:
        for e in cat["emoticons"]:
            key, uni = e.get("id"), e.get("unicode")
            if not key or not uni or "\t" in key + uni or "\n" in key + uni:
                continue
            rank = e.get("popularityRank")
            rows[key] = (uni, rank if isinstance(rank, int) else 99999)
    with open(OUT, "w", encoding="utf-8") as f:
        for key in sorted(rows):
            uni, rank = rows[key]
            f.write(f"{key}\t{uni}\t{rank}\n")
    print(f"wrote {len(rows)} emoticons to {OUT}")


if __name__ == "__main__":
    main(sys.argv[1:])
