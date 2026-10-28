#!/usr/bin/env python3
"""Draws bench/results.csv (from bench/run.sh) as SVG, standard library only.

    python3 bench/chart.py [results.csv] [size-for-bar-chart]

Writes bench/at-<size>.svg (git vs sprig per command at one size) and
bench/scaling.svg (each command against repo size).
"""
import csv
import math
import os
import sys
from collections import defaultdict
from html import escape

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "results.csv")
AT = int(sys.argv[2]) if len(sys.argv) > 2 else 3000
COMMITS = os.environ.get("COMMITS", "100")  # what run.sh was given
TOOLS = [("git", "#e8590c"), ("sprig", "#2b8a3e")]
INK, MUTED, GRID, BG = "#1f2328", "#59636e", "#d8dee4", "#ffffff"
FONT = "font-family='-apple-system,Segoe UI,Helvetica,Arial,sans-serif'"

data = defaultdict(dict)  # (command, files) -> {tool: value}
order = []
for r in csv.DictReader(open(SRC)):
    if r["command"] not in order:
        order.append(r["command"])
    data[(r["command"], int(r["files"]))][r["tool"]] = float(r["ms"])
sizes = sorted({f for _, f in data})


def text(x, y, s, size=12, fill=INK, anchor="start", weight="normal"):
    return (f"<text x='{x:.1f}' y='{y:.1f}' font-size='{size}' fill='{fill}' "
            f"text-anchor='{anchor}' font-weight='{weight}' {FONT}>{escape(s)}</text>")


def svg(w, h, body, title):
    return (f"<svg xmlns='http://www.w3.org/2000/svg' width='{w}' height='{h}' viewBox='0 0 {w} {h}'>"
            f"<title>{escape(title)}</title><rect width='{w}' height='{h}' fill='{BG}'/>"
            + "".join(body) + "</svg>\n")


def legend(x, y):
    out = []
    for tool, color in TOOLS:
        out.append(f"<rect x='{x}' y='{y - 9}' width='10' height='10' rx='2' fill='{color}'/>")
        out.append(text(x + 15, y, tool))
        x += 70
    return out


def fmt(v):
    return f"{v:.0f}" if v >= 100 else f"{v:.1f}"


def ticks(top):
    """A 'nice' step so the axis shows 4-6 ticks up to at least top."""
    raw = top / 5
    mag = 10 ** math.floor(math.log10(raw))
    step = next(m * mag for m in (1, 2, 2.5, 5, 10) if m * mag >= raw)
    return step, step * math.ceil(top / step)


def bars():
    cmds = [c for c in order if c != "storage (KB)" and (c, AT) in data]
    left, right, row, top = 170, 70, 34, 56
    w = 760
    h = top + row * len(cmds) + 40
    vmax = max(v for c in cmds for v in data[(c, AT)].values())
    step, axis = ticks(vmax)
    scale = (w - left - right) / axis
    body = [text(16, 24, f"git vs sprig, {AT:,} files, {COMMITS} commits (ms, lower is better)", 15, weight="600")]
    body += legend(left, 44)
    v = 0.0
    while v <= axis + 1e-9:
        x = left + v * scale
        body.append(f"<line x1='{x:.1f}' x2='{x:.1f}' y1='{top}' y2='{h - 30}' stroke='{GRID}'/>")
        body.append(text(x, h - 14, fmt(v), 11, MUTED, "middle"))
        v += step
    for i, c in enumerate(cmds):
        y = top + i * row
        body.append(text(left - 10, y + 20, c, 12, INK, "end"))
        for j, (tool, color) in enumerate(TOOLS):
            val = data[(c, AT)].get(tool)
            if val is None:
                continue
            by = y + 5 + j * 12
            body.append(f"<rect x='{left}' y='{by}' width='{max(val * scale, 1):.1f}' height='11' rx='2' fill='{color}'/>")
            body.append(text(left + val * scale + 5, by + 9.5, fmt(val), 10, MUTED))
    return svg(w, h, body, f"git vs sprig at {AT} files")


def lines():
    cmds = [c for c in order if all((c, s) in data for s in sizes)]
    cols, pw, ph, gap = 3, 250, 170, 18
    rows = math.ceil(len(cmds) / cols)
    w = cols * pw + (cols + 1) * gap
    h = 70 + rows * (ph + gap)
    body = [text(16, 24, "How each command scales with repo size (ms; storage in KB)", 15, weight="600")]
    body += legend(16, 48)
    lx = lambda s: math.log10(s)
    x0, x1 = lx(sizes[0]), lx(sizes[-1])
    for k, c in enumerate(cmds):
        ox = gap + (k % cols) * (pw + gap)
        oy = 64 + (k // cols) * (ph + gap)
        il, ir, it, ib = 40, 12, 26, 26  # inner plot margins
        vmax = max(v for s in sizes for v in data[(c, s)].values())
        step, axis = ticks(vmax)
        px = lambda s: ox + il + (lx(s) - x0) / (x1 - x0 or 1) * (pw - il - ir)
        py = lambda v: oy + ph - ib - v / axis * (ph - it - ib)
        body.append(f"<rect x='{ox}' y='{oy}' width='{pw}' height='{ph}' rx='6' fill='none' stroke='{GRID}'/>")
        body.append(text(ox + 10, oy + 17, c, 12, weight="600"))
        v = 0.0
        while v <= axis + 1e-9:
            body.append(f"<line x1='{ox + il}' x2='{ox + pw - ir}' y1='{py(v):.1f}' y2='{py(v):.1f}' stroke='{GRID}' stroke-width='0.6'/>")
            body.append(text(ox + il - 5, py(v) + 4, fmt(v), 10, MUTED, "end"))
            v += step
        for s in sizes:
            label = f"{s // 1000}k" if s >= 1000 else str(s)
            body.append(text(px(s), oy + ph - 9, label, 10, MUTED, "middle"))
        for tool, color in TOOLS:
            pts = [(px(s), py(data[(c, s)][tool])) for s in sizes if tool in data[(c, s)]]
            if not pts:
                continue
            body.append(f"<polyline fill='none' stroke='{color}' stroke-width='2' points='"
                        + " ".join(f"{x:.1f},{y:.1f}" for x, y in pts) + "'/>")
            body += [f"<circle cx='{x:.1f}' cy='{y:.1f}' r='3' fill='{color}'/>" for x, y in pts]
    return svg(w, h, body, "sprig and git scaling with repo size")


with open(os.path.join(HERE, f"at-{AT}.svg"), "w") as f:
    f.write(bars())
with open(os.path.join(HERE, "scaling.svg"), "w") as f:
    f.write(lines())
