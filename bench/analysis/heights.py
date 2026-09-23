#!/usr/bin/env python3
"""heights.py — offline analysis of captured Arc CL logs, per height.

    heights.py proposers <run-dir> [--json]    who proposed each decided height, and how fast
    heights.py phases <cl.log> [<cl.log> ...]  where the time inside a height goes
    heights.py --selftest

Both read the files `bench/fleet/fleet logs` (or `down`) writes BEFORE any
`compose down` — which throws a container's log buffer away — laid out as
<run-dir>/logs/validatorN/validatorN_cl.log.

proposers
---------
Parses the CL's stock log lines (plaintext tracing, `<rfc3339>Z  LEVEL fields msg`):

  INFO  🔮 Started round height=<h> round=<r> role=<role> proposer=0x<addr>
        every validator, every round it starts; the ONLY line carrying a proposer
  INFO  🎉 Consensus has decided on value height=<h> round=<r> value_id=<id> signatures=<n>
        the decided ROUND (can be > 0 after a timeout), not the proposer
  INFO  decided{height=<h> round=<r>}: 🟢 Successfully committed the decided value
  WARN  missed_proposer=0x<addr> height=<h> missed_round=<r> Consensus round missed
        (UNCONFIRMED: inferred from the source call, never seen in a captured log)
  ERROR Manual intervention required! ...   — a PARKED CL

No single line carries both a decided height and its proposer, so attribution
is a join: decided (height, round) -> "Started round" at that same (height,
round), taken from any captured log (round-robin selection is deterministic),
"UNKNOWN" only if none started it. A validator that decides but never proposes
(a pure sync-follower) or a parked one shows up here and nowhere else.

phases
------
Parses the `height_timing` line one per height, emitted only by the arc branch
`lean-lane-instrumented` with ARC_HEIGHT_TIMING=1 (`fleet up` injects it into
every CL when that variable is set). Stamps are ms after the height's start
(`t_start` = ms from the previous decide to this start; `-` = did not happen
on this node); `dec_*` fields are elapsed DURATIONS of the decide path's parts.
Reports mean/p50/p90 per phase, split by role and by proposer, plus cadence:

    height_timing h=42 role=validator proposer=0x1234abcd round=0 t_start=12
      first_part=35 last_part=180 parts=14 bytes=1234567 assembled=181
      lean_stage=262 evm_newpayload=250 prevote=263 precommit=310 decided=402
      anchor=418 evm_fcu=455 lean_txs=2400 build_lean=- build_evm=- parts_sent=-
      dec_monitor=0.8 dec_lookup=2.3 dec_bind=0.0 dec_anchor_call=11.9 ...
"""

from __future__ import annotations

import argparse
import datetime
import glob
import io
import json
import math
import os
import re
import statistics
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Iterable, Iterator, Optional, Sequence

# ============================================================== phases
LINE_RE = re.compile(r"height_timing\s+h=\d+.*$")
KV_RE = re.compile(r"([a-z_]+)=(\S+)")

# Timestamp fields, in the order the CL writes them.
STAMP_FIELDS = (
    "t_start",
    "first_part",
    "last_part",
    "assembled",
    "lean_stage",
    "evm_newpayload",
    "prevote",
    "precommit",
    "decided",
    "anchor",
    "evm_fcu",
    "build_lean",
    "build_evm",
    "parts_sent",
)
COUNT_FIELDS = ("round", "parts", "bytes", "lean_txs")

# Decide-path segment DURATIONS (ms, one decimal) — unlike every other field
# these are elapsed times, not stamps relative to the height start. They
# decompose `decided -> anchor -> evm_fcu` and the gap to the next height.
SEGMENT_FIELDS = (
    "dec_monitor",
    "dec_lookup",
    "dec_bind",
    "dec_anchor_call",
    "dec_clone",
    "dec_store",
    "dec_clean",
    "dec_prune",
    "dec_next",
)

# name -> (from_field, to_field). `None` as the source means "the height start",
# which is the origin every stamp is already relative to.
PHASES: tuple[tuple[str, Optional[str], str], ...] = (
    ("start_to_first_part", None, "first_part"),
    ("first_to_last_part", "first_part", "last_part"),
    ("last_part_to_assembled", "last_part", "assembled"),
    ("assembled_to_lean_stage", "assembled", "lean_stage"),
    ("assembled_to_newpayload", "assembled", "evm_newpayload"),
    ("validate_to_prevote", "lean_stage", "prevote"),
    ("prevote_to_precommit", "prevote", "precommit"),
    ("precommit_to_decided", "precommit", "decided"),
    ("decided_to_anchor", "decided", "anchor"),
    ("anchor_to_fcu", "anchor", "evm_fcu"),
    # Proposer-side build.
    ("start_to_build_lean", None, "build_lean"),
    ("build_lean_to_build_evm", "build_lean", "build_evm"),
    ("build_evm_to_parts_sent", "build_evm", "parts_sent"),
    # Whole-height spans.
    ("start_to_decided", None, "decided"),
    ("gap_decide_to_next_start", None, "t_start"),
)


