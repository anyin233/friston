#!/usr/bin/env python3
"""Live-caption latency and accuracy check of mm-meetingd, reproducible on any Mac.

Builds a Chinese meeting-style recording with the macOS `say` voices (several speakers,
short in-sentence pauses, 0.6–1.2 s between turns, one long monologue), streams it through
`caption/simulateFile` at real time and measures what a viewer of the caption bar sees:

- first caption: sentence onset → first partial/final that shows its first characters;
- follow lag: sentence end → first partial/final that shows its last characters;
- final latency: sentence end → the `caption/final` that contains its last characters;
- partial interval: time between consecutive partials of one utterance;
- CER of all finals against the reference text (punctuation and spaces removed).

Usage:
  scripts/eval-captions.py [--engine target/engines/Release/mm-meetingd/mm-meetingd]
                           [--workdir DIR] [--json OUT] [--repeat N]

Env vars are passed through, so model or timing overrides of the engine can be compared.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import wave
from dataclasses import dataclass, field
from pathlib import Path

RATE = 16_000

# (speaker, text, pause after) turns. Consecutive turns by one speaker with a pause < 0.5 s
# form one long monologue (> 12 s), which exercises the forced split of the live segmenter.
TURNS: list[tuple[str, str, float]] = [
    ("A", "大家好，我们开始今天的周会，先过一下上周的进度。", 0.9),
    ("B", "后端这边，订单服务的迁移已经完成了百分之八十。", 0.7),
    ("B", "剩下的主要是退款接口，预计周三之前可以联调。", 1.1),
    ("C", "前端的新版结算页上线了，转化率提升了大概三个点。", 0.8),
    ("A", "很好，那退款接口联调的时候，测试同学需要提前准备数据吗？", 1.2),
    ("D", "需要的，我们会准备二十个典型场景，包括部分退款和跨月退款。", 0.6),
    ("C", "另外，下周二有一次大促，客户端要提前发版。", 0.3),
    ("C", "我建议周五之前冻结代码，留出两天做回归测试，", 0.3),
    ("C", "同时请运维同学把监控告警的阈值调低一些，避免大促期间漏报。", 1.0),
    ("B", "数据库的扩容方案也要在周四之前评审完。", 0.9),
    ("A", "好的，那我们把这几个事项记下来，会后我发纪要给大家。", 0.8),
    ("D", "最后提醒一下，季度预算评审改到下周一上午十点。", 1.0),
]


# Speaker → (`say` voice, words per minute). Tingting is the only Mandarin voice every macOS
# ships with a full sample set (Eddy, Flo… render silence until downloaded), so speakers
# differ by speaking rate; the pitch is the same, which is fine for latency and CER.
VOICES = {"A": ("Tingting", 200), "B": ("Tingting", 240), "C": ("Tingting", 170), "D": ("Tingting", 260)}


@dataclass
class RefSentence:
    """One reference turn placed on the timeline."""

    speaker: str
    text: str
    start_ms: int
    end_ms: int


@dataclass
class Event:
    """One caption notification and when it arrived (ms since playback start)."""

    kind: str  # "partial" or "final"
    at_ms: float
    utterance: int
    start_ms: int
    text: str
    end_ms: int | None = None


@dataclass
class Report:
    """Aggregated metrics of one run."""

    cer: float
    first_caption_ms: list[float] = field(default_factory=list)
    follow_lag_ms: list[float] = field(default_factory=list)
    final_latency_ms: list[float] = field(default_factory=list)
    partial_interval_ms: list[float] = field(default_factory=list)
    missed_first: int = 0
    missed_final: int = 0


def normalize(text: str) -> str:
    """Strips punctuation and whitespace so only spoken characters are compared.

    Params: text – transcript or reference.
    Returns: the characters that count for CER and matching.
    """
    return "".join(ch for ch in text if ch.isalnum())


def edit_distance(a: str, b: str) -> int:
    """Levenshtein distance between two strings (character level).

    Params: a, b – strings.
    Returns: number of insertions, deletions and substitutions.
    """
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def cer(reference: str, hypothesis: str) -> float:
    """Character error rate after normalisation.

    Params: reference, hypothesis – full texts.
    Returns: edit distance divided by the reference length (0 for an empty reference).
    """
    ref, hyp = normalize(reference), normalize(hypothesis)
    return edit_distance(ref, hyp) / len(ref) if ref else 0.0


def shows(text: str, probe: str) -> bool:
    """Whether a caption shows a probe, tolerating one wrong character.

    ASR may get a single homophone wrong; requiring an exact match would count a correct,
    timely caption as missing.
    Params: text – caption text; probe – normalised characters to look for.
    Returns: True when some window of `text` differs from `probe` in at most one character.
    """
    t = normalize(text)
    n = len(probe)
    if n == 0:
        return True
    return any(sum(x != y for x, y in zip(t[i : i + n], probe)) <= 1 for i in range(len(t) - n + 1))


def measure(refs: list[RefSentence], events: list[Event], probe_len: int = 3) -> Report:
    """Computes the viewer-facing metrics from reference timing and caption events.

    Params: refs – sentences in time order; events – notifications in arrival order;
            probe_len – characters at the start/end of a sentence used to recognise it.
    Returns: the report.
    """
    finals = [e for e in events if e.kind == "final"]
    hyp_text = "".join(e.text for e in sorted(finals, key=lambda e: e.start_ms))
    report = Report(cer=cer("".join(r.text for r in refs), hyp_text))
    for r in refs:
        chars = normalize(r.text)
        head, tail = chars[:probe_len], chars[-probe_len:]
        # Only captions whose utterance could contain this sentence.
        candidates = [e for e in events if e.at_ms >= r.start_ms and e.start_ms <= r.end_ms + 500]
        first = next((e for e in candidates if shows(e.text, head)), None)
        if first is None:
            report.missed_first += 1
        else:
            report.first_caption_ms.append(first.at_ms - r.start_ms)
        follow = next((e for e in candidates if e.at_ms >= r.end_ms - 300 and shows(e.text, tail)), None)
        if follow is not None:
            report.follow_lag_ms.append(follow.at_ms - r.end_ms)
        final = next((e for e in candidates if e.kind == "final" and shows(e.text, tail)), None)
        if final is None:
            report.missed_final += 1
        else:
            report.final_latency_ms.append(final.at_ms - r.end_ms)
    last_partial: dict[int, float] = {}
    for e in events:
        if e.kind != "partial":
            continue
        if e.utterance in last_partial:
            report.partial_interval_ms.append(e.at_ms - last_partial[e.utterance])
        last_partial[e.utterance] = e.at_ms
    return report


def synthesize(workdir: Path) -> tuple[Path, list[RefSentence]]:
    """Renders the turns with `say` into one 16 kHz mono WAV.

    Params: workdir – where the per-turn clips and the result go.
    Returns: the WAV path and the reference timeline.
    """
    pcm = bytearray()
    refs: list[RefSentence] = []
    lead = int(0.8 * RATE)
    pcm += b"\x00\x00" * lead
    for i, (speaker, text, pause) in enumerate(TURNS):
        voice, wpm = VOICES[speaker]
        clip = workdir / f"turn{i:02d}.wav"
        subprocess.run(
            ["say", "-v", voice, "-r", str(wpm), "-o", str(clip), "--file-format=WAVE",
             f"--data-format=LEI16@{RATE}", text],
            check=True,
        )
        with wave.open(str(clip)) as w:
            frames = w.readframes(w.getnframes())
        if len(frames) < RATE:  # < 0.5 s for a whole sentence means the voice is not installed
            raise RuntimeError(f"`say -v {voice}` rendered only {len(frames) // 2} samples for {text!r}; "
                               "install the voice in System Settings › Accessibility › Spoken Content")
        start = len(pcm) // 2
        pcm += frames
        refs.append(RefSentence(speaker, text, start * 1000 // RATE, (len(pcm) // 2) * 1000 // RATE))
        pcm += b"\x00\x00" * int(pause * RATE)
    pcm += b"\x00\x00" * int(1.5 * RATE)
    out = workdir / "meeting.wav"
    with wave.open(str(out), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(bytes(pcm))
    return out, refs


class Engine:
    """A running mm-meetingd talking newline-delimited JSON-RPC over stdio."""

    def __init__(self, binary: str, log_path: Path):
        """Starts the engine; stderr goes to `log_path`.

        Params: binary – mm-meetingd path; log_path – engine log file.
        """
        self.log = open(log_path, "w")
        self.proc = subprocess.Popen(
            [binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, text=True, cwd="/"
        )
        self.responses: dict[int, dict] = {}
        self.notifications: list[tuple[float, dict]] = []
        self.cond = threading.Condition()
        self.next_id = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        """Collects responses and timestamped notifications until stdout closes."""
        assert self.proc.stdout
        for line in self.proc.stdout:
            now = time.monotonic()
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                print(f"engine wrote non-JSON to stdout: {line!r}", file=sys.stderr)
                continue
            with self.cond:
                if "id" in msg and ("result" in msg or "error" in msg):
                    self.responses[msg["id"]] = msg
                else:
                    self.notifications.append((now, msg))
                self.cond.notify_all()

    def call(self, method: str, params: dict | None = None, timeout: float = 1800) -> tuple[float, dict]:
        """Sends one request and waits for its response.

        Params: method; params; timeout – seconds.
        Returns: (monotonic send time, result).
        Raises: RuntimeError on an RPC error or timeout.
        """
        self.next_id += 1
        rid = self.next_id
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        assert self.proc.stdin
        sent = time.monotonic()
        self.proc.stdin.write(json.dumps(msg, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()
        with self.cond:
            if not self.cond.wait_for(lambda: rid in self.responses, timeout=timeout):
                raise RuntimeError(f"{method}: no response after {timeout} s")
            resp = self.responses.pop(rid)
        if "error" in resp:
            raise RuntimeError(f"{method}: {resp['error']}")
        return sent, resp["result"]

    def close(self) -> None:
        """Asks the engine to exit and waits for it."""
        try:
            self.call("shutdown", timeout=60)
        except Exception as e:  # The engine may already be gone; report but keep going.
            print(f"shutdown: {e}", file=sys.stderr)
        self.proc.wait(timeout=60)
        self.log.close()


def run_once(engine: Engine, wav: Path, warmup: Path) -> tuple[list[Event], float]:
    """Warms the models up, then streams the meeting at real time.

    Params: engine; wav – meeting recording; warmup – short clip that loads every model.
    Returns: caption events (arrival relative to playback start) and wall seconds of the run.
    """
    engine.call("caption/simulateFile", {"path": str(warmup), "speed": 0, "language": "zh"})
    with engine.cond:
        engine.notifications.clear()
    # Models are resident now, so playback starts within milliseconds of the request.
    sent, _ = engine.call("caption/simulateFile", {"path": str(wav), "speed": 1, "language": "zh"})
    wall = time.monotonic() - sent
    events = []
    with engine.cond:
        for at, msg in engine.notifications:
            method, p = msg.get("method"), msg.get("params", {})
            if method not in ("caption/partial", "caption/final"):
                continue
            events.append(
                Event(
                    kind=method.split("/")[1],
                    at_ms=(at - sent) * 1000,
                    utterance=p["utteranceId"],
                    start_ms=p["startMs"],
                    text=p["text"],
                    end_ms=p.get("endMs"),
                )
            )
    return events, wall


def pct(values: list[float], q: float) -> float:
    """Percentile with linear interpolation (q in 0..100); NaN for no values."""
    if not values:
        return float("nan")
    s = sorted(values)
    k = (len(s) - 1) * q / 100
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def summary(report: Report) -> dict:
    """Turns a report into rounded median/p90/max figures.

    Params: report.
    Returns: a JSON-friendly dict.
    """

    def stats(v: list[float]) -> dict:
        return {"n": len(v), "median": round(pct(v, 50)), "p90": round(pct(v, 90)), "max": round(max(v)) if v else None}

    return {
        "cer": round(report.cer * 100, 2),
        "firstCaptionMs": stats(report.first_caption_ms),
        "followLagMs": stats(report.follow_lag_ms),
        "finalLatencyMs": stats(report.final_latency_ms),
        "partialIntervalMs": stats(report.partial_interval_ms),
        "missedFirst": report.missed_first,
        "missedFinal": report.missed_final,
    }


def main() -> int:
    """Entry point; prints the summary as JSON."""
    root = Path(__file__).resolve().parent.parent
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--engine", default=str(root / "target/engines/Release/mm-meetingd/mm-meetingd"))
    ap.add_argument("--workdir", help="keep the synthesized audio and engine log here")
    ap.add_argument("--json", help="also write the summary and raw events to this file")
    ap.add_argument("--repeat", type=int, default=1, help="real-time runs to average over")
    args = ap.parse_args()

    workdir = Path(args.workdir or tempfile.mkdtemp(prefix="mm-eval-captions-"))
    workdir.mkdir(parents=True, exist_ok=True)
    wav, refs = synthesize(workdir)
    warmup = workdir / "warmup.wav"
    with wave.open(str(wav)) as src, wave.open(str(warmup), "wb") as dst:
        dst.setparams(src.getparams())
        dst.writeframes(src.readframes(8 * RATE))
    print(f"audio {wav} ({refs[-1].end_ms / 1000:.1f} s, {len(refs)} turns); engine log {workdir / 'engine.log'}",
          file=sys.stderr)

    engine = Engine(args.engine, workdir / "engine.log")
    runs = []
    try:
        for i in range(args.repeat):
            events, wall = run_once(engine, wav, warmup)
            s = summary(measure(refs, events))
            s["wallSeconds"] = round(wall, 1)
            runs.append({"summary": s, "events": [e.__dict__ for e in events]})
            print(json.dumps(s, ensure_ascii=False))
    finally:
        engine.close()
    if args.json:
        Path(args.json).write_text(
            json.dumps({"refs": [r.__dict__ for r in refs], "runs": runs}, ensure_ascii=False, indent=1)
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
