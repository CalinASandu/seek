"""Generates the animated README Decimator view (docs/assets/decimator-dark.svg, decimator-light.svg).

Plain CSS inside the SVG, with the helpers of make-readme-space.py: a duplicate scan finds three groups of copies,
the pointer reviews and moves the extra copies to the Recycle Bin, and each one turns to dust left to right.
Run: python scripts/make-readme-decimator.py
"""

import importlib.util
import random
from pathlib import Path
from xml.sax.saxutils import escape

_spec = importlib.util.spec_from_file_location("space", Path(__file__).with_name("make-readme-space.py"))
space = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(space)
pct, window_anim, until, FONT, CYCLE = space.pct, space.window_anim, space.until, space.FONT, space.CYCLE

W, H = 960, 540
X0, X1 = 62, W - 62  # a copy row, from its checkbox to its date
FOUND = [0.9, 1.5, 2.1]  # when the scan finds each group
REVIEW_CLICK, MOVE_CLICK = 3.15, 4.3
GONE = [4.75, 5.2, 5.65]  # when each extra copy starts turning to dust
SWEEP = 0.55  # seconds for the dust to cross a row
FADE_OUT = 9.3

# name, size in GB, the copy kept, the extra copy (removed), its date
GROUPS = [
    ("ubuntu-24.04.iso", 6.1, r"D:\ISO\ubuntu-24.04.iso", r"C:\Users\Calin\Downloads\ubuntu-24.04.iso", "3 Oct 2026"),
    ("Win11_24H2.iso", 5.4, r"D:\ISO\Win11_24H2.iso", r"C:\Users\Calin\Downloads\Win11_24H2 (1).iso", "21 Sep 2026"),
    ("OBS 2026-10-12.mkv", 4.1, r"D:\Videos\OBS 2026-10-12.mkv", r"C:\Users\Calin\Videos\OBS 2026-10-12.mkv", "12 Oct 2026"),
]

# the app's own selector beam and sheet colours, on top of the Space theme
EXTRA = {
    "dark": dict(beam="#f5f5f7", beam_ink="#1d1d1f", sheet="#2c2c2e", green="#30d158", dim="#00000059"),
    "light": dict(beam="#1d1d1f", beam_ink="#ffffff", sheet="#ffffff", green="#34c759", dim="#0000002e"),
}


def segmented(x: float, y: float, labels: list[str], seg_w: float, h: float, c: dict, fs: float) -> str:
    """A pill switch like the app's; the label starting with * sits on the beam."""
    out = [f'<rect x="{x}" y="{y}" width="{seg_w * len(labels) + 4}" height="{h}" rx="{h / 2}" fill="{c["seg"]}"/>']
    for i, label in enumerate(labels):
        on = label.startswith("*")
        cx = x + 2 + seg_w * i + seg_w / 2
        if on:
            out.append(
                f'<rect x="{x + 2 + seg_w * i}" y="{y + 2}" width="{seg_w}" height="{h - 4}" rx="{h / 2 - 2}" '
                f'fill="{c["beam"]}" filter="url(#lift)"/>'
            )
        ink = f'fill="{c["beam_ink"]}" font-weight="600"' if on else f'fill="{c["text2"]}"'
        out.append(f'<text x="{cx}" y="{y + h / 2 + fs * 0.36:.1f}" font-size="{fs}" {ink} text-anchor="middle">{label.lstrip("*")}</text>')
    return "".join(out)


def checkbox(x: float, y: float, on: bool, c: dict) -> str:
    if not on:
        return f'<rect x="{x}" y="{y}" width="13" height="13" rx="3.5" fill="none" stroke="{c["muted"]}" stroke-width="1.2"/>'
    return (
        f'<rect x="{x}" y="{y}" width="13" height="13" rx="3.5" fill="{c["accent"]}"/>'
        f'<path d="M{x + 3} {y + 6.8}l2.6 2.6 4.6-5.2" fill="none" stroke="#ffffff" stroke-width="1.7" '
        f'stroke-linecap="round" stroke-linejoin="round"/>'
    )


