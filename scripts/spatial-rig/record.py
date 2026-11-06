"""Spatial test rig, part 2: record from one input via mm-meetingd and print spatial/cue results.

Usage: record.py <mm-meetingd> <device-uid> <out-dir> <seconds> ['{"spatialSpacingCm":20,"spatialFlip":true}']
For a loopback device, export MM_SPATIAL_TEST_DEVICE_UID=<device-uid> first so the engine treats it
as a microphone pair (virtual devices are never spatial otherwise). Deletes nothing; remove
<out-dir>/rec afterwards if it holds real speech.
"""
import json, subprocess, sys, time, threading
eng, uid, out, secs = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
extra = json.loads(sys.argv[5]) if len(sys.argv) > 5 else {}
p = subprocess.Popen([eng], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(out + "/engine.err", "w"), text=True, cwd="/")
notes, replies, cv = [], {}, threading.Condition()
def reader():
    for line in p.stdout:
        m = json.loads(line)
        with cv:
            if "id" in m: replies[m["id"]] = m; cv.notify_all()
            elif m.get("method") in ("spatial/cue", "caption/final"): notes.append((time.time(), m["method"], m["params"]))
threading.Thread(target=reader, daemon=True).start()
def call(i, m, params=None):
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":i,"method":m,"params":params})+"\n"); p.stdin.flush()
    with cv:
        while i not in replies: cv.wait(120)
    return replies[i].get("result", replies[i].get("error"))
preset = {"title": "xreal", "microphoneUID": uid, "outputDirectory": out + "/rec", "captionLanguage": "zh", **extra}
st = call(1, "capture/start", preset); print("start:", st.get("microphoneChannels") if isinstance(st, dict) else st)
time.sleep(secs)
sp = call(2, "capture/stop"); print("files:", sp.get("files") if isinstance(sp, dict) else sp)
time.sleep(3); p.kill()
json.dump([[t, m, prm] for t, m, prm in notes], open(out + "/notes.json", "w"), ensure_ascii=False)
cues = [prm for _, m, prm in notes if m == "spatial/cue"]
finals = [prm for _, m, prm in notes if m == "caption/final" and prm.get("text")]
print(f"finals={len(finals)} cues={len(cues)}")
for c in cues: print("cue", c["utteranceId"], c["speaker"], [round(x, 2) for x in c["position"]], round(c["confidence"], 2), c.get("azimuthDeg"))