@dataclass
class Height:
    """One parsed `height_timing` line."""

    h: int
    role: str
    proposer: str
    stamps: dict[str, int] = field(default_factory=dict)
    counts: dict[str, int] = field(default_factory=dict)
    segments: dict[str, float] = field(default_factory=dict)
    source: str = ""

    def duration(self, frm: Optional[str], to: str) -> Optional[int]:
        """Milliseconds between two stamps, or None if either is missing."""
        end = self.stamps.get(to)
        if end is None:
            return None
        if frm is None:
            return end
        start = self.stamps.get(frm)
        if start is None:
            return None
        return end - start


def parse_line(line: str, source: str = "") -> Optional[Height]:
    """Parse one log line, or return None if it is not a height_timing line."""
    match = LINE_RE.search(line)
    if not match:
        return None
    kvs = dict(KV_RE.findall(match.group(0)))
    raw_h = kvs.get("h")
    if raw_h is None or not raw_h.isdigit():
        return None

    rec = Height(
        h=int(raw_h),
        role=kvs.get("role", "unknown"),
        proposer=kvs.get("proposer", "unknown"),
        source=source,
    )
    for key in STAMP_FIELDS:
        raw = kvs.get(key)
        if raw is None or raw == "-":
            continue
        try:
            rec.stamps[key] = int(raw)
        except ValueError:
            continue
    for key in COUNT_FIELDS:
        raw = kvs.get(key)
        if raw is None or raw == "-":
            continue
        try:
            rec.counts[key] = int(raw)
        except ValueError:
            continue
    for key in SEGMENT_FIELDS:
        raw = kvs.get(key)
        if raw is None or raw == "-":
            continue
        try:
            rec.segments[key] = float(raw)
        except ValueError:
            continue
    return rec


def parse_lines(lines: Iterable[str], source: str = "") -> Iterator[Height]:
    for line in lines:
        rec = parse_line(line, source)
        if rec is not None:
            yield rec


def percentile(values: Sequence[float], q: float) -> float:
    """Nearest-rank percentile of a non-empty sample; q in [0, 1].

    Nearest rank (ceil), not interpolation: with a handful of heights an
    interpolated p90 invents a value no height actually took.
    """
    ordered = sorted(values)
    if not ordered:
        raise ValueError("percentile of an empty sample")
    rank = max(1, min(len(ordered), math.ceil(q * len(ordered))))
    return ordered[rank - 1]


def summarize(values: Sequence[float]) -> Optional[dict[str, float]]:
    if not values:
        return None
    return {
        "n": len(values),
        "mean": sum(values) / len(values),
        "p50": percentile(values, 0.50),
        "p90": percentile(values, 0.90),
    }


def phase_table(records: Sequence[Height]) -> list[tuple[str, dict[str, float]]]:
    """mean/p50/p90 for every phase that at least one record observed."""
    out = []
    for name, frm, to in PHASES:
        samples = []
        for rec in records:
            value = rec.duration(frm, to)
            if value is not None:
                samples.append(float(value))
        stats = summarize(samples)
        if stats is not None:
            out.append((name, stats))
    return out


def render_phases(title: str, records: Sequence[Height], out) -> None:
    rows = phase_table(records)
    print(f"\n{title}  (heights={len(records)})", file=out)
    if not rows:
        print("  no phase observed", file=out)
        return
    print(f"  {'phase':<28} {'n':>5} {'mean':>9} {'p50':>9} {'p90':>9}", file=out)
    for name, s in rows:
        print(
            f"  {name:<28} {int(s['n']):>5} {s['mean']:>9.1f} "
            f"{s['p50']:>9.1f} {s['p90']:>9.1f}",
            file=out,
        )


def render_segments(title: str, records: Sequence[Height], out) -> None:
    """The `dec_*` decide-path breakdown, for records that carry it.

    Prints nothing when no height has any segment — a log from a CL without
    the instrumentation reads exactly as it did before.
    """
    rows = []
    for key in SEGMENT_FIELDS:
        samples = [r.segments[key] for r in records if key in r.segments]
        stats = summarize(samples)
        if stats is not None:
            rows.append((key, stats))
    if not rows:
        return
    print(f"\n{title}  (heights={len(records)})", file=out)
    print(f"  {'segment':<28} {'n':>5} {'mean':>9} {'p50':>9} {'p90':>9}", file=out)
    for name, s in rows:
        print(
            f"  {name:<28} {int(s['n']):>5} {s['mean']:>9.2f} "
            f"{s['p50']:>9.2f} {s['p90']:>9.2f}",
            file=out,
        )
    # The budget these fields exist to explain: everything the CL does between
    # the certificate arriving and the anchor returning. The residual against
    # `decided_to_anchor` is what is still unattributed.
    pre = ("dec_monitor", "dec_lookup", "dec_bind", "dec_anchor_call")
    accounted = [
        sum(r.segments[k] for k in pre if k in r.segments)
        for r in records
        if any(k in r.segments for k in pre)
    ]
    measured = [
        float(v) for v in (r.duration("decided", "anchor") for r in records) if v is not None
    ]
    a, m = summarize(accounted), summarize(measured)
    if a and m:
        print(
            f"  {'-> sum(pre-anchor dec_*)':<28} {int(a['n']):>5} {a['mean']:>9.2f} "
            f"{a['p50']:>9.2f} {a['p90']:>9.2f}",
            file=out,
        )
        print(
            f"  {'-> decided_to_anchor':<28} {int(m['n']):>5} {m['mean']:>9.2f} "
            f"{m['p50']:>9.2f} {m['p90']:>9.2f}",
            file=out,
        )