def text_width(s: str, size: float) -> float:
    return len(s) * size * 0.53


def dust(key: str, t0: float, y: float, spans: list[tuple[float, float, str]], css: list, rng: random.Random) -> list[str]:
    """Specks over a row that light up as the sweep passes them, then drift up and right and fade."""
    css.append(
        f"@keyframes {key}{{0%,{pct(t0)}{{opacity:0;transform:translate(0,0)}}{pct(t0 + 0.02)}{{opacity:1;"
        f"transform:translate(0,0)}}{pct(t0 + 1.0)},100%{{opacity:0;transform:translate(var(--x),var(--y))}}}}"
        f".{key}{{animation:{key} {CYCLE}s cubic-bezier(.2,.65,.3,1) infinite;opacity:0}}"
    )
    out = []
    for xa, xb, color in spans:
        x = xa
        while x < xb:
            for _ in range(2):
                px, py = x + rng.uniform(0, 4), y - 10 + rng.uniform(0, 12)
                dx, dy = rng.uniform(18, 120), rng.uniform(-48, 10)
                delay = (px - X0) / (X1 - X0) * SWEEP
                size = rng.choice((1.6, 2.2, 2.8))
                out.append(
                    f'<rect class="{key}" x="{px:.1f}" y="{py:.1f}" width="{size}" height="{size}" rx=".5" '
                    f'fill="{color}" style="--x:{dx:.0f}px;--y:{dy:.0f}px;animation-delay:{delay:.2f}s"/>'
                )
            x += 5
    return out


