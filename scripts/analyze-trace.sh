#!/usr/bin/env bash
# Summarize a tracing-chrome JSON trace.
#
# Usage: scripts/analyze-trace.sh <trace.json>
#
# Pairs B (begin) / E (end) events per (tid, name) using a stack so that
# nested spans of the same name are matched correctly, then reports
# count / total / avg / p50 / p95 / max in microseconds, sorted by total.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <trace.json>" >&2
  exit 2
fi

trace=$1

if ! command -v jq >/dev/null; then
  echo "jq is required" >&2
  exit 1
fi

# Step 1: flatten B/E events with name, tid, ph, ts.
# Step 2: a small awk program walks events per tid in order, maintaining a
#         per-name stack; on E it pops and emits a duration.
# Step 3: aggregate by name with sort/awk.

jq -r '.[] | select(.ph == "B" or .ph == "E") | "\(.tid)\t\(.name)\t\(.ph)\t\(.ts)"' "$trace" |
awk -F'\t' '
{
  tid=$1; name=$2; ph=$3; ts=$4;
  key = tid "|" name;
  if (ph == "B") {
    # push timestamp on the stack for this (tid, name).
    n = ++count[key];
    stack[key, n] = ts + 0.0;
  } else {
    n = count[key]--;
    if (n > 0) {
      dur = (ts + 0.0) - stack[key, n];
      delete stack[key, n];
      print name "\t" dur;
    }
  }
}' |
awk -F'\t' '
{
  name = $1; dur = $2 + 0.0;
  c[name]++;
  total[name] += dur;
  if (dur > max[name]) max[name] = dur;
  durs[name, c[name]] = dur;
}
END {
  for (n in c) {
    # sort durs[n,1..c[n]] ascending for percentiles
    for (i = 1; i <= c[n]; i++) sorted[i] = durs[n, i];
    for (i = 2; i <= c[n]; i++) {
      v = sorted[i]; j = i - 1;
      while (j > 0 && sorted[j] > v) { sorted[j+1] = sorted[j]; j-- }
      sorted[j+1] = v;
    }
    p50_idx = int((c[n] + 1) * 0.5); if (p50_idx < 1) p50_idx = 1;
    p95_idx = int((c[n] + 1) * 0.95); if (p95_idx < 1) p95_idx = 1;
    if (p95_idx > c[n]) p95_idx = c[n];
    p50 = sorted[p50_idx]; p95 = sorted[p95_idx];
    avg = total[n] / c[n];
    printf "%-20s  count=%-5d  total_us=%-10.0f  avg_us=%-7.0f  p50=%-7.0f  p95=%-7.0f  max=%-7.0f\n", \
      n, c[n], total[n], avg, p50, p95, max[n];
    for (i = 1; i <= c[n]; i++) delete sorted[i];
  }
}' |
sort -t= -k3 -nr
