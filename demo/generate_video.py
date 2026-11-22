#!/usr/bin/env python3
"""Render the deterministic 58-second WalkCache fallback from reviewed evidence excerpts."""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont
import subprocess

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "demo"
ASSETS = OUT / "assets"
ASSETS.mkdir(parents=True, exist_ok=True)
W, H = 1920, 1080
BG, PANEL = "#0d1418", "#152026"
TEXT, MUTED, MINT, AMBER, RED = "#eff6f2", "#9fb0aa", "#82dab3", "#f4c877", "#ff9f9f"
FONT = "/System/Library/Fonts/SFNS.ttf"
MONO = "/System/Library/Fonts/Menlo.ttc"
DURATIONS = [5, 8, 7, 8, 10, 7, 6, 7]


def font(size, mono=False):
    return ImageFont.truetype(MONO if mono else FONT, size)


def text(draw, xy, value, size=34, color=TEXT, mono=False):
    draw.text(xy, value, font=font(size, mono), fill=color)


def wrap(draw, value, width, size=34, mono=False):
    lines, current = [], ""
    for word in value.split():
        trial = f"{current} {word}".strip()
        if draw.textlength(trial, font=font(size, mono)) <= width:
            current = trial
        else:
            if current:
                lines.append(current)
            current = word
    if current:
        lines.append(current)
    return lines


def paragraph(draw, xy, value, width, size=34, color=TEXT, mono=False, spacing=16):
    x, y = xy
    for line in wrap(draw, value, width, size, mono):
        text(draw, (x, y), line, size, color, mono)
        y += size + spacing
    return y


def pill(draw, x, y, label, color=MINT):
    draw.rounded_rectangle((x, y, x + draw.textlength(label, font=font(24, True)) + 34, y + 48), 12, fill=color)
    text(draw, (x + 17, y + 10), label, 24, BG, True)


def terminal(draw, box, title, lines):
    x1, y1, x2, y2 = box
    draw.rounded_rectangle(box, 18, fill="#080f13", outline="#30463e", width=2)
    draw.rounded_rectangle((x1, y1, x2, y1 + 62), 18, fill="#1b2a29")
    text(draw, (x1 + 24, y1 + 16), title, 25, MINT, True)
    y = y1 + 87
    for value, color in lines:
        for line in wrap(draw, value, x2 - x1 - 48, 29, True):
            text(draw, (x1 + 24, y), line, 29, color, True)
            y += 42
        y += 9


def base(index, kicker, title_value, subtitle):
    im = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(im)
    d.rectangle((0, 0, W, 12), fill=MINT)
    for n in range(8):
        d.rectangle((56 + n * 226, 34, 56 + n * 226 + 202, 42), fill=MINT if n < index else "#31423b")
    text(d, (64, 76), f"0{index} / 08   {kicker}", 25, MINT, True)
    text(d, (64, 139), title_value, 72)
    paragraph(d, (66, 247), subtitle, 1740, 34, MUTED)
    d.line((64, 960, 1856, 960), fill="#31423b", width=2)
    text(d, (64, 986), "Committed real-run excerpts · Durations are not a cross-commit speedup claim", 23, MUTED, True)
    text(d, (1582, 986), f"{sum(DURATIONS[:index-1]):02d}s / 58s", 23, MUTED, True)
    return im, d


def save(im, index):
    path = ASSETS / f"slide-{index:02d}.png"
    im.save(path)
    return path


slides = []
# 01
im, d = base(1, "WALKCACHE", "Explore. Verify. Cache. Replay.", "A local learning layer for repeatable Maestro QA journeys.")
text(d, (66, 405), "Learn the walk once.", 54, TEXT)
text(d, (66, 485), "Replay without a model.", 54, MINT)
text(d, (66, 565), "Repair only bounded staleness.", 54, TEXT)
pill(d, 66, 696, "ANDROID + VERIFIED CHROMIUM")
text(d, (66, 784), "OpenClaw / Kimi × Maestro", 34, MUTED)
slides.append(save(im, 1))

# 02
im, d = base(2, "COLD EXPLORATION", "The agent discovers the journey.", "OpenClaw/Kimi explores through Maestro. WalkCache independently runs the complete candidate before storing it.")
terminal(d, (66, 342, 1818, 890), "walk008-cold-miss.transcript.txt", [
    ("$ walkcache run checkout", MUTED),
    ("CACHE  MISS · not-found", AMBER),
    ("13 model round trips", TEXT),
    ("11 meaningful assertions", TEXT),
    ("PASS · cold-recorded · 95.499 s", MINT),
    ("YAML saved only after independent verification", MINT),
])
slides.append(save(im, 2))

# 03
im, d = base(3, "VERIFIED YAML", "The memory is ordinary Maestro YAML.", "Stable selectors and product assertions stay executable without WalkCache or an LLM. Secrets and raw attempt artifacts stay ignored.")
yaml_lines = [
    "# excerpt: repaired-checkout.yaml",
    "- tapOn:",
    "    id: dev.walkcache.checkout:id/add_to_cart",
    '- assertVisible: "Total: $5.00"',
    "- tapOn:",
    "    id: dev.walkcache.checkout:id/checkout_button",
    "- assertVisible: Order confirmed",
    '- assertVisible: "Order #WC-1001 · Mochi sticker pack"',
]
terminal(d, (66, 342, 1818, 890), "12 ASSERTIONS · SELECTORS + OUTCOME", [(line, MINT if line.startswith("- assert") else TEXT) for line in yaml_lines])
slides.append(save(im, 3))

