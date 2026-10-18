"""Generates the animated README Space view (docs/assets/space-dark.svg, space-light.svg).

Plain CSS inside the SVG, like make-readme-hero.py: the drive's sunburst sweeps in ring by ring, the pointer
clicks a folder, the view opens that folder (rings and list change), then it loops.
Run: python scripts/make-readme-space.py
"""

import math
from pathlib import Path
from xml.sax.saxutils import escape

W, H = 960, 500
CYCLE = 10.0  # seconds per loop
CX, CY = 248, 321  # sunburst center
RINGS = [(58, 98), (100, 126), (128, 146)]  # inner/outer radius per ring
GAP = 1.2  # px between neighbouring arcs
SWEEP = 0.9  # seconds for one ring to sweep a full circle
OPEN_AT = 4.3  # the click: the drive view leaves, the folder view arrives
FADE_OUT = 9.3

# name, size in GB, hue (index into chart colors, -1 = file), children (same shape, hue inherited)
DRIVE = ("System (C:)", "335 GB", "2,964 files", [
    ("Program Files (x86)", 139, 0, [("Steam", 118, [("steamapps", 116, [])]), ("Microsoft", 9, []), ("Adobe", 6, [])]),
    ("Users", 104, 1, [
        ("Calin", 98, [("Videos", 41, []), ("AppData", 22, []), ("Downloads", 18, []), ("Documents", 9, []),
                       ("Pictures", 6, [])]),
        ("Public", 4, []),
    ]),
    ("Windows", 31, 2, [("WinSxS", 12, []), ("System32", 8, []), ("Installer", 5, []), ("SysWOW64", 3, [])]),
    ("Program Files", 25, 3, [("Microsoft Office", 7, []), ("NVIDIA", 6, []), ("Docker", 5, [])]),
    ("pagefile.sys", 16, -1, []),
    ("hiberfil.sys", 13, -1, []),
    ("ProgramData", 6.4, 4, [("Package Cache", 3, []), ("Microsoft", 2, [])]),
    ("swapfile.sys", 0.256, -1, []),
])
FOLDER = ("Calin", "98 GB", "41,208 files", [
    ("Videos", 41, 0, [("OBS", 24, [("2026", 17, [])]), ("Movies", 15, [])]),
    ("AppData", 22, 1, [("Local", 17, [("Docker", 9, []), ("npm-cache", 3, [])]), ("Roaming", 4, [])]),
    ("Downloads", 18, 2, [("ubuntu-24.04.iso", 6, []), ("Win11.iso", 5, [])]),
    ("Documents", 9, 3, [("Projects", 5, []), ("Uni", 3, [])]),
    ("Pictures", 6, 5, [("Camera", 4, []), ("Screenshots", 2, [])]),
    ("Desktop", 1.6, 4, []),
])
CLICKED = ("Users", "Calin")  # the second-ring arc the pointer opens

THEMES = {
    "dark": dict(
        bg="#202020", window="#1e1e20", border="#ffffff1a", text="#f5f5f7", text2="#ebebf5d9", muted="#98989f",
        accent="#0a84ff", track="#ffffff14", seg="#7676803d", thumb="#636366", shadow="#00000080",
        hover="#ffffff40", tip="#3a3a3c", tone="#aeaeb2", file="#545458",
        chart=["#0a84ff", "#ff9f0a", "#bf5af2", "#30d158", "#ff375f", "#40c8e0"],
    ),
    "light": dict(
        bg="#f3f3f3", window="#ffffff", border="#0000001a", text="#1d1d1f", text2="#3c3c43", muted="#6e6e73",
        accent="#007aff", track="#0000000f", seg="#7676801f", thumb="#ffffff", shadow="#00000026",
        hover="#0000001f", tip="#ffffff", tone="#ffffff", file="#c7c7cc",
        chart=["#007aff", "#ff9500", "#af52de", "#34c759", "#ff2d55", "#30b0c7"],
    ),
}

FONT = "'Segoe UI Variable Text','Segoe UI',system-ui,-apple-system,'Helvetica Neue',Arial,sans-serif"


def pct(t: float) -> str:
    return f"{max(0.0, min(100.0, t / CYCLE * 100)):.2f}%"


