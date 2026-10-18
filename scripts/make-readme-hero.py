"""Generates the animated README hero (docs/assets/hero-dark.svg, hero-light.svg).

The animation is plain CSS inside the SVG (GitHub renders it in <img>; scripts are stripped):
a query is typed, results slide in (some from offline devices), the status line appears, then it loops.
Run: python scripts/make-readme-hero.py
"""

from pathlib import Path
from xml.sax.saxutils import escape

W, H = 960, 500
CYCLE = 9.0  # seconds per loop
QUERY = "report ext:pdf"
MATCH = "report"
TYPE_START, TYPE_STEP = 0.6, 0.11  # first key at 0.6 s, then one key every 110 ms
ROWS_START, ROW_STEP = TYPE_START + len(QUERY) * TYPE_STEP + 0.35, 0.12
FADE_OUT = 8.3  # everything fades out here, before the loop restarts

ROWS = [
    # name, folder, size, modified, device, online
    ("Annual report 2025.pdf", r"D:\Work\Finance", "2.4 MB", "Mar 3, 2026", "Data (D:)", True),
    ("Thesis report final.pdf", r"E:\Uni\Thesis\Drafts", "5.1 MB", "Jun 12, 2025", "Backup 2TB", False),
    ("report-q1.pdf", r"\\nas\media\docs\2026", "880 KB", "Jan 20, 2026", "NAS media", True),
    ("Lab report 3.pdf", r"C:\Users\Calin\Documents", "312 KB", "Nov 4, 2025", "System (C:)", True),
    ("Scanned report.pdf", r"F:\DCIM\Scans", "1.1 MB", "Aug 9, 2024", "Camera SD", False),
]

THEMES = {
    "dark": dict(
        bg="#202020", window="#2b2b2b", border="#ffffff1a", text="#ffffff", text2="#e0e0e0",
        muted="#a0a0a0", accent="#60a5fa", accent_text="#7ab4ff", accent_soft="#60a5fa26",
        offline="#6b7280", online="#22c55e", field="#ffffff0f", select="#60a5fa1f", shadow="#00000080",
    ),
    "light": dict(
        bg="#f3f3f3", window="#ffffff", border="#0000001a", text="#1a1a1a", text2="#3d3d3d",
        muted="#5f5f5f", accent="#3b82f6", accent_text="#1d5fd6", accent_soft="#3b82f61a",
        offline="#9ca3af", online="#16a34a", field="#0000000a", select="#3b82f614", shadow="#00000026",
    ),
}

FONT = "'Segoe UI Variable Text','Segoe UI',system-ui,-apple-system,'Helvetica Neue',Arial,sans-serif"
MONO = "'Cascadia Mono','Cascadia Code',Consolas,'SFMono-Regular',Menlo,monospace"


def pct(t: float) -> str:
    return f"{max(0.0, min(100.0, t / CYCLE * 100)):.2f}%"


def appear(name: str, at: float, rise: float = 0.0) -> str:
    """Keyframes: hidden until `at`, visible (optionally sliding up by `rise` px) until FADE_OUT."""
    a0, a1, f0, f1 = pct(at), pct(at + 0.25), pct(FADE_OUT), pct(FADE_OUT + 0.35)
    tr0 = f"transform:translateY({rise}px);" if rise else ""
    tr1 = "transform:translateY(0);" if rise else ""
    return (
        f"@keyframes {name}{{0%,{a0}{{opacity:0;{tr0}}}{a1},{f0}{{opacity:1;{tr1}}}{f1},100%{{opacity:0;{tr1}}}}}"
        f".{name}{{animation:{name} {CYCLE}s linear infinite;opacity:0}}"
    )


def highlight(name: str, c: dict) -> str:
    """Name with the first case-insensitive match of MATCH in the accent colour."""
    i = name.lower().find(MATCH)
    if i < 0:
        return escape(name)
    a, m, b = name[:i], name[i : i + len(MATCH)], name[i + len(MATCH) :]
    return f"{escape(a)}<tspan fill=\"{c['accent_text']}\" font-weight=\"600\">{escape(m)}</tspan>{escape(b)}"


