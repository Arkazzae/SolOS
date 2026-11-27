"""Project the Sol game tables into the offline browser catalogue."""
import argparse
import csv
import json
import re
from pathlib import Path

OUTPUT = Path(__file__).resolve().parents[1] / "src/data/gamedb"


def project(source, system):
    entries = []
    seen = set()
    with (source / f"{system}.tsv").open(encoding="utf-8", newline="") as table:
        rows = csv.DictReader((line for line in table if not line.startswith("#")), delimiter="\t")
        for row in rows:
            crc = row["crc"].upper()
            if not re.fullmatch(r"[0-9A-F]{8}", crc) or crc in seen:
                raise ValueError(f"Invalid or duplicate {system} CRC: {crc}")
            seen.add(crc)
            game = {"title": row["name"]}
            if row["year"]:
                game["year"] = int(row["year"])
            if row["developer"]:
                game["studio"] = row["developer"]
            if row["players"]:
                game["players"] = int(row["players"])
            entries.append(f'  "{crc}": {json.dumps(game, ensure_ascii=False)}')
    (OUTPUT / f"{system}.json").write_text("{\n" + ",\n".join(entries) + "\n}\n", encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tables", type=Path, help="directory containing sms.tsv and gg.tsv")
    args = parser.parse_args()
    for system in ("sms", "gg"):
        project(args.tables, system)