def mix(a: str, b: str, wa: float) -> str:
    """`wa` of colour a, the rest b (like color-mix)."""
    ca = [int(a[i : i + 2], 16) for i in (1, 3, 5)]
    cb = [int(b[i : i + 2], 16) for i in (1, 3, 5)]
    return "#" + "".join(f"{round(x * wa + y * (1 - wa)):02x}" for x, y in zip(ca, cb))


def window_anim(name: str, show: float, hide: float, rise: float = 0.0, extra: str = "") -> str:
    """Hidden until `show`, visible (optionally sliding up by `rise` px) until `hide`, then fades out."""
    a0, a1, f0, f1 = pct(show), pct(show + 0.25), pct(hide), pct(hide + 0.3)
    tr0 = f"transform:translateY({rise}px);" if rise else ""
    tr1 = "transform:translateY(0);" if rise else ""
    return (
        f"@keyframes {name}{{0%,{a0}{{opacity:0;{tr0}}}{a1},{f0}{{opacity:1;{tr1}}}{f1},100%{{opacity:0;{tr1}}}}}"
        f".{name}{{animation:{name} {CYCLE}s linear infinite;opacity:0;{extra}}}"
    )


def until(name: str, hide: float) -> str:
    """Visible from the start of the loop until `hide`, then fades out."""
    return (
        f"@keyframes {name}{{0%,{pct(hide)}{{opacity:1}}{pct(hide + 0.3)},100%{{opacity:0}}}}"
        f".{name}{{animation:{name} {CYCLE}s linear infinite}}"
    )


def polar(r: float, deg: float) -> tuple[float, float]:
    """Point at radius r, `deg` clockwise from 12 o'clock."""
    a = math.radians(deg)
    return CX + r * math.sin(a), CY - r * math.cos(a)


def arc_path(r: float, a0: float, a1: float) -> str:
    x0, y0 = polar(r, a0)
    x1, y1 = polar(r, a1)
    large = 1 if a1 - a0 > 180 else 0
    return f"M{x0:.2f} {y0:.2f}A{r} {r} 0 {large} 1 {x1:.2f} {y1:.2f}"


def sunburst(view: tuple, c: dict, css: list, key: str, start: float) -> tuple[list[str], dict]:
    """Arcs of `view` as thick stroked paths that sweep in clockwise, each ring a little after the one inside.
    Returns the SVG and {(parent, name): (ring, a0, a1)} for every arc drawn."""
    total = sum(item[1] for item in view[3])
    out, where = [], {}
    n = 0

    def walk(items, depth, a0, span_per_gb, hue, parent):
        nonlocal n
        a = a0
        for item in items:
            name, size = item[0], item[1]
            h = item[2] if depth == 0 else hue
            kids = item[3] if depth == 0 else item[2]
            span = size * span_per_gb
            r0, r1 = RINGS[depth]
            r = (r0 + r1) / 2
            gap = math.degrees(GAP / r) / 2
            if span > 2 * gap + 0.3:
                base = c["chart"][h] if h >= 0 else c["file"]
                color = base if depth == 0 or h < 0 else mix(base, c["tone"], 1 - depth * 0.29)
                cls = f"{key}{n}"
                n += 1
                t0 = start + depth * 0.3 + (a + gap) / 360 * SWEEP
                t1 = t0 + (span - 2 * gap) / 360 * SWEEP
                css.append(
                    f"@keyframes {cls}{{0%,{pct(t0)}{{stroke-dashoffset:100}}{pct(t1)},100%{{stroke-dashoffset:0}}}}"
                    f".{cls}{{animation:{cls} {CYCLE}s linear infinite;stroke-dashoffset:100}}"
                )
                out.append(
                    f'<path class="{cls}" d="{arc_path(r, a + gap, a + span - gap)}" fill="none" stroke="{color}" '
                    f'stroke-width="{r1 - r0}" pathLength="100" stroke-dasharray="100 101"/>'
                )
                where[(parent, name)] = (depth, a, a + span)
            if kids and depth + 1 < len(RINGS):
                walk(kids, depth + 1, a, span_per_gb, h, name)
            a += span

    walk(view[3], 0, 0.0, 360 / total, -1, None)
    return out, where


def folder_icon(x: float, y: float, color: str) -> str:
    return (
        f'<path transform="translate({x},{y})" d="M1 3.5A1.5 1.5 0 0 1 2.5 2h3.6l1.6 1.8h5.8A1.5 1.5 0 0 1 15 5.3v6.2'
        f'a1.5 1.5 0 0 1-1.5 1.5h-11A1.5 1.5 0 0 1 1 11.5z" fill="{color}" fill-opacity="0.26" '
        f'stroke="{color}" stroke-width="1.3" stroke-linejoin="round"/>'
    )