def render_sizes(records: Sequence[Height], out) -> None:
    for key in ("bytes", "parts", "lean_txs"):
        samples = [float(r.counts[key]) for r in records if key in r.counts]
        stats = summarize(samples)
        if stats is None:
            continue
        print(
            f"  {key:<10} n={int(stats['n']):<6} mean={stats['mean']:>12.1f} "
            f"p50={stats['p50']:>12.1f} p90={stats['p90']:>12.1f}",
            file=out,
        )


def cadence(records: Sequence[Height], out) -> None:
    """Heights per minute, from the height-to-height wall clock.

    Each height's wall time is its own `start_to_decided` plus the gap from the
    previous decide to its start (`t_start`), which together tile the timeline
    with no overlap. Heights missing either field are skipped.
    """
    spans = []
    for rec in records:
        decided = rec.stamps.get("decided")
        gap = rec.stamps.get("t_start")
        if decided is None or gap is None:
            continue
        spans.append(decided + gap)
    if not spans:
        print("\ncadence: not enough complete heights", file=out)
        return
    total_ms = sum(spans)
    mean_ms = total_ms / len(spans)
    print(f"\ncadence  (heights={len(spans)}, wall={total_ms / 1000.0:.1f}s)", file=out)
    print(
        f"  mean height {mean_ms:.1f} ms  ->  "
        f"{60000.0 / mean_ms:.2f} heights/min  ({1000.0 / mean_ms:.2f} blk/s)",
        file=out,
    )
    print(
        f"  p50 height {percentile(spans, 0.50):.1f} ms   "
        f"p90 height {percentile(spans, 0.90):.1f} ms",
        file=out,
    )


def report(records: Sequence[Height], out=sys.stdout) -> None:
    print(f"heights phases: {len(records)} heights parsed", file=out)
    if not records:
        return
    heights = [r.h for r in records]
    print(f"  height range {min(heights)}..{max(heights)}", file=out)
    print("  value size:", file=out)
    render_sizes(records, out)

    cadence(records, out)

    render_phases("ALL", records, out)
    render_segments("ALL decide-path segments", records, out)

    by_role: dict[str, list[Height]] = defaultdict(list)
    for rec in records:
        by_role[rec.role].append(rec)
    for role in sorted(by_role):
        render_phases(f"role={role}", by_role[role], out)
        render_segments(f"role={role} decide-path segments", by_role[role], out)

    by_proposer: dict[str, list[Height]] = defaultdict(list)
    for rec in records:
        by_proposer[rec.proposer].append(rec)
    for proposer in sorted(by_proposer):
        render_phases(f"proposer={proposer}", by_proposer[proposer], out)


def read_files(paths: Sequence[str]) -> list[Height]:
    records: list[Height] = []
    for path in paths:
        with open(path, "r", errors="replace") as handle:
            records.extend(parse_lines(handle, source=path))
    records.sort(key=lambda r: (r.source, r.h))
    return records

# ============================================================== proposers
STARTED_ROUND_RE = re.compile(
    r'^(?P<ts>\S+)\s+INFO\s+.*Started round height=(?P<height>\d+) round=(?P<round>\d+) '
    r'role=(?P<role>\S+) proposer=(?P<proposer>0x\S+)'
)
DECIDED_RE = re.compile(
    r'^(?P<ts>\S+)\s+INFO\s+.*Consensus has decided on value height=(?P<height>\d+) '
    r'round=(?P<round>\d+) value_id=(?P<value_id>\S+) signatures=(?P<sigs>\d+)'
)
COMMITTED_RE = re.compile(
    r'^(?P<ts>\S+)\s+INFO\s+decided\{height=(?P<height>\d+) round=(?P<round>\d+)\}:'
)
MISSED_ROUND_RE = re.compile(
    r'^(?P<ts>\S+)\s+WARN\s+missed_proposer=(?P<proposer>0x\S+) height=(?P<height>\d+) '
    r'missed_round=(?P<round>\d+) Consensus round missed'
)
MANUAL_RE = re.compile(r'^(?P<ts>\S+)\s+ERROR\s+Manual intervention required!')


