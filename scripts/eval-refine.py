#!/usr/bin/env python3
"""Compare post-meeting refine recognizers on AISHELL-4 sessions.

Both recognizers see exactly the same segments: the engine's `transcribe/file`
(Pyannote segmentation + Qwen3-ASR) runs first, then FireRedASR2-AED re-recognizes
each of those segments. So any CER difference comes from the recognizer alone.

A second, "oracle" mode cuts the audio at the reference speech regions instead,
so segmentation errors are removed and only the recognizers are compared.

Usage (needs mlx-audio, jiwer, cn2an, soundfile; e.g. `uv run --with mlx-audio
--with jiwer --with cn2an --with soundfile scripts/eval-refine.py ...`):
  eval-refine.py prep      <aishell4_test_dir> <out_dir> <session>...
  eval-refine.py qwen      <mm-meetingd> <out_dir> <session>...   # engine pipeline
  eval-refine.py fire      <out_dir> <session>...                 # FireRed on engine segments
  eval-refine.py oracle    <out_dir> {qwen|fire} <session>...     # reference segments
  eval-refine.py score     <out_dir> <session>...
  eval-refine.py breakdown <out_dir> <session>...                 # overlap / filler split
Results from 2026-11-07 are in docs/asr-model-survey.md section 7.5.
"""
import collections, json, re, subprocess, sys, time, unicodedata
from pathlib import Path

import jiwer
import soundfile as sf

SR = 16000


def parse_textgrid(path: Path):
    """Read every non-empty interval from a Praat TextGrid.

    Params: path - TextGrid file (AISHELL-4 uses one tier per speaker).
    Returns: list of (start_s, end_s, tier_name, text).
    """
    raw = path.read_bytes()
    for enc in ("utf-8", "utf-16", "gbk"):
        try:
            txt = raw.decode(enc)
            break
        except UnicodeDecodeError:
            continue
    else:
        raise ValueError(f"cannot decode {path}")
    out, tier = [], "?"
    xmin = xmax = None
    for line in txt.splitlines():
        line = line.strip()
        if line.startswith("name ="):
            tier = line.split("=", 1)[1].strip().strip('"')
        elif line.startswith("xmin ="):
            xmin = float(line.split("=")[1])
        elif line.startswith("xmax ="):
            xmax = float(line.split("=")[1])
        elif line.startswith("text ="):
            t = line.split("=", 1)[1].strip().strip('"').strip()
            if t and xmin is not None:
                out.append((xmin, xmax, tier, t))
    return out


def to_cn_digits(text: str) -> str:
    """Spell Arabic numerals as Chinese characters so ITN output scores fairly.

    Params: text - hypothesis text, possibly with digits like "2026年" or "12.5%".
    Returns: text with numbers written in Chinese characters.
    """
    if not re.search(r"\d", text):
        return text
    import cn2an
    # IDs like "001" are read digit by digit ("零零一"); cn2an would drop the zeros
    text = re.sub(r"(?<![\d.])0\d+", lambda m: "".join("零一二三四五六七八九"[int(c)] for c in m.group(0)), text)
    if not re.search(r"\d", text):
        return text
    try:
        return cn2an.transform(text, "an2cn")
    except Exception as e:  # cn2an rejects some mixed forms; fall back per digit run
        print(f"cn2an failed on {text[:40]!r}: {e}", file=sys.stderr)
        return re.sub(r"\d+", lambda m: cn2an.an2cn(m.group(0)), text)


def normalize(text: str, hyp: bool) -> str:
    """Strip markup, punctuation and spaces; lowercase Latin letters.

    Params: text - reference or hypothesis; hyp - True converts digits (references
    in AISHELL-4 already spell numbers in characters).
    Returns: the character string that CER is computed on.
    """
    text = re.sub(r"<[^>]*>", "", text)  # AISHELL-4 tags such as <sil>, <%>, <$>
    text = re.sub(r"\[[^\]]*\]", "", text)
    if hyp:
        text = to_cn_digits(text)
    text = unicodedata.normalize("NFKC", text).lower()
    return "".join(ch for ch in text if ch.isalnum())


def cmd_prep(src: Path, out: Path, sessions):
    """Write channel-0 16 kHz mono WAV plus reference JSON for each session."""
    out.mkdir(parents=True, exist_ok=True)
    for s in sessions:
        audio = next(src.rglob(f"{s}.flac"), None) or next(src.rglob(f"{s}.wav"))
        grid = next(src.rglob(f"{s}.TextGrid"))
        data, sr = sf.read(audio, dtype="float32", always_2d=True)
        if sr != SR:
            raise ValueError(f"{audio}: expected {SR} Hz, got {sr}")
        sf.write(out / f"{s}.wav", data[:, 0], SR, subtype="PCM_16")
        ref = sorted(parse_textgrid(grid))
        (out / f"{s}.ref.json").write_text(json.dumps(ref, ensure_ascii=False))
        print(f"{s}: {data.shape[0]/SR/60:.1f} min, {data.shape[1]} ch, {len(ref)} ref intervals")


