"""Build the size-tuned versions of the copse logo from the master design.

The master (copse-leaf-original.svg) is drawn on a 1064-unit canvas with 30-unit
lines: right at large sizes, but under a pixel wide at 32 px. So each size gets its
own version, as icon sets do: heavier lines, bigger dots, fewer details and a
tighter crop as it shrinks.

  master   96 px and up   the design as drawn
  medium   48-96 px       lines and dots heavier, outline slightly thicker
  small    24-32 px       main line and one branch, big dots, notch smoothed away
  tiny     16 px          leaf, midrib and three dots (the favicon)

Each also has a -dark version (a lighter green for dark backgrounds), and
favicon.svg switches between the two with the system's light or dark setting.
avatar.svg is the organization avatar: the master's light weights on white,
cropped tightly so the leaf fills the square. avatar-512.png is rendered from it
(on macOS: qlmanage -t -s 512 -o . avatar.svg).

Usage: python3 assets/logo/make-sizes.py [output folder]   (default: this folder)
"""
import os
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__))

LEAF = ("M 326,746 C 276,525 437,321 730,275 C 851,253 1004,258 1070,123 C 1110,330 1105,685 1001,820 "
        "C 906,956 752,1005 612,990 C 471,978 406,954 315,1017 C 265,1062 244,1116 238,1153 "
        "C 208,1056 288,932 481,818 L 505,851 C 407,918 352,963 315,1017 C 403,928 516,932 609,935 "
        "C 857,945 1025,850 1039,410 C 1007,472 946,514 893,541 C 1001,431 1032,297 1042,210 "
        "C 976,280 866,295 751,315 C 503,357 297,551 326,746 Z")
# Small sizes: drop the little notch inside the right edge (it turns into a blob).
LEAF_SMOOTH = LEAF.replace("C 1007,472 946,514 893,541 C 1001,431 1032,297 1042,210", "L 1042,210")

VEINS = {
    "central": "M 490,835 C 600,755 647,623 694,534 C 711,499 724,479 741,469 C 795,431 838,383 861,352",
    "left": "M 490,835 C 491,772 455,709 461,659 C 461,616 483,577 509,554 C 564,492 636,483 741,469",
    "right": "M 490,835 C 581,791 638,799 701,791 C 785,781 833,751 831,701 C 831,630 745,616 689,582",
}
NODES = {"root": (490, 835), "left": (509, 554), "middle": (642, 661), "junction": (741, 469),
         "tip": (861, 352), "right": (831, 701), "midrib": (690, 545)}

# name: (leaf path, extra outline weight, vein width, dot radius, veins, nodes, viewBox)
SIZES = {
    "master": (LEAF, 0, 30, 40, ["central", "left", "right"], [n for n in NODES if n != "midrib"], "128 104 1064 1064"),
    "medium": (LEAF, 18, 46, 52, ["central", "left", "right"], [n for n in NODES if n != "midrib"], "112 90 1092 1092"),
    "small": (LEAF_SMOOTH, 46, 70, 96, ["central", "left"], ["root", "left", "tip"], "100 66 1118 1118"),
    "tiny": (LEAF_SMOOTH, 64, 92, 112, ["central"], ["root", "midrib", "tip"], "90 56 1138 1138"),
}

def svg(name, light, dark=None):
    leaf, outline, vein_w, r, veins, nodes, vb = SIZES[name]
    color = "var(--c)" if dark else light
    style = ""
    if dark:
        # One file that follows the system's light/dark setting (browsers honour this in favicons).
        style = f"<style>svg{{color:{light}}}@media (prefers-color-scheme: dark){{svg{{color:{dark}}}}}</style>"
        color = "currentColor"
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}">', f"<title>copse</title>", style,
             f'<g fill="{color}" stroke="{color}" stroke-linejoin="round" stroke-linecap="round">',
             f'<path stroke-width="{outline}" d="{leaf}"/>', f'<g fill="none" stroke-width="{vein_w}">']
    parts += [f'<path d="{VEINS[v]}"/>' for v in veins]
    parts += ['</g><g stroke="none">']
    parts += [f'<circle cx="{NODES[n][0]}" cy="{NODES[n][1]}" r="{r}"/>' for n in nodes]
    parts += ["</g></g></svg>"]
    return "\n".join(p for p in parts if p) + "\n"

GREEN, GREEN_DARK = "#0C5C27", "#3DBA6E"
for name in SIZES:
    open(f"{OUT}/copse-leaf-{name}.svg", "w").write(svg(name, GREEN))
    open(f"{OUT}/copse-leaf-{name}-dark.svg", "w").write(svg(name, GREEN_DARK))
# The favicon: the small version, switching colour with the system's dark mode.
open(f"{OUT}/favicon.svg", "w").write(svg("tiny", GREEN, GREEN_DARK))
# The avatar: master weights, cropped tightly (the artwork spans x 208..1110 and
# y 123..1153, the stem tip), with a small margin, on white.
cx, cy, side = (208 + 1110) / 2, (123 + 1153) / 2, (1153 - 123) * 1.10
ax, ay = cx - side / 2, cy - side / 2
inner = svg("master", GREEN).split(">", 1)[1].rsplit("</svg>", 1)[0]
open(f"{OUT}/avatar.svg", "w").write(
    f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{ax:.0f} {ay:.0f} {side:.0f} {side:.0f}">'
    f'<rect x="{ax:.0f}" y="{ay:.0f}" width="{side:.0f}" height="{side:.0f}" fill="#ffffff"/>{inner}</svg>\n')
print("written")