def parse_ts(raw):
    """'2026-06-27T03:15:28.634131Z' -> aware UTC datetime."""
    raw = raw.rstrip('Z')
    if '.' not in raw:
        raw += '.0'
    return datetime.datetime.strptime(raw, '%Y-%m-%dT%H:%M:%S.%f').replace(
        tzinfo=datetime.timezone.utc
    )


def parse_log(path):
    """Parse one validatorN_cl.log into its raw event streams."""
    started = {}  # (height, round) -> proposer (first one wins)
    decided = []  # [(height, round, ts, sigs)] in file order
    committed = {}  # (height, round) -> ts
    missed = []  # [(height, round, proposer, ts)]
    manual = []  # [ts]
    with open(path, 'r', encoding='utf-8', errors='replace') as fh:
        for line in fh:
            m = STARTED_ROUND_RE.match(line)
            if m:
                key = (int(m['height']), int(m['round']))
                started.setdefault(key, m['proposer'])
                continue
            m = DECIDED_RE.match(line)
            if m:
                decided.append(
                    (int(m['height']), int(m['round']), parse_ts(m['ts']), int(m['sigs']))
                )
                continue
            m = COMMITTED_RE.match(line)
            if m:
                committed[(int(m['height']), int(m['round']))] = parse_ts(m['ts'])
                continue
            m = MISSED_ROUND_RE.match(line)
            if m:
                missed.append(
                    (int(m['height']), int(m['round']), m['proposer'], parse_ts(m['ts']))
                )
                continue
            m = MANUAL_RE.match(line)
            if m:
                manual.append(parse_ts(m['ts']))
    return {
        'started': started,
        'decided': decided,
        'committed': committed,
        'missed': missed,
        'manual': manual,
    }


def find_cl_logs(run_dir):
    found, missing = {}, []
    dirs = glob.glob(os.path.join(run_dir, 'logs', 'validator*'))
    idx = sorted(int(m.group(1)) for d in dirs if (m := re.search(r'validator(\d+)$', d)))
    for n in idx or range(1, 5):
        p = os.path.join(run_dir, 'logs', f'validator{n}', f'validator{n}_cl.log')
        if os.path.isfile(p):
            found[n] = p
        else:
            missing.append(n)
    return found, missing


def build_canonical(all_parsed):
    """Merge per-validator streams into one canonical decided-height timeline.

    canonical[height] = {'round', 'ts' (earliest seen), 'sigs', 'seen_by'}
    merged_started[(height, round)] = proposer, unioned across all logs.
    Returns (merged_started, canonical, started_conflicts, round_conflicts).
    """
    merged_started = {}
    started_conflicts = []
    for n, p in all_parsed.items():
        for key, proposer in p['started'].items():
            if key in merged_started and merged_started[key] != proposer:
                started_conflicts.append((key, merged_started[key], proposer, n))
            else:
                merged_started.setdefault(key, proposer)

    by_height = defaultdict(list)
    for n, p in all_parsed.items():
        for (h, r, ts, sigs) in p['decided']:
            by_height[h].append((r, ts, n, sigs))

    canonical = {}
    round_conflicts = []
    for h, entries in by_height.items():
        entries.sort(key=lambda e: e[1])  # earliest timestamp first
        r0, ts0, n0, sigs0 = entries[0]
        rounds_seen = {r for (r, _ts, _n, _sigs) in entries}
        if len(rounds_seen) > 1:
            round_conflicts.append((h, sorted(rounds_seen)))
        canonical[h] = {'round': r0, 'ts': ts0, 'sigs': sigs0, 'seen_by': len(entries)}
    return merged_started, canonical, started_conflicts, round_conflicts


def attribute(canonical, merged_started):
    rows = []
    for h in sorted(canonical):
        r = canonical[h]['round']
        proposer = merged_started.get((h, r), 'UNKNOWN')
        rows.append(
            {
                'height': h,
                'round': r,
                'ts': canonical[h]['ts'],
                'sigs': canonical[h]['sigs'],
                'seen_by': canonical[h]['seen_by'],
                'proposer': proposer,
            }
        )
    for i, row in enumerate(rows):
        if i == 0:
            row['interval_ms'] = None
            continue
        prev = rows[i - 1]
        if row['height'] == prev['height'] + 1:
            row['interval_ms'] = (row['ts'] - prev['ts']).total_seconds() * 1000.0
        else:
            row['interval_ms'] = None  # gap: heights missing from every captured log
    return rows


def pctl(vals, p):
    return percentile(vals, p) if vals else None


def per_proposer_stats(rows):
    groups = defaultdict(list)
    for r in rows:
        groups[r['proposer']].append(r)
    out = {}
    for proposer, rs in groups.items():
        intervals = [r['interval_ms'] for r in rs if r['interval_ms'] is not None]
        out[proposer] = {
            'heights_decided': len(rs),
            'mean_interval_ms': statistics.mean(intervals) if intervals else None,
            'p50_interval_ms': pctl(intervals, 0.5),
            'p90_interval_ms': pctl(intervals, 0.9),
            'rounds_gt0': sum(1 for r in rs if r['round'] > 0),
            'first_height': rs[0]['height'],
            'last_height': rs[-1]['height'],
        }
    return out


