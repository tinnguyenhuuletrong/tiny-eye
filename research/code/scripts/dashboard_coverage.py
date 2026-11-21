#!/usr/bin/env python3
"""Construct-level coverage of real Grafana panels by TQS 3.

Reads the dashboard record that margince-qc keeps (`shared/stag-dashboards.csv`: one line per
panel, PromQL queries joined by ' ⏐ '), lists every PromQL construct the panels use, and says
for each panel whether all of its constructs have a TQS equivalent. Writes aggregate counts
only: no dashboard titles, environment names or label values leave the source file.

Usage: scripts/dashboard_coverage.py <path/to/stag-dashboards.csv> > results/dashboard-coverage.md
"""
import collections
import csv
import io
import re
import sys

# PromQL construct -> (TQS 3 equivalent, verifying case IDs in results/tqs-coverage.md)
SUPPORTED = {
    "rate": ("fn.op rate", "M01 M02 R04 R07 R10"),
    "increase": ("fn.op increase", "M03 R01 R02 R09"),
    "irate": ("fn.op irate", "M04"),
    "delta": ("fn.op delta", "M18"),
    "resets": ("fn.op resets", "M24"),
    "changes": ("fn.op changes", "M25"),
    "sum": ("agg.op sum / then", "M02 R05"),
    "avg": ("agg.op avg", "M29"),
    "min": ("agg.op min", "R06"),
    "max": ("agg.op max", "M11 R05"),
    "count": ("agg.op count / then count", "M23 R09 R11"),
    "topk": ("top (per_step) / top.query", "M19 R07"),
    "bottomk": ("top with order asc", "—"),
    "histogram_quantile": ("fn.op histogram_quantile", "M06 M07 M08 R03 R07"),
    "max_over_time": ("fn.op max_over_time", "M12 R05"),
    "min_over_time": ("fn.op min_over_time", "M14"),
    "avg_over_time": ("fn.op avg_over_time", "M13"),
    "sum_over_time": ("fn.op sum_over_time", "M15"),
    "count_over_time": ("fn.op count_over_time", "M16"),
    "last_over_time": ("fn.op last_over_time", "M17"),
    "absent": ("agg.op absent", "R04z"),
    "vector": ("fill: zero (only `or vector(0)`)", "R01 R02 R09"),
    "clamp_min": ("formula greatest()", "R02"),
    "clamp_max": ("formula least()", "R02"),
    "clamp": ("formula clamp()", "M32"),
    "abs": ("formula abs()", "—"),
    "round": ("formula round()", "—"),
    "time": ("formula `time`", "R06"),
    "label_replace": ("derive (regex) / output name (constant label)", "R10"),
    "by": ("agg.by", "M02"),
    "without": ("agg.without", "M22"),
    "on": ("top.query (only `and on(...) topk`)", "R07"),
}
OPERATORS = {"or": ("formula join outer + coalesce", "R05"), "and": ("top.query (only with topk)", "R07")}
SUBQUERY = re.compile(r"\[[^\]]*:[^\]]*\]")
# An aggregator may carry a grouping clause before its argument: `sum by (le) (...)`.
FUNC = re.compile(r"\b([a-z_]+)\s*(?:(?:by|without)\s*\([^)]*\)\s*)?\(")
OP = re.compile(r"\)\s*(or|and|unless)\b|\b(or|and|unless)\s*(?:vector|sum|max|min|count|avg|label_replace|histogram_quantile|\()")


def main(path: str) -> None:
    lines = [l for l in open(path, encoding="utf-8") if not l.startswith("#")]
    rows = [r for r in csv.DictReader(io.StringIO("".join(lines))) if r["type"] != "dashboard" and r["queries"].strip()]
    construct_panels = collections.Counter()
    unsupported = collections.Counter()
    ok = 0
    for r in rows:
        q = r["queries"]
        used = set(FUNC.findall(q))
        used |= {m for m in re.findall(r"\b(by|without|on)\b", q)}
        used |= {a or b for a, b in OP.findall(q)}
        for u in used:
            construct_panels[u] += 1
        reasons = set()
        if SUBQUERY.search(q):
            reasons.add("subquery `expr[range:step]`")
        for u in used:
            if u not in SUPPORTED and u not in OPERATORS:
                reasons.add(f"`{u}`")
        if reasons:
            for x in reasons:
                unsupported[x] += 1
        else:
            ok += 1
    total = len(rows)
    print("# Real dashboard coverage (construct level)\n")
    print(f"Source: the Margince Grafana board as recorded by margince-qc ({total} panels with PromQL). Aggregate counts only.\n")
    print(f"**{ok}/{total} panels ({ok / total:.0%}) use only constructs TQS 3 can express.** "
          "Construct level means each construct has a TQS equivalent; it does not by itself prove the panel's numbers. "
          "The numbers are proven for the representative panels R01–R11 in `results/tqs-coverage.md`.\n")
    print("| Not expressible | Panels |\n| :--- | ---: |")
    for k, v in unsupported.most_common():
        print(f"| {k} | {v} |")
    print("\n| PromQL construct | Panels using it | TQS 3 | Verified by |\n| :--- | ---: | :--- | :--- |")
    for k, v in construct_panels.most_common():
        tqs, cases = SUPPORTED.get(k) or OPERATORS.get(k) or ("**none**", "—")
        print(f"| `{k}` | {v} | {tqs} | {cases} |")


if __name__ == "__main__":
    main(sys.argv[1])
