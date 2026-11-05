#!/usr/bin/env python3
"""Speaker-diarization check of mm-meetingd against a reference RTTM (e.g. AMI).

Usage: scripts/eval-diarization.py <mm-meetingd> <audio.wav> <reference.rttm> {file|live}
  file = offline `transcribe/file` (Pyannote); live = `caption/simulateFile` (live pipeline).
Prints the number of hypothesis speakers and the speaker-confusion rate over 100 ms frames
where exactly one reference speaker talks. Threshold sweeps: set MM_FUSION_* env vars
(see SpeakerFusionConfig.fromEnvironment).
"""
import json, subprocess, sys, itertools, collections
engine, wav, rttm, mode = sys.argv[1:5]
ref = collections.defaultdict(set)
for line in open(rttm):
    p = line.split(); s, d, spk = float(p[3]), float(p[4]), p[7]
    for f in range(int(s*10), int((s+d)*10)): ref[f].add(spk)
proc = subprocess.Popen([engine], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, cwd="/")
params = {"path": wav, "language": "en"}
method = "transcribe/file" if mode == "file" else "caption/simulateFile"
if mode != "file": params["speed"] = 0
proc.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params})+"\n"); proc.stdin.flush()
while True:
    r = json.loads(proc.stdout.readline())
    if r.get("id") == 1: break
proc.stdin.write(json.dumps({"jsonrpc":"2.0","id":2,"method":"shutdown"})+"\n"); proc.stdin.flush()
if "error" in r: sys.exit(f"engine error: {r['error']}")
segs = r["result"].get("segments") or r["result"].get("finals")
hyp = {}
for s in segs:
    k = s["speaker"]; lab = k.get("index", k["kind"])
    for f in range(s["startMs"]//100, s["endMs"]//100): hyp[f] = lab
# frames where exactly one reference speaker talks and the hypothesis has a label
frames = [f for f in hyp if len(ref.get(f, ())) == 1]
refs = sorted({s for v in ref.values() for s in v}); hyps = sorted(set(hyp.values()), key=str)
co = collections.Counter((hyp[f], next(iter(ref[f]))) for f in frames)
# best many-to-one mapping hyp -> ref (each hyp cluster to its majority ref speaker)
correct = sum(max(co[(h, rs)] for rs in refs) for h in hyps)
# one-to-one optimal mapping (stricter): permutations over the 4 ref speakers
best = 0
for perm in itertools.permutations(hyps, min(len(hyps), len(refs))) if len(hyps) <= 8 else []:
    best = max(best, sum(co[(h, rs)] for h, rs in zip(perm, refs)))
covered = sum(1 for f in ref if len(ref[f]) == 1)
print(f"{mode}: segments={len(segs)} hyp_speakers={len(hyps)} ref_speakers={len(refs)} "
      f"single-speaker frames scored={len(frames)}/{covered} "
      f"confusion(many-to-one)={1-correct/len(frames):.3f} confusion(one-to-one)={1-best/len(frames):.3f}" if frames else f"{mode}: no overlap")
print("hyp cluster sizes:", collections.Counter(hyp.values()).most_common(8))