def file_icon(x: float, y: float, color: str, knob: str) -> str:
    return (
        f'<g transform="translate({x},{y})" fill="{knob}" stroke="{color}" stroke-width="1.3" stroke-linecap="round">'
        '<path d="M2 4h12M2 8h12M2 12h12"/><circle cx="5" cy="4" r="1.6"/><circle cx="10" cy="8" r="1.6"/>'
        '<circle cx="6" cy="12" r="1.6"/></g>'
    )


def fmt_size(gb: float) -> str:
    return f"{gb * 1000:.0f} MB" if gb < 1 else (f"{gb:.1f} GB" if gb < 10 and gb != int(gb) else f"{gb:.0f} GB")


def item_list(view: tuple, c: dict, css: list, key: str, show: float, hide: float) -> list[str]:
    """The Contents list beside the sunburst: icon, name, size and a bar relative to the largest item."""
    items = view[3]
    largest = max(i[1] for i in items)
    out = []
    x0, x1, y0, step = 486, 900, 230, 31
    for i, (name, size, hue, _) in enumerate(items):
        y = y0 + i * step
        cls = f"{key}r{i}"
        css.append(window_anim(cls, show + i * 0.07, hide, rise=6))
        color = c["chart"][hue] if hue >= 0 else c["muted"]
        icon = folder_icon(x0, y - 11, color) if hue >= 0 else file_icon(x0, y - 11, c["muted"], c["window"])
        bar_w = (x1 - 110 - (x0 + 26)) * size / largest
        out.append(
            f'<g class="{cls}">{icon}'
            f'<text x="{x0 + 26}" y="{y + 1}" font-size="13.5" fill="{c["text"]}">{escape(name)}</text>'
            f'<text x="{x1}" y="{y + 5}" font-size="13" fill="{c["text2"]}" text-anchor="end">{fmt_size(size)}</text>'
            f'<rect x="{x0 + 26}" y="{y + 7}" width="{x1 - 110 - (x0 + 26)}" height="3" rx="1.5" fill="{c["track"]}"/>'
            f'<rect x="{x0 + 26}" y="{y + 7}" width="{max(bar_w, 2):.1f}" height="3" rx="1.5" fill="{color}"/></g>'
        )
    return out