def build(theme: str) -> str:
    c = {**space.THEMES[theme], **EXTRA[theme]}
    rng = random.Random(7)
    css = [f"text{{font-family:{FONT}}}"]
    out, clips = [], []

    # window, title bar with the view switch on Decimator
    out.append(f'<rect x="0" y="0" width="{W}" height="{H}" rx="16" fill="{c["bg"]}"/>')
    out.append(
        f'<rect x="24" y="24" width="{W - 48}" height="{H - 48}" rx="12" fill="{c["window"]}" '
        f'stroke="{c["border"]}" filter="url(#shadow)"/>'
    )
    out.append(
        f'<g transform="translate(44,44)"><rect width="22" height="22" rx="6" fill="{c["accent"]}"/>'
        f'<circle cx="10" cy="10" r="5" fill="none" stroke="#ffffff" stroke-width="2.2"/>'
        f'<path d="M14 14l4 4" stroke="#ffffff" stroke-width="2.4" stroke-linecap="round"/></g>'
    )
    out.append(f'<text x="76" y="61" font-size="15" font-weight="600" fill="{c["text"]}">Seek</text>')
    out.append(segmented(W / 2 - 137, 40, ["Search", "Space", "*Decimator"], 90, 30, c, 13))
    for i, d in enumerate(["M0 0h12", "M0 -6h12v12h-12z", "M0 -6l12 12M12 -6l-12 12"]):
        out.append(
            f'<path transform="translate({W - 150 + i * 36},56)" d="{d}" fill="none" '
            f'stroke="{c["muted"]}" stroke-width="1.3"/>'
        )
    out.append(f'<path d="M24 84H{W - 24}" stroke="{c["border"]}"/>')

    # tabs, Recently removed, the big number and Rescan
    out.append(segmented(46, 96, ["*Duplicates", "Leftovers", "Apps"], 92, 26, c, 12))
    out.append(f'<text x="{W - 46}" y="114" font-size="12.5" fill="{c["text2"]}" text-anchor="end">Recently removed</text>')
    left = [sum(g[1] for g in GROUPS[i:]) for i in range(len(GROUPS))]  # GB still to free before each removal
    figures = [("0 B", None, 0.0, FOUND[0])]
    found = 0.0
    for i, g in enumerate(GROUPS):
        found += g[1]
        figures.append((f"{found:.1f} GB", None, FOUND[i], FOUND[i + 1] if i + 1 < len(FOUND) else GONE[0] + 0.45))
    for i in range(1, len(GROUPS)):
        figures.append((f"{left[i]:.1f} GB", None, GONE[i - 1] + 0.45, GONE[i] + 0.45))
    done = GONE[-1] + 0.6
    figures.append((f"{left[0]:.1f} GB", c["green"], done, FADE_OUT))
    for n, (label, color, show, hide) in enumerate(figures):
        name = f"f{n}"
        hide -= 0.3  # gone before the next number fades in, so two never overlap
        css.append(until(name, hide) if show == 0 else window_anim(name, show, hide))
        tail = f'<tspan font-size="20" font-weight="600" fill="{c["green"]}"> freed</tspan>' if color else ""
        out.append(
            f'<text class="{name}" x="46" y="178" font-size="36" font-weight="700" fill="{color or c["text"]}">'
            f"{label}{tail}</text>"
        )
    captions = [
        ("Looking for files that are exact copies…", 0.0, FOUND[-1] + 0.1),
        (f"in {len(GROUPS)} groups of copies", FOUND[-1] + 0.1, done),
        (f"{len(GROUPS)} copies are in the Recycle Bin. Put them back anytime.", done, FADE_OUT),
    ]
    for n, (label, show, hide) in enumerate(captions):
        name = f"k{n}"
        css.append(until(name, hide) if show == 0 else window_anim(name, show, hide))
        out.append(f'<text class="{name}" x="46" y="202" font-size="13" fill="{c["muted"]}">{label}</text>')
    out.append(
        f'<rect x="{W - 142}" y="154" width="96" height="30" rx="8" fill="{c["accent"]}"/>'
        f'<text x="{W - 94}" y="174" font-size="13" font-weight="600" fill="#ffffff" text-anchor="middle">Rescan</text>'
    )

    # the scan's progress line, then a plain divider
    css.append(
        f"@keyframes sl{{0%,{pct(0.3)}{{transform:scaleX(0)}}{pct(FOUND[-1] + 0.1)},100%{{transform:scaleX(1)}}}}"
        f".sl{{animation:sl {CYCLE}s cubic-bezier(.4,0,.4,1) infinite;transform-origin:46px 0}}"
    )
    css.append(until("sw", FOUND[-1] + 0.25))
    out.append(
        f'<path d="M46 218H{W - 46}" stroke="{c["border"]}"/><g class="sw">'
        f'<rect x="46" y="216.5" width="{W - 92}" height="3" rx="1.5" fill="{c["track"]}"/>'
        f'<rect class="sl" x="46" y="216.5" width="{W - 92}" height="3" rx="1.5" fill="{c["accent"]}"/></g>'
    )

    # the groups: each found during the scan, its extra copy swept away to dust, then the group closes
    specks = []
    for i, (name, gb, keep, extra, date) in enumerate(GROUPS):
        y0 = 232 + i * 76
        t = GONE[i]
        cls = f"g{i}"
        css.append(window_anim(cls, FOUND[i], t + SWEEP + 0.15, rise=6))
        keep_y, extra_y = y0 + 43, y0 + 62
        css.append(
            f"@keyframes c{i}{{0%,{pct(t)}{{transform:translateX(0)}}{pct(t + SWEEP)},100%{{transform:translateX("
            f"{X1 - X0 + 12}px)}}}}.c{i}{{animation:c{i} {CYCLE}s linear infinite}}"
        )
        clips.append(
            f'<clipPath id="cut{i}"><rect class="c{i}" x="{X0 - 6}" y="{extra_y - 14}" width="{X1 - X0 + 12}" '
            f'height="20"/></clipPath>'
        )
        out.append(
            f'<g class="{cls}"><rect x="46" y="{y0}" width="{W - 92}" height="70" rx="10" fill="{c["text"]}" '
            f'fill-opacity="0.035" stroke="{c["border"]}"/>'
            f'<text x="{X0}" y="{y0 + 22}" font-size="13.5" font-weight="600" fill="{c["text"]}">{escape(name)}</text>'
            f'<text x="{X1}" y="{y0 + 22}" font-size="12.5" fill="{c["muted"]}" text-anchor="end">2 copies · '
            f'<tspan fill="{c["text"]}" font-weight="600">{gb} GB</tspan> to free</text>'
            f'{checkbox(X0, keep_y - 10, False, c)}'
            f'<text x="84" y="{keep_y}" font-size="12.5" fill="{c["text2"]}">{escape(keep)}</text>'
            f'<rect x="{X1 - 38}" y="{keep_y - 12}" width="38" height="16" rx="8" fill="{c["green"]}" fill-opacity="0.16"/>'
            f'<text x="{X1 - 19}" y="{keep_y}" font-size="11" font-weight="600" fill="{c["green"]}" '
            f'text-anchor="middle">Keep</text>'
            f'<g clip-path="url(#cut{i})">{checkbox(X0, extra_y - 10, True, c)}'
            f'<text x="84" y="{extra_y}" font-size="12.5" fill="{c["text2"]}">{escape(extra)}</text>'
            f'<text x="{X1}" y="{extra_y}" font-size="12" fill="{c["muted"]}" text-anchor="end">{date}</text></g></g>'
        )
        spans = [
            (X0, X0 + 13, c["accent"]),
            (84, 84 + text_width(extra, 12.5), c["text2"]),
            (X1 - text_width(date, 12), X1, c["muted"]),
        ]
        specks += dust(f"d{i}", t, extra_y, spans, css, rng)
    out.append(f"<g>{''.join(specks)}</g>")

    # once every group is gone
    css.append(window_anim("empty", done + 0.3, FADE_OUT, rise=6))
    out.append(
        f'<g class="empty"><circle cx="{W / 2}" cy="318" r="20" fill="{c["green"]}" fill-opacity="0.16"/>'
        f'<path d="M{W / 2 - 8} 318l5.5 5.5 10.5-11" fill="none" stroke="{c["green"]}" stroke-width="2.6" '
        f'stroke-linecap="round" stroke-linejoin="round"/>'
        f'<text x="{W / 2}" y="364" font-size="15" font-weight="600" fill="{c["text"]}" text-anchor="middle">'
        f"No duplicates left</text>"
        f'<text x="{W / 2}" y="386" font-size="12.5" fill="{c["muted"]}" text-anchor="middle">'
        f"Nothing was deleted: the copies wait in the Recycle Bin.</text></g>"
    )

    # the selection bar and Review…
    css.append(window_anim("bar", FOUND[-1] + 0.2, MOVE_CLICK + 0.2))
    out.append(
        f'<g class="bar"><path d="M24 470H{W - 24}" stroke="{c["border"]}"/>'
        f'<text x="46" y="498" font-size="13" fill="{c["text2"]}"><tspan font-weight="600" fill="{c["text"]}">'
        f'{len(GROUPS)} copies</tspan> selected · {left[0]:.1f} GB</text>'
        f'<rect x="{W - 146}" y="480" width="100" height="28" rx="8" fill="{c["accent"]}"/>'
        f'<text x="{W - 96}" y="499" font-size="13" font-weight="600" fill="#ffffff" text-anchor="middle">Review…</text></g>'
    )

    # the review sheet
    sheet_at, sheet_off = REVIEW_CLICK + 0.15, MOVE_CLICK + 0.15
    css.append(window_anim("dim", sheet_at, sheet_off))
    css.append(window_anim("sheet", sheet_at, sheet_off, rise=10))
    sx, sy, sw, sh = W / 2 - 220, 200, 440, 150
    out.append(f'<rect class="dim" x="24" y="24" width="{W - 48}" height="{H - 48}" rx="12" fill="{c["dim"]}"/>')
    out.append(
        f'<g class="sheet"><rect x="{sx}" y="{sy}" width="{sw}" height="{sh}" rx="14" fill="{c["sheet"]}" '
        f'stroke="{c["border"]}" filter="url(#shadow)"/>'
        f'<text x="{W / 2}" y="{sy + 40}" font-size="15.5" font-weight="600" fill="{c["text"]}" text-anchor="middle">'
        f"Move {len(GROUPS)} copies to the Recycle Bin?</text>"
        f'<text x="{W / 2}" y="{sy + 64}" font-size="12.5" fill="{c["muted"]}" text-anchor="middle">'
        f"{left[0]:.1f} GB · put them back anytime from Recently removed</text>"
        f'<rect x="{W / 2 - 102}" y="{sy + 94}" width="80" height="30" rx="8" fill="{c["seg"]}"/>'
        f'<text x="{W / 2 - 62}" y="{sy + 114}" font-size="13" fill="{c["text"]}" text-anchor="middle">Cancel</text>'
        f'<rect x="{W / 2 - 12}" y="{sy + 94}" width="150" height="30" rx="8" fill="{c["accent"]}"/>'
        f'<text x="{W / 2 + 63}" y="{sy + 114}" font-size="13" font-weight="600" fill="#ffffff" '
        f'text-anchor="middle">Move to Recycle Bin</text></g>'
    )

    # pointer: to Review…, click, to Move to Recycle Bin, click
    p0, p1, p2 = (700, 430), (W - 100, 496), (W / 2 + 60, sy + 111)
    css.append(
        f"@keyframes pm{{0%,{pct(2.5)}{{transform:translate({p0[0]}px,{p0[1]}px)}}{pct(3.0)},{pct(3.6)}{{"
        f"transform:translate({p1[0]}px,{p1[1]}px)}}{pct(4.1)},100%{{transform:translate({p2[0]}px,{p2[1]}px)}}}}"
        f".pm{{animation:pm {CYCLE}s cubic-bezier(.3,.7,.3,1) infinite}}"
    )
    css.append(window_anim("pv", 2.4, MOVE_CLICK + 0.3))
    dips = "".join(
        f"{pct(t - 0.08)}{{transform:scale(1)}}{pct(t)}{{transform:scale(.82)}}{pct(t + 0.08)}{{transform:scale(1)}}"
        for t in (REVIEW_CLICK, MOVE_CLICK)
    )
    css.append(f"@keyframes pc{{0%{{transform:scale(1)}}{dips}100%{{transform:scale(1)}}}}.pc{{animation:pc {CYCLE}s linear infinite}}")
    out.append(
        f'<g class="pv"><g class="pm"><g class="pc"><path d="M0 0V17l4.2-4 2.8 6.3 2.6-1.1-2.8-6.2H12.6z" '
        f'fill="#ffffff" stroke="#000000" stroke-width="1.1" stroke-linejoin="round"/></g></g></g>'
    )

    defs = (
        f'<defs><filter id="shadow" x="-5%" y="-5%" width="110%" height="115%">'
        f'<feDropShadow dx="0" dy="6" stdDeviation="10" flood-color="{c["shadow"]}"/></filter>'
        f'<filter id="lift" x="-20%" y="-30%" width="140%" height="190%">'
        f'<feDropShadow dx="0" dy="1" stdDeviation="1.5" flood-color="#00000033"/></filter>{"".join(clips)}</defs>'
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" '
        f'aria-label="Seek Decimator: a scan finds three groups of copies, then the extra copies turn to dust on '
        f'their way to the Recycle Bin">'
        f"<style>{''.join(css)}</style>{defs}{''.join(out)}</svg>\n"
    )


def main() -> None:
    out_dir = Path(__file__).resolve().parent.parent / "docs" / "assets"
    for theme in space.THEMES:
        (out_dir / f"decimator-{theme}.svg").write_text(build(theme), encoding="utf-8")
        print(f"wrote docs/assets/decimator-{theme}.svg")


if __name__ == "__main__":
    main()
