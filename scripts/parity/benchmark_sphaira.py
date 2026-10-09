#!/usr/bin/env python3
"""Benchmark identical released-client requests against two prepared servers.

Both servers must contain the same synthetic library and metadata, and run on
the same host. Run once before changes and again after. Artwork's first request
is reported separately; it is cold only if the operator cleared that cache.
Never include passwords in committed command lines or reports.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path
import platform
import statistics
import subprocess
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen


def request(url, body, authorization):
    headers = {"Content-Type": "application/json"}
    if authorization:
        headers["Authorization"] = authorization
    payload = json.dumps(body).encode() if body is not None else None
    started = time.perf_counter()
    with urlopen(Request(url, payload, headers), timeout=120) as response:
        data = response.read()
    return (time.perf_counter() - started) * 1000, data


def rss(pid):
    if not pid:
        return None
    value = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True).strip()
    return int(value) * 1024 if value else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", action="append", required=True,
                        help="label=http://host:port (repeat for both implementations)")
    parser.add_argument("--pid", action="append", default=[], help="label=pid, for local RSS samples")
    parser.add_argument("--authorization-file", type=Path, help="file containing Authorization header")
    parser.add_argument("--fixture", type=Path, default=Path(__file__).resolve().parents[2] /
                        "ownfoil-rs/tests/fixtures/sphaira_native.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--expected-base-count", type=int, required=True,
                        help="assert identical base-library size before timing")
    parser.add_argument("--artwork-path", action="append", default=[],
                        help="local HTTP artwork path, first and repeat requests measured separately")
    args = parser.parse_args()
    if args.samples < 2:
        parser.error("--samples must be at least 2")
    servers = [item.split("=", 1) for item in args.server]
    pids = {label: int(pid) for label, pid in (item.split("=", 1) for item in args.pid)}
    authorization = args.authorization_file.read_text().strip() if args.authorization_file else None
    fixture = json.loads(args.fixture.read_text())
    # Base listing/search/sorts exercise pagination without unstable download tokens.
    cases = [case for case in fixture["cases"] if case["name"].startswith(("all_", "search_"))]
    report = {"platform": platform.platform(), "samples": args.samples,
              "fixture_sha256": hashlib.sha256(args.fixture.read_bytes()).hexdigest(),
              "expected_base_count": args.expected_base_count,
              "query_counts": None,
              "notes": ["RSS samples are observed values, not a true process high-water mark.",
                        "SQL query counts require server tracing and are not observable through HTTP.",
                        "First artwork request is cold only after operator cache reset."],
              "servers": {}}
    digests = {}
    for label, endpoint in servers:
        entry = {"queries": {}, "artwork": [], "max_sampled_rss_bytes": rss(pids.get(label))}
        for case in cases:
            variables = dict(case["variables"], page=1, pageSize=40)
            body = {"query": case["query"], "variables": variables}
            first, raw = request(endpoint.rstrip("/") + "/api/graphql", body, authorization)
            data = json.loads(raw)
            if data.get("errors"):
                raise RuntimeError(data["errors"])
            if case["name"].startswith("all_"):
                assert data["data"]["apps"]["total"] == args.expected_base_count, data
            digest = hashlib.sha256(json.dumps(data, sort_keys=True).encode()).hexdigest()
            key = case["name"]
            if key in digests and digests[key] != digest:
                raise RuntimeError(f"Servers disagree on {key}; timings cannot establish parity")
            digests[key] = digest
            samples = []
            for _ in range(args.samples):
                elapsed, repeat = request(endpoint.rstrip("/") + "/api/graphql", body, authorization)
                if json.loads(repeat) != data:
                    raise RuntimeError(f"Response changed during benchmark: {key}")
                samples.append(elapsed)
                observed = rss(pids.get(label))
                if observed is not None:
                    entry["max_sampled_rss_bytes"] = max(entry["max_sampled_rss_bytes"] or 0, observed)
            entry["queries"][key] = {"first_ms": first, "warm_p50_ms": statistics.median(samples),
                                      "warm_p95_ms": sorted(samples)[math.ceil(.95 * len(samples)) - 1],
                                      "response_bytes": len(raw), "response_sha256": digest}
        for path in args.artwork_path:
            first, raw = request(endpoint.rstrip("/") + path, None, authorization)
            warm, repeat = request(endpoint.rstrip("/") + path, None, authorization)
            assert repeat == raw, "Artwork bytes changed between first and repeat request"
            entry["artwork"].append({"path": path, "first_ms": first, "repeat_ms": warm,
                                     "bytes": len(raw)})
        report["servers"][label] = entry
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Saved controlled HTTP benchmark to {args.output}")


if __name__ == "__main__":
    try:
        main()
    except HTTPError as error:
        raise SystemExit(f"HTTP {error.code}: {error.reason}") from error
