"""Offline analysis of randomized three-arm acceptance samples."""
import argparse
import json
import random
import statistics
from collections import Counter
from pathlib import Path


def distribution(values):
    values = sorted(values)
    if not values:
        return None
    return {"n": len(values), "median_ms": statistics.median(values),
            "mean_ms": statistics.mean(values), "p95_ms": values[max(0, (len(values)*95+99)//100-1)],
            "min_ms": values[0], "max_ms": values[-1]}


def paired(values, seed=20261027):
    rng = random.Random(seed)
    means = sorted(statistics.mean(rng.choices(values, k=len(values))) for _ in range(30_000))
    return {"saving": distribution(values), "faster": sum(v > 0 for v in values),
            "slower": sum(v < 0 for v in values), "ties": sum(v == 0 for v in values),
            "mean_saving_bootstrap_95pct_ms": [means[750], means[29249]]}


def first_data(row):
    return (row["earliest_arrival_us"] if row["mode"] == "broadcast_multi" and not row["metadata_conflict"]
            else row["primary_arrival_us"])


def analyze(path):
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    modes = ("single", "broadcast_single", "broadcast_multi")
    groups = {mode: [r for r in rows if r["mode"] == mode] for mode in modes}
    result = {"groups": {mode: {
        "first_acceptance_data": distribution([first_data(r)/1000 for r in rs]),
        "validated_acceptance": distribution([r["effective_validated_us"]/1000 for r in rs]),
        "rpc_acknowledgement": distribution([r["acknowledgement_us"]/1000 for r in rs if r["acknowledgement_us"] is not None]),
        "actual_nodes": sorted({r["nodes"] for r in rs}),
        "metadata_conflicts": sum(r["metadata_conflict"] for r in rs),
        "observer_errors": sum(len(r["observer_errors"]) for r in rs),
    } for mode, rs in groups.items()}}
    comparisons = {}
    for left, right in (("single", "broadcast_single"), ("broadcast_single", "broadcast_multi"), ("single", "broadcast_multi")):
        pairs = []
        for a in groups[left]:
            b = next((b for b in groups[right] if b["block"] == a["block"]), None)
            if b:
                pairs.append({"block": a["block"], "saving_ms": (a["effective_validated_us"]-b["effective_validated_us"])/1000,
                              "left_ms": a["effective_validated_us"]/1000, "right_ms": b["effective_validated_us"]/1000})
        comparisons[f"{left}_minus_{right}"] = {
            **(paired([p["saving_ms"] for p in pairs]) if pairs else {}),
            "pairs_by_saving": sorted(pairs, key=lambda p: p["saving_ms"]),
        }
    full = groups["broadcast_multi"]
    result["comparisons"] = comparisons
    result["same_transaction_multi_observation_saving"] = paired([
        (r["primary_validated_us"]-r["effective_validated_us"])/1000 for r in full]) if full else None
    result["fastest_validated_endpoint_counts"] = dict(Counter(
        min(r["receipts"], key=lambda receipt: receipt["validated_us"])["endpoint"] for r in full))
    result["measured_fees_sompi"] = sum(r["fee_sompi"] for r in rows)
    result["acknowledgement_errors"] = sum(r["acknowledgement_error"] is not None for r in rows)
    result["limits"] = "One testnet window and fixed primary. Command-mass self-transfers, not full game replay/rendering. Paired bootstrap assumes independent randomized blocks."
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("samples", type=Path)
    args = parser.parse_args()
    print(json.dumps(analyze(args.samples), indent=2))