# 04
im, d = base(4, "WARM REPLAY", "Cache HIT. Zero model calls.", "The exact stored walk goes directly to Maestro. Neither the explorer nor a model client is constructed.")
terminal(d, (66, 350, 1020, 873), "walk012-readme-review.txt", [
    ("CACHE  HIT", MINT),
    ("PASS · 12 assertions", TEXT),
    ("MODEL CALLS  0", MINT),
    ("29.023 s · this captured run", TEXT),
    ("Same meaningful order assertion", MUTED),
])
d.rounded_rectangle((1080, 350, 1818, 873), 18, fill=PANEL, outline="#30463e", width=2)
pill(d, 1130, 404, "EXACT LOCAL HIT")
text(d, (1130, 506), "goal + app", 36, TEXT, True)
text(d, (1130, 562), "build + flow", 36, TEXT, True)
d.line((1130, 635, 1730, 635), fill="#52685d", width=3)
text(d, (1130, 690), "MAESTRO", 43, MINT, True)
text(d, (1130, 756), "12 checks passed", 30, TEXT, True)
text(d, (1130, 813), "NO MODEL CLIENT", 25, MUTED, True)
slides.append(save(im, 4))

# 05
im, d = base(5, "BOUNDED REPAIR", "Repair the selector—not the assertion.", "A stale build diagnoses one recognized checkout selector failure, proposes only the configured Java/XML patch, rebuilds, and verifies the unchanged full journey.")
terminal(d, (66, 343, 1818, 890), "walk008-stale-repair.transcript.txt · PROPOSED DIFF", [
    ("- Button checkout = button(\"Checkout\", null);", RED),
    ("+ Button checkout = button(\"Checkout\", checkoutId);", MINT),
    ("- <!-- checkout_button deliberately absent -->", RED),
    ("+ <item name=\"checkout_button\" type=\"id\" />", MINT),
    ("2 configured files · 1 repair · 0 model calls", TEXT),
    ("PASS · 11 assertions · 65.865 s", MINT),
    ("autoCommit: false · no PR · no hidden repair", MINT),
])
slides.append(save(im, 5))

# 06
im, d = base(6, "REGRESSION GUARD", "A real failure stays a failure.", "Meaningful product assertion failures are never rewritten or repaired into green. Unknown failures do not trigger the repair policy either.")
pill(d, 66, 365, "PRODUCT_REGRESSION", RED)
terminal(d, (66, 442, 1818, 890), "walk008-product-regression.transcript.txt", [
    ("FAIL · exact Order #WC-1001 assertion", RED),
    ("REPAIR ATTEMPTS  0", MINT),
    ("MODEL CALLS  0", MINT),
    ("Exit 1 · 41.876 s · source diffs: none", TEXT),
])
slides.append(save(im, 6))

# 07
im, d = base(7, "VERIFIED CHROMIUM", "The same zero-model cache contract.", "Maestro Web 2.12.0 Beta/Experimental independently verifies reviewed YAML and replays it in real Chromium.")
terminal(d, (66, 342, 1818, 890), "walk010-warm.txt + walk010-cold-unsupported.txt", [
    ("CACHE HIT · 12 assertions · 0 model calls", MINT),
    ("PASS · 42.634 s · this captured run", TEXT),
    ("Browser/build/adapter identity is cached", TEXT),
    ("Web cold exploration: UNSUPPORTED", AMBER),
    ("Web repair: UNSUPPORTED · no Android fallback", AMBER),
])
slides.append(save(im, 7))

# 08
im, d = base(8, "ARCHITECTURE", "Verified memory. Honest boundaries.", "Exact goal, app, platform, flow schema, build and flow identity—plus adapter/browser identity for Web.")
items = [
    ("VERIFY BEFORE PERSISTENCE", MINT),
    ("ATOMIC LOCAL CACHE + VALIDATION HISTORY", TEXT),
    ("BOUNDED, VISIBLE REPAIR—NEVER AUTO-COMMIT", TEXT),
    ("NO iOS CLAIM · NO GENERAL REPAIR CLAIM", AMBER),
    ("NO INSTANT-REPLAY CLAIM", AMBER),
]
y = 355
for label, color in items:
    d.rounded_rectangle((66, y, 1818, y + 74), 12, fill=PANEL)
    text(d, (94, y + 19), label, 29, color, True)
    y += 91
text(d, (66, 858), "github.com/seankim-android/kimi-hackathon  ·  Apache-2.0", 32, MINT, True)
slides.append(save(im, 8))

# Explicit per-input trims avoid image-concat demuxer padding (the rejected first take was 65 s).
command = ["ffmpeg", "-y", "-v", "error"]
for path in slides:
    command += ["-loop", "1", "-framerate", "30", "-i", str(path)]
filters = []
labels = []
for index, duration in enumerate(DURATIONS):
    label = f"v{index}"
    labels.append(f"[{label}]")
    filters.append(f"[{index}:v]trim=duration={duration},fps=30,setpts=PTS-STARTPTS[{label}]")
filters.append(f"{''.join(labels)}concat=n={len(slides)}:v=1:a=0,format=yuv420p[video]")
command += [
    "-filter_complex", ";".join(filters), "-map", "[video]", "-t", str(sum(DURATIONS)),
    "-c:v", "libx264", "-preset", "medium", "-crf", "18",
    "-movflags", "+faststart", "-metadata", "title=WalkCache — Evidence-backed demo",
    "-metadata", "comment=58-second deterministic fallback from committed real-run evidence; no instant-replay claim.",
    str(OUT / "walkcache-demo.mp4"),
]
subprocess.run(command, check=True)
print(f"Rendered {OUT / 'walkcache-demo.mp4'}: {sum(DURATIONS)} seconds, 1920x1080, 30 fps")
