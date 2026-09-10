#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["fonttools>=4", "uharfbuzz>=0.50"]
# ///
"""Render with local Berkeley Mono and rsvg-convert; no font redistribution."""
from __future__ import annotations

import argparse
import html
import io
import re
import subprocess
import textwrap
from pathlib import Path

import uharfbuzz as hb
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parents[1]
WIDTH = 1600
HEIGHT = 1100
BACKGROUND = "#101114"
FOREGROUND = "#d5d7dc"
ACCENT = "#d58c68"
FEATURES = {"liga": True, "calt": True}


class Typeface:
    def __init__(self, path: Path, label: str) -> None:
        self.data = path.read_bytes()
        self.face = hb.Face(self.data)
        self.shaper = hb.Font(self.face)
        self.font = TTFont(io.BytesIO(self.data))
        self.glyphs = self.font.getGlyphSet()
        self.label = label
        self.outlines: dict[str, str] = {}

    def shape(self, text: str, features: dict[str, bool] = FEATURES) -> hb.Buffer:
        buffer = hb.Buffer()
        buffer.add_str(text)
        buffer.guess_segment_properties()
        hb.shape(self.shaper, buffer, features)
        return buffer

    def outline(self, glyph: int) -> str:
        identity = f"{self.label}-{glyph}"
        if identity not in self.outlines:
            pen = SVGPathPen(self.glyphs)
            self.glyphs[self.font.getGlyphName(glyph)].draw(pen)
            self.outlines[identity] = pen.getCommands()
        return identity


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--font-dir", type=Path, default=Path.home() / "Library/Fonts")
    args = parser.parse_args()
    regular = Typeface(args.font_dir / "BerkeleyMono-Regular.otf", "regular")
    bold = Typeface(args.font_dir / "BerkeleyMono-Bold.otf", "bold")
    ligature_text = "-> => != == ::"
    enabled = regular.shape(ligature_text)
    disabled = regular.shape(ligature_text, {"liga": False, "calt": False})
    assert [g.codepoint for g in enabled.glyph_infos] != [g.codepoint for g in disabled.glyph_infos], "Font has no active programming ligatures"

    source = (ROOT / "docs/social/fork-join-example.rs").read_text()
    end = source.index('    }).expect("scope failed")') + len('    }).expect("scope failed")')
    excerpt = textwrap.dedent(source[source.index("    loom::scope"):end]).rstrip()
    elements: list[str] = []

    def write(text: str, x: float, baseline: float, size: int, color: str, face: Typeface = regular) -> float:
        shaped = face.shape(text)
        factor = size / face.face.upem
        cursor = x
        for glyph, position in zip(shaped.glyph_infos, shaped.glyph_positions, strict=True):
            identity = face.outline(glyph.codepoint)
            tx = cursor + position.x_offset * factor
            ty = baseline - position.y_offset * factor
            elements.append(f'<use href="#{identity}" fill="{color}" transform="translate({tx:.4f} {ty:.4f}) scale({factor:.6f} {-factor:.6f})"/>')
            cursor += position.x_advance * factor
        assert cursor <= WIDTH - 80, f"Text overflow: {text}"
        return cursor

    write("grep, without function coloring.", 96, 190, 45, "#f1f0ed", bold)

    tokens = re.compile(r'("[^"\n]*"|\b(?:let|mut|for|in|if|move)\b|\b(?:fork|join|read|contains)(?=\())')
    for index, line in enumerate(excerpt.splitlines()):
        x = 96.0
        for token in tokens.split(line):
            if not token:
                continue
            color = FOREGROUND
            if token.startswith('"'):
                color = "#9cab9f"
            elif token in {"fork", "join", "read", "contains"}:
                color = ACCENT
            elif token in {"let", "mut", "for", "in", "if", "move"}:
                color = "#aeb6c7"
            x = write(token, x, 320 + index * 42, 30, color)

    definitions = []
    for face in [regular, bold]:
        for identity, path in face.outlines.items():
            definitions.append(f'<path id="{identity}" d="{html.escape(path, quote=True)}"/>')
    svg = '\n'.join([
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}" role="img">',
        '<title>Loom: recursive text search without function coloring</title>',
        '<desc>Berkeley Mono with liga and calt shaping. Code excerpt from the runnable Rust example.</desc>',
        '<metadata>' + html.escape(excerpt) + '</metadata>',
        '<defs>' + ''.join(definitions) + '</defs>',
        f'<rect width="{WIDTH}" height="{HEIGHT}" fill="{BACKGROUND}"/>',
        *elements,
        '</svg>',
    ])
    output = ROOT / "docs/assets/loom-grep-dark"
    output.with_suffix(".svg").write_text(svg)
    subprocess.run(["rsvg-convert", "--output", str(output.with_suffix(".png")), str(output.with_suffix(".svg"))], check=True)
    print(f"Rendered {output.name}.svg and .png; Berkeley Mono contextual ligatures verified.")


if __name__ == "__main__":
    main()