def cmd_qwen(engine: str, out: Path, sessions):
    """Run the engine's offline transcription on each session WAV."""
    for s in sessions:
        wav = (out / f"{s}.wav").resolve()
        proc = subprocess.Popen([engine], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=open(out / f"{s}.engine.log", "w"), text=True, cwd="/")
        t0 = time.time()
        req = {"jsonrpc": "2.0", "id": 1, "method": "transcribe/file",
               "params": {"path": str(wav), "language": "zh"}}
        proc.stdin.write(json.dumps(req) + "\n"); proc.stdin.flush()
        while True:
            line = proc.stdout.readline()
            if not line:
                raise RuntimeError(f"{s}: engine exited early, see {s}.engine.log")
            r = json.loads(line)
            if r.get("id") == 1:
                break
        took = time.time() - t0
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}) + "\n")
        proc.stdin.flush(); proc.wait(timeout=60)
        if "error" in r:
            raise RuntimeError(f"{s}: transcribe/file failed: {r['error']}")
        res = r["result"]; res["seconds"] = took
        (out / f"{s}.qwen.json").write_text(json.dumps(res, ensure_ascii=False))
        print(f"{s}: {len(res['segments'])} segments in {took:.0f} s, model={res.get('model')}")


def cmd_fire(out: Path, sessions):
    """Re-recognize the engine's segments with FireRedASR2-AED (mlx-audio)."""
    import mlx.core as mx
    from mlx_audio.stt import load
    model = load("mlx-community/FireRedASR2-AED-mlx")
    for s in sessions:
        audio, _ = sf.read(out / f"{s}.wav", dtype="float32")
        segs = json.loads((out / f"{s}.qwen.json").read_text())["segments"]
        t0, res = time.time(), []
        for g in segs:
            clip = audio[g["startMs"] * SR // 1000: g["endMs"] * SR // 1000]
            text = model.generate(mx.array(clip), beam_size=3).text if len(clip) > SR // 10 else ""
            res.append({"startMs": g["startMs"], "endMs": g["endMs"], "text": text})
        took = time.time() - t0
        (out / f"{s}.fire.json").write_text(json.dumps({"segments": res, "seconds": took}, ensure_ascii=False))
        print(f"{s}: {len(res)} segments in {took:.0f} s")


def oracle_regions(ref, max_len=25.0, gap=0.3):
    """Merge reference intervals into speech regions for oracle segmentation.

    Overlapping or near-touching intervals (any speaker) become one region so
    overlapped speech is recognized once; a region is closed before it would
    exceed max_len seconds (a single longer interval stays whole).
    Params: ref - sorted (start, end, tier, text) list; max_len, gap - seconds.
    Returns: list of (start_s, end_s).
    """
    regions = []
    for s, e, *_ in ref:
        if regions and s <= regions[-1][1] + gap and max(e, regions[-1][1]) - regions[-1][0] <= max_len:
            regions[-1][1] = max(regions[-1][1], e)
        elif regions and s < regions[-1][1]:  # overlap but too long: still must not cut speech
            regions[-1][1] = max(regions[-1][1], e)
        else:
            regions.append([s, e])
    return regions


def cmd_oracle(out: Path, which: str, sessions):
    """Recognize oracle regions with one mlx-audio model.

    Params: which - "fire" (FireRedASR2-AED) or "qwen" (Qwen3-ASR 1.7B 8bit, same
    weights the engine uses).
    """
    import mlx.core as mx
    from mlx_audio.stt import load
    repo = {"fire": "mlx-community/FireRedASR2-AED-mlx", "qwen": "mlx-community/Qwen3-ASR-1.7B-8bit"}[which]
    model = load(repo)
    for s in sessions:
        audio, _ = sf.read(out / f"{s}.wav", dtype="float32")
        ref = json.loads((out / f"{s}.ref.json").read_text())
        t0, res = time.time(), []
        for a, b in oracle_regions(ref):
            clip = mx.array(audio[int(a * SR): int(b * SR)])
            if which == "fire":
                text = model.generate(clip, beam_size=3).text
            else:
                text = model.generate(clip, language="Chinese").text
            res.append({"startMs": int(a * 1000), "endMs": int(b * 1000), "text": text})
            mx.clear_cache()
        took = time.time() - t0
        (out / f"{s}.oracle-{which}.json").write_text(json.dumps({"segments": res, "seconds": took}, ensure_ascii=False))
        print(f"{s}: oracle-{which} {len(res)} regions in {took:.0f} s", flush=True)


def edit_ops(ref: str, hyp: str):
    """Character-level substitutions, deletions, insertions via jiwer."""
    o = jiwer.process_characters(ref, hyp)
    return o.substitutions, o.deletions, o.insertions


def cmd_score(out: Path, sessions):
    """Print per-session and pooled CER for every available hypothesis file."""
    totals = {}
    dur_total = 0.0
    for s in sessions:
        ref = "".join(normalize(t, False) for *_, t in json.loads((out / f"{s}.ref.json").read_text()))
        dur = sf.info(out / f"{s}.wav").duration; dur_total += dur
        for name in ("qwen", "fire", "oracle-qwen", "oracle-fire"):
            p = out / f"{s}.{name}.json"
            if not p.exists():
                continue
            d = json.loads(p.read_text())
            hyp = "".join(normalize(g["text"], True) for g in sorted(d["segments"], key=lambda g: g["startMs"]))
            sub, dele, ins = edit_ops(ref, hyp)
            t = totals.setdefault(name, [0, 0, 0, 0, 0.0])
            t[0] += sub; t[1] += dele; t[2] += ins; t[3] += len(ref); t[4] += d.get("seconds", 0)
            print(f"{s} {name:11s} CER {(sub+dele+ins)/len(ref)*100:6.2f}%  (S {sub} D {dele} I {ins} / N {len(ref)})  RTF {d.get('seconds',0)/dur:.3f}")
    for name, (sub, dele, ins, n, sec) in totals.items():
        print(f"ALL  {name:11s} CER {(sub+dele+ins)/n*100:6.2f}%  (S {sub} D {dele} I {ins} / N {n})  RTF {sec/dur_total:.3f}")


FILLERS = set("啊呃嗯哦额唉诶噢哎呀吧呢嘛哈")


def region_refs(ref, regions):
    """Group reference intervals into the oracle regions they were merged into.

    Params: ref - sorted intervals; regions - output of oracle_regions.
    Returns: list of (ref_text, overlapped) per region.
    """
    out = []
    for a, b in regions:
        ivs = [iv for iv in ref if iv[0] >= a - 1e-6 and iv[1] <= b + 1e-6]
        overl = any(ivs[i][1] > ivs[j][0] for i in range(len(ivs)) for j in range(i + 1, len(ivs)))
        out.append(("".join(normalize(t, False) for *_, t in ivs), overl))
    return out


def cmd_breakdown(run: Path, sessions):
    """Print CER per (model, overlap class, filler handling) and the most deleted chars."""
    stats = collections.defaultdict(lambda: [0, 0])
    dels = {m: collections.Counter() for m in ("qwen", "fire")}
    for s in sessions:
        ref = json.loads((run / f"{s}.ref.json").read_text())
        rr = region_refs(ref, oracle_regions(ref))
        for m in ("qwen", "fire"):
            segs = json.loads((run / f"{s}.oracle-{m}.json").read_text())["segments"]
            assert len(segs) == len(rr), (s, m, len(segs), len(rr))
            for (rt, ov), g in zip(rr, segs):
                ht = normalize(g["text"], True)
                for drop in (False, True):
                    r = "".join(c for c in rt if not (drop and c in FILLERS))
                    h = "".join(c for c in ht if not (drop and c in FILLERS))
                    if not r:
                        continue
                    o = jiwer.process_characters(r, h)
                    k = (m, "overlap" if ov else "single", "no-filler" if drop else "all")
                    stats[k][0] += o.substitutions + o.deletions + o.insertions
                    stats[k][1] += len(r)
                    if not drop:
                        for al in o.alignments[0]:
                            if al.type == "delete":
                                dels[m].update(r[al.ref_start_idx:al.ref_end_idx])
    for k in sorted(stats):
        e, n = stats[k]
        print(f"{k[0]:5s} {k[1]:8s} {k[2]:10s} CER {e/n*100:6.2f}%  N {n}")
    for m, c in dels.items():
        print(m, "top deleted:", " ".join(f"{ch}{n}" for ch, n in c.most_common(15)))


if __name__ == "__main__":
    cmd, *rest = sys.argv[1:]
    if cmd == "prep":
        cmd_prep(Path(rest[0]), Path(rest[1]), rest[2:])
    elif cmd == "qwen":
        cmd_qwen(rest[0], Path(rest[1]), rest[2:])
    elif cmd == "fire":
        cmd_fire(Path(rest[0]), rest[1:])
    elif cmd == "oracle":
        cmd_oracle(Path(rest[0]), rest[1], rest[2:])
    elif cmd == "breakdown":
        cmd_breakdown(Path(rest[0]), rest[1:])
    elif cmd == "score":
        cmd_score(Path(rest[0]), rest[1:])
    else:
        sys.exit(__doc__)