def per_minute_series(rows):
    """Per-minute (bucketed from the first decided ts) heights decided, split
    by proposer, plus the whole-run per-minute mean height-to-height interval
    — this is the series that lets a cadence drift (e.g. 114 -> 85 blk/min
    over 10 minutes) be pinned to a specific proposer or to none."""
    if not rows:
        return [], [], {}
    t0 = rows[0]['ts']
    by_minute_proposer = defaultdict(lambda: defaultdict(int))
    by_minute_intervals = defaultdict(list)
    for r in rows:
        minute = int((r['ts'] - t0).total_seconds() // 60)
        by_minute_proposer[minute][r['proposer']] += 1
        if r['interval_ms'] is not None:
            by_minute_intervals[minute].append(r['interval_ms'])
    minutes = sorted(set(by_minute_proposer) | set(by_minute_intervals))
    overall = []
    for minute in minutes:
        ivs = by_minute_intervals.get(minute, [])
        overall.append(
            {
                'minute': minute,
                'heights_decided': sum(by_minute_proposer[minute].values()),
                'mean_interval_ms': statistics.mean(ivs) if ivs else None,
            }
        )
    return minutes, overall, by_minute_proposer


def fmt_ms(v):
    return '-' if v is None else f'{v:.0f}'


def render(run_dir, found, missing, all_parsed, merged_started, canonical,
           started_conflicts, round_conflicts, rows):
    print(f'heights proposers: {run_dir}')
    print(f'  CL logs found: {sorted(found)}  missing: {missing or "none"}')
    for n, p in all_parsed.items():
        manual = p['manual']
        if manual:
            print(f'  validator{n}: {len(manual)} "Manual intervention" (parked) — first at {manual[0].isoformat()}')
    if started_conflicts:
        print(f'  WARNING: {len(started_conflicts)} (height,round) had disagreeing proposers across logs (byzantine or a bug):')
        for (key, p1, p2, n) in started_conflicts[:10]:
            print(f'    {key}: {p1} vs {p2} (from validator{n})')
    if round_conflicts:
        print(f'  WARNING: {len(round_conflicts)} heights decided at different rounds across logs (should not happen):')
        for (h, rs) in round_conflicts[:10]:
            print(f'    height {h}: rounds {rs}')
    any_missed = [(n, m) for n, p in all_parsed.items() for m in p['missed']]
    if any_missed:
        print(f'  {len(any_missed)} "Consensus round missed" events observed (UNCONFIRMED pattern — see the module docstring)')

    if not rows:
        print('  no decided heights parsed — nothing to attribute')
        return

    print()
    print(f'  {len(rows)} heights decided, {rows[0]["height"]}..{rows[-1]["height"]}, '
          f'{rows[0]["ts"].isoformat()} .. {rows[-1]["ts"].isoformat()}')
    unknown = [r for r in rows if r['proposer'] == 'UNKNOWN']
    if unknown:
        print(f'  {len(unknown)} heights with NO "Started round" in any captured log '
              f'(proposer unknown): {[r["height"] for r in unknown][:10]}{" ..." if len(unknown) > 10 else ""}')

    stats = per_proposer_stats(rows)
    print()
    print('  per-proposer attribution')
    print(f'  {"proposer":44s} {"heights":>8s} {"mean_ms":>8s} {"p50_ms":>8s} {"p90_ms":>8s} {"rounds>0":>9s} {"range":>15s}')
    for proposer, s in sorted(stats.items(), key=lambda kv: -kv[1]['heights_decided']):
        rng = f'{s["first_height"]}-{s["last_height"]}'
        print(f'  {proposer:44s} {s["heights_decided"]:8d} '
              f'{fmt_ms(s["mean_interval_ms"]):>8s} {fmt_ms(s["p50_interval_ms"]):>8s} '
              f'{fmt_ms(s["p90_interval_ms"]):>8s} {s["rounds_gt0"]:9d} {rng:>15s}')

    minutes, overall, by_minute_proposer = per_minute_series(rows)
    proposers = sorted(stats.keys())
    print()
    print('  per-minute cadence per proposer (heights decided in that minute)')
    header = '  minute ' + ''.join(f'{p[-8:]:>10s}' for p in proposers) + f'{"total":>10s}{"mean_iv_ms":>12s}'
    print(header)
    for minute, o in zip(minutes, overall):
        row = f'  {minute:6d} '
        for p in proposers:
            row += f'{by_minute_proposer[minute].get(p, 0):10d}'
        row += f'{o["heights_decided"]:10d}{fmt_ms(o["mean_interval_ms"]):>12s}'
        print(row)

    print()
    print('  whole-run per-minute mean height interval (cadence drift)')
    print(f'  {"minute":>6s} {"blk/min":>8s} {"mean_interval_ms":>17s}')
    for minute, o in zip(minutes, overall):
        print(f'  {minute:6d} {o["heights_decided"]:8d} {fmt_ms(o["mean_interval_ms"]):>17s}')


def proposers_main(run_dir, as_json=False):
    found, missing = find_cl_logs(run_dir)
    if not found:
        print(f'heights proposers: FAIL: no validatorN_cl.log under {run_dir}/logs/validatorN/ '
              f'(run `bench/fleet/fleet logs` or `down` first)', file=sys.stderr)
        return 1
    all_parsed = {n: parse_log(p) for n, p in found.items()}
    merged_started, canonical, started_conflicts, round_conflicts = build_canonical(all_parsed)
    rows = attribute(canonical, merged_started)

    if as_json:
        out = {
            'run_dir': run_dir,
            'found': sorted(found),
            'missing': missing,
            'started_conflicts': len(started_conflicts),
            'round_conflicts': len(round_conflicts),
            'heights': [
                {**{k: v for k, v in r.items() if k != 'ts'}, 'ts': r['ts'].isoformat()}
                for r in rows
            ],
            'per_proposer': per_proposer_stats(rows),
        }
        print(json.dumps(out, indent=2))
        return 0

    render(run_dir, found, missing, all_parsed, merged_started, canonical,
           started_conflicts, round_conflicts, rows)
    return 0

def _parse_text(text):
    """parse_log over an in-memory string (selftest only)."""
    import tempfile
    with tempfile.NamedTemporaryFile('w', suffix='_cl.log', delete=False) as fh:
        fh.write(text)
    try:
        return parse_log(fh.name)
    finally:
        os.unlink(fh.name)


# ============================================================== self-test
# Proposers fixture: real captured CL lines (heights renumbered), plus one
# synthetic "Manual intervention" and one synthetic, UNCONFIRMED "round missed".
PROPOSERS_FIXTURE = """\
2026-06-27T03:15:28.634131Z  INFO 🦋 Started height height=100 network_id=0x9749bc9f
2026-06-27T03:15:28.634133Z  INFO 🔮 Started round height=100 round=0 role=None proposer=0x6791e002c8419e8a2fa5266e2951ed1780bc2295
2026-06-27T03:15:29.103045Z  INFO 🎉 Consensus has decided on value height=100 round=0 value_id=c39ef0482b5250cdbc032a02d67c5cb6637c529401e29063c1e0d18ba81ee7c4 signatures=16
2026-06-27T03:15:29.112019Z  INFO decided{height=100 round=0}: 🟢 Successfully committed the decided value
2026-06-27T03:15:29.117614Z  INFO 🦋 Started height height=101 network_id=0x9749bc9f
2026-06-27T03:15:29.117618Z  INFO 🔮 Started round height=101 round=0 role=None proposer=0x7e4499718f868ff1f89fe5432e11c9f9f3e01fae
2026-06-27T03:15:29.153183Z  INFO 🎉 Consensus has decided on value height=101 round=0 value_id=70fe14dbfe8ba32e41912bae7decc9c5b11f130fb802a09d7908b9a15d1c3811 signatures=17
2026-06-27T03:15:29.157659Z  INFO decided{height=101 round=0}: 🟢 Successfully committed the decided value
2026-06-27T03:15:29.160230Z  INFO 🦋 Started height height=102 network_id=0x9749bc9f
2026-06-27T03:15:29.160234Z  INFO 🔮 Started round height=102 round=0 role=None proposer=0xfdc37d533f82159bf6c46da00db4faa379b941de
2026-06-27T03:16:05.200000Z  WARN missed_proposer=0xfdc37d533f82159bf6c46da00db4faa379b941de height=102 missed_round=0 Consensus round missed
2026-06-27T03:16:05.210230Z  INFO 🔮 Started round height=102 round=1 role=None proposer=0xfeb2b98d614701434ea6a12ccf29e0e40ae56941
2026-06-27T03:16:05.265687Z  INFO 🎉 Consensus has decided on value height=102 round=1 value_id=c530b840f2c56df89fb3648809c4ebfe4f17c9d47119106c22d2b1cd4a983f6a signatures=16
2026-06-27T03:16:05.270708Z  INFO decided{height=102 round=1}: 🟢 Successfully committed the decided value
2026-06-27T01:36:20.483735Z ERROR Manual intervention required! Waiting for termination signal (SIGTERM)...
"""



# Phases fixture: hand-written height_timing lines.
PHASES_FIXTURE = """\
2026-09-14T20:00:00.000Z  INFO arc_node_consensus::app: unrelated line
2026-09-14T20:00:00.100Z  INFO height_timing h=100 role=validator proposer=0xaaaa1111 round=0 t_start=10 first_part=30 last_part=180 parts=14 bytes=1200000 assembled=185 lean_stage=260 evm_newpayload=250 prevote=262 precommit=300 decided=400 anchor=415 evm_fcu=450 lean_txs=2400 build_lean=- build_evm=- parts_sent=- dec_monitor=0.8 dec_lookup=2.3 dec_bind=0.0 dec_anchor_call=11.9 dec_clone=0.4 dec_store=3.1 dec_clean=0.9 dec_prune=0.2 dec_next=1.5
2026-09-14T20:00:00.600Z  INFO height_timing h=101 role=proposer proposer=0xbbbb2222 round=0 t_start=20 first_part=- last_part=- parts=0 bytes=1300000 assembled=- lean_stage=- evm_newpayload=90 prevote=95 precommit=200 decided=300 anchor=320 evm_fcu=350 lean_txs=2500 build_lean=30 build_evm=80 parts_sent=140
2026-09-14T20:00:01.100Z  INFO height_timing h=102 role=validator proposer=0xcccc3333 round=1 t_start=30 first_part=50 last_part=400 parts=20 bytes=1400000 assembled=410 lean_stage=500 evm_newpayload=480 prevote=505 precommit=600 decided=800 anchor=830 evm_fcu=880 lean_txs=2600 build_lean=- build_evm=- parts_sent=- dec_monitor=1.2 dec_lookup=2.7 dec_bind=- dec_anchor_call=25.1 dec_clone=0.6 dec_store=4.0 dec_clean=1.0 dec_prune=0.3 dec_next=2.0
"""



def selftest_proposers():
    failures = []

    def check(name, cond):
        print(f'  [{"PASS" if cond else "FAIL"}] {name}')
        if not cond:
            failures.append(name)

    parsed = _parse_text(PROPOSERS_FIXTURE)

    check('parses 3 decided events', len(parsed['decided']) == 3)
    check('parses 4 started-round events', len(parsed['started']) == 4)
    check('parses 1 manual-intervention', len(parsed['manual']) == 1)
    check('parses 1 missed-round (unconfirmed pattern)', len(parsed['missed']) == 1)
    check(
        'height 100 round 0 proposer',
        parsed['started'].get((100, 0)) == '0x6791e002c8419e8a2fa5266e2951ed1780bc2295',
    )
    check(
        'height 102 round 1 (post-timeout) proposer differs from round 0',
        parsed['started'][(102, 1)] == '0xfeb2b98d614701434ea6a12ccf29e0e40ae56941'
        and parsed['started'][(102, 0)] == '0xfdc37d533f82159bf6c46da00db4faa379b941de',
    )
    check(
        '🎉 decided line for height 102 carries round=1 (the decided round, post-timeout)',
        any(h == 102 and r == 1 for (h, r, _ts, _s) in parsed['decided']),
    )

    all_parsed = {1: parsed}
    merged_started, canonical, started_conflicts, round_conflicts = build_canonical(all_parsed)
    rows = attribute(canonical, merged_started)
    check('canonical has 3 heights', len(canonical) == 3)
    check('no started/round conflicts on a single log', not started_conflicts and not round_conflicts)
    check(
        'height 102 attributed to the round-1 proposer, not the round-0 one',
        next(r for r in rows if r['height'] == 102)['proposer']
        == '0xfeb2b98d614701434ea6a12ccf29e0e40ae56941',
    )
    check('height 100 has no interval (first row)', rows[0]['interval_ms'] is None)
    check(
        'height 101 interval = decided(101) - decided(100) ~= 50 ms',
        rows[1]['interval_ms'] is not None and 30 < rows[1]['interval_ms'] < 60,
    )

    stats = per_proposer_stats(rows)
    check('3 distinct proposers attributed', len(stats) == 3)

    # cross-log fallback: a second log that never started height 100's round 0
    # itself (e.g. parked/sync-following) must still get the proposer from log 1
    parked_fixture = PROPOSERS_FIXTURE.replace(
        '2026-06-27T03:15:28.634133Z  INFO 🔮 Started round height=100 round=0 role=None proposer=0x6791e002c8419e8a2fa5266e2951ed1780bc2295\n',
        '',
    )
    parsed2 = _parse_text(parked_fixture)
    check('validator2 fixture is missing height 100 round 0 locally', (100, 0) not in parsed2['started'])
    merged2, canonical2, _, _ = build_canonical({1: parsed, 2: parsed2})
    rows2 = attribute(canonical2, merged2)
    check(
        'height 100 still attributed via validator1 fallback, not UNKNOWN',
        next(r for r in rows2 if r['height'] == 100)['proposer'] != 'UNKNOWN',
    )

    return failures



def selftest_phases() -> list:
    records = list(parse_lines(PHASES_FIXTURE.splitlines()))
    failures: list[str] = []

    def check(label: str, got, want) -> None:
        if got != want:
            failures.append(f"{label}: got {got!r}, want {want!r}")

    check("parsed count", len(records), 3)
    check("heights", [r.h for r in records], [100, 101, 102])
    check("roles", [r.role for r in records], ["validator", "proposer", "validator"])
    check("proposers", records[1].proposer, "0xbbbb2222")
    check("round parsed", records[2].counts.get("round"), 1)
    check("bytes parsed", records[0].counts.get("bytes"), 1200000)
    check("lean_txs parsed", records[1].counts.get("lean_txs"), 2500)

    # `-` fields must be absent, not zero.
    check("dash is absent", "first_part" in records[1].stamps, False)
    check("dash duration is None", records[1].duration("first_part", "last_part"), None)

    # Durations.
    check("stream span h100", records[0].duration("first_part", "last_part"), 150)
    check("assemble h100", records[0].duration("last_part", "assembled"), 5)
    check("vote gap h100", records[0].duration("prevote", "precommit"), 38)
    check("commit h100", records[0].duration("precommit", "decided"), 100)
    check("anchor h100", records[0].duration("decided", "anchor"), 15)
    check("start-to-first h100", records[0].duration(None, "first_part"), 30)
    check("build h101", records[1].duration("build_lean", "build_evm"), 50)

    # Aggregation: the two validator heights only.
    validators = [r for r in records if r.role == "validator"]
    rows = dict(phase_table(validators))
    check("validator n", rows["first_to_last_part"]["n"], 2)
    check("validator mean", rows["first_to_last_part"]["mean"], 250.0)
    check("validator p50", rows["first_to_last_part"]["p50"], 150.0)
    check("validator p90", rows["first_to_last_part"]["p90"], 350.0)

    # The proposer's build phases must not appear in the validator table.
    check("no build for validators", "build_lean_to_build_evm" in rows, False)

    # Percentiles on a known sample.
    check("p50 of 1..10", percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.5), 5)
    check("p90 of 1..10", percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.9), 9)
    check("p50 single", percentile([7], 0.5), 7)

    # Cadence tiles t_start + decided: 410, 320, 830 ms.
    buf = io.StringIO()
    cadence(records, buf)
    text = buf.getvalue()
    if "heights=3" not in text:
        failures.append(f"cadence did not use all 3 heights: {text!r}")
    if "520.0 ms" not in text:
        failures.append(f"cadence mean should be 520.0 ms: {text!r}")

    # A full report must not raise on the fixture.
    buf = io.StringIO()
    report(records, buf)
    for expected in ("role=validator", "role=proposer", "proposer=0xaaaa1111", "cadence"):
        if expected not in buf.getvalue():
            failures.append(f"report is missing section {expected!r}")

    # Decide-path segments: floats, `-` absent, and a line written by a CL
    # without the instrumentation (h=101) parses exactly as before.
    check("segment parsed", records[0].segments.get("dec_anchor_call"), 11.9)
    check("segment zero kept", records[0].segments.get("dec_bind"), 0.0)
    check("segment dash absent", "dec_bind" in records[2].segments, False)
    check("old-format line has no segments", records[1].segments, {})

    buf = io.StringIO()
    render_segments("seg", records, buf)
    text = buf.getvalue()
    for expected in ("dec_anchor_call", "sum(pre-anchor dec_*)", "decided_to_anchor"):
        if expected not in text:
            failures.append(f"segment table is missing {expected!r}: {text!r}")
    # h=100: 0.8 + 2.3 + 0.0 + 11.9 = 15.0; h=102: 1.2 + 2.7 + 25.1 = 29.0.
    if "15.00" not in text or "29.00" not in text:
        failures.append(f"segment sums wrong: {text!r}")

    # A CL log with no segments at all must print no segment table.
    buf = io.StringIO()
    render_segments("seg", [records[1]], buf)
    if buf.getvalue() != "":
        failures.append(f"segment table should be empty: {buf.getvalue()!r}")

    # Non-matching input yields nothing.
    check("noise ignored", parse_line("2026-01-01 INFO nothing here"), None)

    return failures


def selftest() -> int:
    failures = selftest_proposers() + selftest_phases()
    for f in failures:
        print(f"FAIL {f}", file=sys.stderr)
    print(f"heights.py selftest: {len(failures)} failure(s)" if failures else "heights.py selftest: all checks passed")
    return 1 if failures else 0


def main(argv: Sequence[str]) -> int:
    if list(argv[:1]) == ["--selftest"]:
        return selftest()
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("proposers", help="per-proposer attribution of decided heights")
    p.add_argument("run_dir", help="a run's local dir, e.g. bench/runs/<run-id>")
    p.add_argument("--json", action="store_true", help="machine-readable output")
    q = sub.add_parser("phases", help="per-phase decomposition from height_timing lines")
    q.add_argument("logs", nargs="+", help="CL logs containing height_timing lines")
    a = ap.parse_args(argv)
    if a.cmd == "proposers":
        return proposers_main(a.run_dir, as_json=a.json)
    records = read_files(a.logs)
    if not records:
        print("no height_timing lines found — was the CL the lean-lane-instrumented build, "
              "run with ARC_HEIGHT_TIMING=1?", file=sys.stderr)
        return 1
    report(records)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
