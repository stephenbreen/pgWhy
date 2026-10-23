#!/usr/bin/env python3
"""Cut the promo from the motion capture `e2e/specs/promo.e2e.ts` records.

The capture is real app motion: the spec performs each interaction a step at a time and grabs a
frame per step, with a timestamp per frame (`manifest.json`). This script replays those frames on
their real timing, retimes each beat to its slot in a 30-second reel, holds the last frame of the
short beats, lays captions on top, and joins the beats with hard cuts between two cards.

    python3 scripts/promo.py <capture-dir> <out.mp4>

No zoompan anywhere: the motion is the app's own. Captions are rendered with Pillow (this ffmpeg
has no drawtext) and overlaid per beat. Requires ffmpeg and Pillow.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

W, H, FPS = 1920, 1080, 30
BOLD = "/System/Library/Fonts/Supplemental/Arial Bold.ttf"
REG = "/System/Library/Fonts/Supplemental/Arial.ttf"
BG_TOP, BG_BOTTOM = (22, 28, 36), (9, 12, 16)
ACCENT = (47, 111, 223)
CARD = 2.6  # seconds per title/closing card

# The reel: cards and beats in order. `target` is the slot; the capture is sped up or slowed down
# to fit, and the last frame is held for `hold`. `drop` trims from the front (the 1M-row result
# spends its first seconds streaming, which reads as a frozen screen).
REEL: list[dict] = [
    {"card": "pgWhy", "sub": "A Postgres workbench for people who came from SSMS"},
    {"beat": "tree", "caption": "Every object, its columns and types — one click", "target": 2.2, "hold": 0.5},
    {"beat": "query", "caption": "Write it, run it, keep the types", "target": 3.4, "hold": 0.3},
    {"beat": "million-scroll", "caption": "1,000,000 rows, streamed and windowed", "target": 5.0, "hold": 0.5, "interp": True},
    {"beat": "edit", "caption": "Edit in place — pending changes marked", "target": 3.0, "hold": 0.4},
    {"beat": "commit", "caption": "One transaction, every row count verified", "target": 2.0, "hold": 1.0},
    {"beat": "guardrail", "caption": "Destructive actions: env tag, typed name, server lock", "target": 3.0, "hold": 0.8},
    {"beat": "confirm", "caption": "Refresh local from remote: one action, one confirmation", "target": 3.2, "hold": 1.2},
    {"beat": "history", "caption": "Backups, restores and runs in one history", "target": 3.0, "hold": 0.8},
    {"card": "pgWhy v1.1", "sub": "Postgres verbs, SSMS density, one safety model"},
]


def run(cmd: list[str]) -> None:
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL)


def caption_png(text: str, out: Path) -> None:
    """White caption on a translucent rounded panel, sized to the text, readable over any UI."""
    font = ImageFont.truetype(BOLD, 44)
    probe = ImageDraw.Draw(Image.new("RGBA", (1, 1)))
    l, t, r, b = probe.textbbox((0, 0), text, font=font)
    pad = 24
    img = Image.new("RGBA", (r - l + 2 * pad, b - t + 2 * pad), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rounded_rectangle(
        (0, 0, img.width - 1, img.height - 1), radius=14, fill=(11, 15, 20, 200)
    )
    draw.text((pad - l, pad - t), text, font=font, fill=(255, 255, 255, 255))
    img.save(out)


def card_png(title: str, sub: str, out: Path) -> None:
    """Title card: soft gradient, accent glow, rule, title, subtitle."""
    ramp = Image.linear_gradient("L").resize((W, H))
    img = Image.composite(Image.new("RGB", (W, H), BG_TOP), Image.new("RGB", (W, H), BG_BOTTOM), ramp)
    glow = Image.radial_gradient("L").resize((W, H))
    tint = Image.new("RGB", (W, H), ACCENT)
    img = Image.composite(tint, img, glow.point(lambda v: int(v * 0.22)))
    img = img.filter(ImageFilter.GaussianBlur(48))
    draw = ImageDraw.Draw(img)
    title_font = ImageFont.truetype(BOLD, 92)
    sub_font = ImageFont.truetype(REG, 38)
    tl, tt, tr, tb = draw.textbbox((0, 0), title, font=title_font)
    sl, st, sr, sb = draw.textbbox((0, 0), sub, font=sub_font)
    block = 4 + 46 + (tb - tt) + 28 + (sb - st)
    y = (H - block) / 2
    draw.rectangle(((W - 96) / 2, y, (W + 96) / 2, y + 4), fill=ACCENT)
    y += 4 + 46
    draw.text(((W - (tr - tl)) / 2 - tl, y - tt), title, font=title_font, fill=(255, 255, 255))
    y += (tb - tt) + 28
    draw.text(((W - (sr - sl)) / 2 - sl, y - st), sub, font=sub_font, fill=(176, 186, 200))
    img.save(out)


def beat_clip(frames_dir: Path, offsets: list[int], slot: dict, out: Path, tmp: Path) -> None:
    """Replay one beat on its real timing, retimed to `target`, caption overlaid, last frame held."""
    drop = slot.get("drop", 0)
    keep = list(zip(sorted(frames_dir.iterdir()), offsets))[drop:]
    if not keep:
        raise SystemExit(f"beat {frames_dir.name}: no frames after trimming")

    target = slot["target"]
    hold = slot["hold"]
    motion = max(0.2, target - hold)

    # per-frame durations from the recorded timeline; a lone frame gets a nominal duration
    gaps = [max(1, b[1] - a[1]) for a, b in zip(keep, keep[1:])]
    if not gaps:
        gaps = [100]
    raw_total = sum(gaps) / 1000.0
    scale = motion / raw_total if raw_total > 0 else 1.0

    lines: list[str] = []
    for i, (path, _) in enumerate(keep):
        dur = (gaps[i] if i < len(gaps) else gaps[-1]) / 1000.0 * scale
        if i == len(keep) - 1:
            dur += hold
        lines.append(f"file '{path.resolve()}'\nduration {dur:.4f}")
    # the concat demuxer wants the final file repeated after its duration
    lines.append(f"file '{keep[-1][0].resolve()}'")
    listing = tmp / f"{frames_dir.name}.txt"
    listing.write_text("\n".join(lines) + "\n")

    caption = tmp / f"{frames_dir.name}-caption.png"
    caption_png(slot["caption"], caption)
    # The capture is ~10 fps. Where the UI *translates* (a scroll), interpolate the missing frames
    # from real motion — two of every three frames would otherwise repeat. Where it changes
    # discretely (panels opening, text appearing) interpolation only smears glyphs, so those
    # beats are resampled by duplication instead.
    if slot.get("interp"):
        motion_filter = (
            f"minterpolate=fps={FPS}:mi_mode=mci:mc_mode=aobmc:vsbmc=1:scd=fdiff:scd_threshold=10"
        )
    else:
        motion_filter = f"fps={FPS}"
    run(
        [
            "ffmpeg", "-y",
            "-f", "concat", "-safe", "0", "-i", str(listing),
            "-loop", "1", "-i", str(caption),
            "-filter_complex",
            # The capture is ~10 fps: interpolate the missing frames from real motion (the UI
            # slides, it does not dissolve), and let scene-change detection leave hard cuts
            # alone instead of smearing them. Without this, two of every three frames repeat.
            f"[0:v]scale={W}:{H}:flags=lanczos,setsar=1,{motion_filter}[ui];"
            f"[1:v]format=rgba[cap];"
            f"[ui][cap]overlay=56:{H}-190[out]",
            "-map", "[out]", "-t", f"{target:.3f}",
            "-r", str(FPS), "-c:v", "libx264", "-preset", "medium", "-crf", "18",
            "-pix_fmt", "yuv420p", str(out),
        ]
    )


def card_clip(slot: dict, out: Path, tmp: Path, fade: str) -> None:
    card = tmp / f"{abs(hash(slot['card']))}.png"
    card_png(slot["card"], slot["sub"], card)
    vf = f"fade={fade},format=yuv420p" if fade else "format=yuv420p"
    run(
        [
            "ffmpeg", "-y", "-loop", "1", "-framerate", str(FPS), "-t", f"{CARD:.2f}",
            "-i", str(card), "-vf", f"scale={W}:{H},{vf}", "-r", str(FPS),
            "-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p",
            str(out),
        ]
    )


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    capture, out = Path(sys.argv[1]), Path(sys.argv[2])
    manifest = json.loads((capture / "manifest.json").read_text())
    offsets = {b["name"]: b["offsets"] for b in manifest["beats"]}

    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        clips: list[Path] = []
        for i, slot in enumerate(REEL):
            clip = tmp / f"{i:02d}.mp4"
            if "card" in slot:
                fade = f"in:st=0:d=0.4" if i == 0 else f"out:st={CARD - 0.5:.2f}:d=0.5"
                card_clip(slot, clip, tmp, fade)
            else:
                beat = slot["beat"]
                if beat not in offsets:
                    print(f"missing beat {beat} in {capture}", file=sys.stderr)
                    return 1
                beat_clip(capture / beat, offsets[beat], slot, clip, tmp)
            clips.append(clip)

        listing = tmp / "reel.txt"
        listing.write_text("\n".join(f"file '{c.resolve()}'" for c in clips) + "\n")
        out.parent.mkdir(parents=True, exist_ok=True)
        run(
            [
                "ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(listing),
                "-c", "copy", "-movflags", "+faststart", str(out),
            ]
        )

    probe = subprocess.run(
        [
            "ffprobe", "-v", "error", "-show_entries",
            "format=duration,size:stream=width,height,r_frame_rate,codec_name,nb_frames",
            "-of", "default=noprint_wrappers=1", str(out),
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    print(probe.stdout.strip())
    return 0


if __name__ == "__main__":
    sys.exit(main())