def build(theme: str) -> str:
    c = THEMES[theme]
    css = [f"text{{font-family:{FONT}}}"]
    out = []
    # window and title bar, with the Search | Space switch on Space
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
    sx, sw = W / 2 - 75, 150
    out.append(f'<rect x="{sx}" y="40" width="{sw}" height="30" rx="8" fill="{c["seg"]}"/>')
    out.append(
        f'<rect x="{sx + sw / 2}" y="42" width="{sw / 2 - 2}" height="26" rx="6" fill="{c["thumb"]}" '
        f'filter="url(#lift)"/>'
    )
    out.append(f'<text x="{sx + sw / 4}" y="60" font-size="13" fill="{c["text2"]}" text-anchor="middle">Search</text>')
    out.append(
        f'<text x="{sx + sw * 3 / 4}" y="60" font-size="13" font-weight="600" fill="{c["text"]}" '
        f'text-anchor="middle">Space</text>'
    )
    for i, d in enumerate(["M0 0h12", "M0 -6h12v12h-12z", "M0 -6l12 12M12 -6l-12 12"]):
        out.append(
            f'<path transform="translate({W - 150 + i * 36},56)" d="{d}" fill="none" '
            f'stroke="{c["muted"]}" stroke-width="1.3"/>'
        )
    out.append(f'<path d="M24 84H{W - 24}" stroke="{c["border"]}"/>')

    # header: drive icon + where we are (crossfades on the click), then the drive's capacity bar
    out.append(
        f'<g transform="translate(46,98)" fill="none" stroke="{c["text"]}" stroke-width="1.4">'
        '<rect x="0.7" y="5" width="16" height="8" rx="2"/><path d="M2.5 5l2-4h8l2 4" stroke-linejoin="round"/>'
        f'<circle cx="13" cy="9" r="0.9" fill="{c["text"]}" stroke="none"/></g>'
    )
    a_hide, b_show = OPEN_AT, OPEN_AT + 0.25
    css.append(until("ha", a_hide))
    css.append(window_anim("hb", b_show, FADE_OUT))
    out.append(f'<text class="ha" x="72" y="112" font-size="16" font-weight="600" fill="{c["text"]}">{DRIVE[0]}</text>')
    out.append(
        f'<text class="hb" x="72" y="112" font-size="16" font-weight="600" fill="{c["text"]}">'
        f'<tspan fill="{c["muted"]}" font-weight="400">System (C:)  ›  Users  ›  </tspan>{FOLDER[0]}</text>'
    )
    out.append(
        f'<text class="ha" x="{W - 46}" y="112" font-size="13" fill="{c["muted"]}" text-anchor="end">'
        f'{DRIVE[1]} · {DRIVE[2]}</text>'
    )
    out.append(
        f'<text class="hb" x="{W - 46}" y="112" font-size="13" fill="{c["muted"]}" text-anchor="end">'
        f'{FOLDER[1]} · {FOLDER[2]}</text>'
    )
    bx, bw, by, used, cap = 46, W - 92, 126, 358, 476
    out.append(f'<rect x="{bx}" y="{by}" width="{bw}" height="7" rx="3.5" fill="{c["track"]}"/>')
    x = bx
    for name, size, hue, _ in DRIVE[3]:
        w = bw * size / cap
        if w >= 1.5:
            fill = c["chart"][hue] if hue >= 0 else c["file"]
            out.append(f'<rect x="{x + 0.5:.1f}" y="{by}" width="{w - 1:.1f}" height="7" fill="{fill}"/>')
        x += w
    out.append(
        f'<text x="{bx}" y="{by + 26}" font-size="12.5" fill="{c["muted"]}">'
        f'<tspan font-weight="600" fill="{c["text"]}">{used} GB</tspan> of {cap} GB used</text>'
    )
    out.append(
        f'<text x="{bx + bw}" y="{by + 26}" font-size="12.5" fill="{c["muted"]}" text-anchor="end">'
        f'{cap - used} GB free</text>'
    )
    out.append(f'<path d="M24 166H{W - 24}" stroke="{c["border"]}"/>')
    out.append(f'<path d="M466 166V{H - 24}" stroke="{c["border"]}"/>')

    # list tabs (Contents selected)
    tx, tw = 594, 212
    out.append(f'<rect x="{tx}" y="176" width="{tw}" height="24" rx="7" fill="{c["seg"]}"/>')
    out.append(f'<rect x="{tx + 2}" y="178" width="{tw / 3 - 2:.1f}" height="20" rx="5" fill="{c["thumb"]}" filter="url(#lift)"/>')
    for i, label in enumerate(["Contents", "Largest", "By type"]):
        bold = ' font-weight="600"' if i == 0 else ""
        ink = c["text"] if i == 0 else c["text2"]
        out.append(
            f'<text x="{tx + tw / 6 + i * tw / 3:.1f}" y="192" font-size="11.5"{bold} fill="{ink}" '
            f'text-anchor="middle">{label}</text>'
        )

    # the drive view: sweeps in, then zooms out of the way when the folder is opened
    arcs_a, where = sunburst(DRIVE, c, css, "a", 0.35)
    css.append(
        f"@keyframes va{{0%,{pct(OPEN_AT)}{{opacity:1;transform:scale(1)}}{pct(OPEN_AT + 0.3)},100%{{opacity:0;"
        f"transform:scale(1.12)}}}}.va{{animation:va {CYCLE}s ease-in infinite;transform-origin:{CX}px {CY}px}}"
    )
    ring, h0, h1 = where[CLICKED]
    r0, r1 = RINGS[ring]
    css.append(window_anim("hov", 3.35, OPEN_AT - 0.1))
    hover = (
        f'<path class="hov" d="{arc_path((r0 + r1) / 2, h0 + 0.4, h1 - 0.4)}" fill="none" stroke="{c["hover"]}" '
        f'stroke-width="{r1 - r0}"/>'
    )
    css.append(window_anim("ca", 1.0, OPEN_AT - 0.05))
    center_a = (
        f'<g class="ca"><text x="{CX}" y="{CY - 16}" font-size="12.5" fill="{c["text2"]}" text-anchor="middle">'
        f'{DRIVE[0]}</text><text x="{CX}" y="{CY + 11}" font-size="26" font-weight="700" fill="{c["text"]}" '
        f'text-anchor="middle">{DRIVE[1]}</text><text x="{CX}" y="{CY + 30}" font-size="11.5" fill="{c["muted"]}" '
        f'text-anchor="middle">{DRIVE[2]}</text></g>'
    )
    out.append(f'<g class="va">{"".join(arcs_a)}{hover}{center_a}</g>')
    out += item_list(DRIVE, c, css, "a", 0.6, OPEN_AT - 0.2)

    # the folder view
    arcs_b, _ = sunburst(FOLDER, c, css, "b", OPEN_AT + 0.35)
    css.append(window_anim("vb", OPEN_AT + 0.2, FADE_OUT))
    css.append(window_anim("cb", OPEN_AT + 0.7, FADE_OUT))
    center_b = (
        f'<g class="cb"><text x="{CX}" y="{CY - 16}" font-size="12.5" fill="{c["text2"]}" text-anchor="middle">'
        f'{FOLDER[0]}</text><text x="{CX}" y="{CY + 11}" font-size="26" font-weight="700" fill="{c["text"]}" '
        f'text-anchor="middle">{FOLDER[1]}</text><text x="{CX}" y="{CY + 30}" font-size="11.5" fill="{c["muted"]}" '
        f'text-anchor="middle">{FOLDER[2]}</text></g>'
    )
    out.append(f'<g class="vb">{"".join(arcs_b)}{center_b}</g>')
    out += item_list(FOLDER, c, css, "b", OPEN_AT + 0.45, FADE_OUT)

    # pointer: glides onto the arc, a tooltip names it, it clicks
    mid = (h0 + h1) / 2
    px, py = polar((r0 + r1) / 2, mid)
    sx0, sy0 = 410, 470
    css.append(
        f"@keyframes pm{{0%,{pct(2.3)}{{transform:translate({sx0}px,{sy0}px)}}{pct(3.3)},100%{{"
        f"transform:translate({px:.1f}px,{py:.1f}px)}}}}.pm{{animation:pm {CYCLE}s cubic-bezier(.3,.7,.3,1) infinite}}"
    )
    css.append(window_anim("pv", 2.3, OPEN_AT + 0.1))
    css.append(
        f"@keyframes pc{{0%,{pct(OPEN_AT - 0.2)}{{transform:scale(1)}}{pct(OPEN_AT - 0.1)}{{transform:scale(.82)}}"
        f"{pct(OPEN_AT)},100%{{transform:scale(1)}}}}.pc{{animation:pc {CYCLE}s linear infinite}}"
    )
    css.append(window_anim("tip", 3.55, OPEN_AT - 0.1))
    tip_w = 118
    out.append(
        f'<g class="pv"><g class="pm"><g class="tip"><rect x="14" y="-34" width="{tip_w}" height="26" rx="6" '
        f'fill="{c["tip"]}" stroke="{c["border"]}" filter="url(#lift)"/><text x="24" y="-17" font-size="12" '
        f'fill="{c["text"]}"><tspan font-weight="600">{CLICKED[1]}</tspan>  ·  98 GB</text></g>'
        f'<g class="pc"><path d="M0 0V17l4.2-4 2.8 6.3 2.6-1.1-2.8-6.2H12.6z" fill="#ffffff" stroke="#000000" '
        f'stroke-width="1.1" stroke-linejoin="round"/></g></g></g>'
    )

    defs = (
        f'<defs><filter id="shadow" x="-5%" y="-5%" width="110%" height="115%">'
        f'<feDropShadow dx="0" dy="6" stdDeviation="10" flood-color="{c["shadow"]}"/></filter>'
        f'<filter id="lift" x="-20%" y="-30%" width="140%" height="190%">'
        f'<feDropShadow dx="0" dy="1" stdDeviation="1.5" flood-color="#00000033"/></filter></defs>'
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" '
        f'aria-label="Seek Space view: a sunburst of what fills drive C:, then opening the Calin folder">'
        f"<style>{''.join(css)}</style>{defs}{''.join(out)}</svg>\n"
    )


def main() -> None:
    out_dir = Path(__file__).resolve().parent.parent / "docs" / "assets"
    out_dir.mkdir(parents=True, exist_ok=True)
    for theme in THEMES:
        (out_dir / f"space-{theme}.svg").write_text(build(theme), encoding="utf-8")
        print(f"wrote docs/assets/space-{theme}.svg")


if __name__ == "__main__":
    main()