def file_icon(x: float, y: float, color: str) -> str:
    return (
        f'<g transform="translate({x},{y})" fill="none" stroke="{color}" stroke-width="1.4" stroke-linejoin="round">'
        '<path d="M2 1h7l4 4v11a1 1 0 0 1-1 1H2a1 1 0 0 1-1-1V2a1 1 0 0 1 1-1z"/><path d="M9 1v4h4"/>'
        '<path d="M4 10h6M4 13h4" stroke-linecap="round"/></g>'
    )


def build(theme: str) -> str:
    c = THEMES[theme]
    css = [
        f"text{{font-family:{FONT}}}",
        f".mono{{font-family:{MONO}}}",
        "@keyframes blink{0%,49%{opacity:1}50%,100%{opacity:0}}",
        ".caret{animation:blink 1s step-end infinite}",
    ]
    out = []
    # window
    out.append(f'<rect x="0" y="0" width="{W}" height="{H}" rx="16" fill="{c["bg"]}"/>')
    out.append(
        f'<rect x="24" y="24" width="{W - 48}" height="{H - 48}" rx="12" fill="{c["window"]}" '
        f'stroke="{c["border"]}" filter="url(#shadow)"/>'
    )
    # title bar
    out.append(
        f'<g transform="translate(44,44)"><rect width="22" height="22" rx="6" fill="{c["accent"]}"/>'
        f'<circle cx="10" cy="10" r="5" fill="none" stroke="{c["window"]}" stroke-width="2.2"/>'
        f'<path d="M14 14l4 4" stroke="{c["window"]}" stroke-width="2.4" stroke-linecap="round"/></g>'
    )
    out.append(f'<text x="76" y="61" font-size="15" font-weight="600" fill="{c["text"]}">Seek</text>')
    for i, d in enumerate(["M0 0h12", "M0 -6h12v12h-12z", "M0 -6l12 12M12 -6l-12 12"]):
        out.append(
            f'<path transform="translate({W - 150 + i * 36},56)" d="{d}" fill="none" '
            f'stroke="{c["muted"]}" stroke-width="1.3"/>'
        )
    # search field
    fx, fy, fw, fh = 44, 84, W - 88, 52
    out.append(f'<rect x="{fx}" y="{fy}" width="{fw}" height="{fh}" rx="10" fill="{c["field"]}" stroke="{c["border"]}"/>')
    out.append(
        f'<g transform="translate({fx + 20},{fy + 16})" fill="none" stroke="{c["muted"]}" stroke-width="2">'
        '<circle cx="9" cy="9" r="7"/><path d="M14.5 14.5L20 20" stroke-linecap="round"/></g>'
    )
    # typed query: one glyph per key, placed on a fixed grid so the caret can follow it exactly
    qx, qy, cw = fx + 58, fy + 33, 12.1
    for i, ch in enumerate(QUERY):
        cls = f"k{i}"
        css.append(appear(cls, TYPE_START + i * TYPE_STEP))
        color = c["accent_text"] if i < len(MATCH) else c["text"]
        out.append(
            f'<text class="mono {cls}" x="{qx + i * cw:.1f}" y="{qy}" font-size="20" fill="{color}">{escape(ch)}</text>'
        )
    steps = len(QUERY)
    t0, t1 = pct(TYPE_START), pct(TYPE_START + steps * TYPE_STEP)
    css.append(
        f"@keyframes caretmove{{0%,{t0}{{transform:translateX(0)}}{t1},100%{{transform:translateX({steps * cw:.1f}px)}}}}"
        f".caretpos{{animation:caretmove {CYCLE}s steps({steps},end) infinite}}"
    )
    out.append(
        f'<g class="caretpos"><rect class="caret" x="{qx - 1}" y="{fy + 15}" width="2" height="23" '
        f'rx="1" fill="{c["accent"]}"/></g>'
    )
    # column headers
    hy = 170
    cols = [("Name", 76), ("Folder", 330), ("Size", 548), ("Modified", 628), ("Device", 740)]
    for label, x in cols:
        anchor = ' text-anchor="end"' if label == "Size" else ""
        lx = x + 52 if label == "Size" else x
        out.append(f'<text x="{lx}" y="{hy}" font-size="13" font-weight="600" fill="{c["muted"]}"{anchor}>{label}</text>')
    out.append(f'<path d="M44 {hy + 12}H{W - 44}" stroke="{c["border"]}"/>')
    # rows
    for i, (name, folder, size, modified, device, online) in enumerate(ROWS):
        y = hy + 26 + i * 44
        cls = f"r{i}"
        css.append(appear(cls, ROWS_START + i * ROW_STEP, rise=8))
        ink = c["text"] if online else c["offline"]
        ink2 = c["text2"] if online else c["offline"]
        g = [f'<g class="{cls}">']
        if i == 0:
            g.append(f'<rect x="44" y="{y}" width="{W - 88}" height="40" rx="6" fill="{c["select"]}"/>')
        g.append(file_icon(54, y + 12, c["accent"] if online else c["offline"]))
        g.append(f'<text x="76" y="{y + 25}" font-size="14.5" fill="{ink}">{highlight(name, c) if online else escape(name)}</text>')
        g.append(f'<text x="330" y="{y + 25}" font-size="13.5" fill="{ink2}">{escape(folder)}</text>')
        g.append(f'<text x="600" y="{y + 25}" font-size="13.5" fill="{ink2}" text-anchor="end">{size}</text>')
        g.append(f'<text x="628" y="{y + 25}" font-size="13.5" fill="{ink2}">{modified}</text>')
        dot = (
            f'<circle cx="746" cy="{y + 20}" r="4" fill="{c["online"]}"/>'
            if online
            else f'<circle cx="746" cy="{y + 20}" r="3.5" fill="none" stroke="{c["offline"]}" stroke-width="1.5"/>'
        )
        g.append(dot)
        g.append(f'<text x="758" y="{y + 25}" font-size="13.5" fill="{ink}">{escape(device)}</text>')
        if not online:
            g.append(
                f'<rect x="{758 + len(device) * 7.4 + 8:.0f}" y="{y + 10}" width="48" height="20" rx="10" '
                f'fill="none" stroke="{c["offline"]}"/>'
                f'<text x="{758 + len(device) * 7.4 + 32:.0f}" y="{y + 24}" font-size="11" fill="{c["offline"]}" '
                f'text-anchor="middle">offline</text>'
            )
        g.append("</g>")
        out.append("".join(g))
    # status line
    sy = H - 52
    status_at = ROWS_START + len(ROWS) * ROW_STEP + 0.15
    css.append(appear("st", status_at))
    out.append(
        f'<g class="st"><text x="48" y="{sy}" font-size="13.5" fill="{c["text2"]}">'
        f'<tspan font-weight="600" fill="{c["text"]}">5 results</tspan> in 1.7 ms</text>'
        f'<circle cx="190" cy="{sy - 4.5}" r="4" fill="{c["online"]}"/>'
        f'<text x="200" y="{sy}" font-size="13.5" fill="{c["text2"]}">3 online</text>'
        f'<circle cx="276" cy="{sy - 4.5}" r="3.5" fill="none" stroke="{c["offline"]}" stroke-width="1.5"/>'
        f'<text x="286" y="{sy}" font-size="13.5" fill="{c["text2"]}">2 offline — still searchable</text></g>'
    )
    defs = (
        f'<defs><filter id="shadow" x="-5%" y="-5%" width="110%" height="115%">'
        f'<feDropShadow dx="0" dy="6" stdDeviation="10" flood-color="{c["shadow"]}"/></filter></defs>'
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
        f'role="img" aria-label="Seek: typing a query shows matching files from online and offline drives instantly">'
        f"<style>{''.join(css)}</style>{defs}{''.join(out)}</svg>\n"
    )


def main() -> None:
    out_dir = Path(__file__).resolve().parent.parent / "docs" / "assets"
    out_dir.mkdir(parents=True, exist_ok=True)
    for theme in THEMES:
        (out_dir / f"hero-{theme}.svg").write_text(build(theme), encoding="utf-8")
        print(f"wrote docs/assets/hero-{theme}.svg")


if __name__ == "__main__":
    main()
