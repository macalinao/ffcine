# ffcine triage transcription worker (faster-whisper). Written out and run by ffcine: it loads
# the model once, then reads one JSON job per stdin line ({"id", "path"}, path = raw 16 kHz mono
# f32le) and writes one JSON result per stdout line, so a whole card is one GPU batch.
import json
import sys

import numpy as np
from faster_whisper import WhisperModel

model_name, device, compute, language, hotwords = sys.argv[1:6]
model = WhisperModel(model_name, device=device, compute_type=compute)
print(json.dumps({"ready": True}), flush=True)

for line in sys.stdin:
    job = json.loads(line)
    try:
        audio = np.fromfile(job["path"], dtype=np.float32)
        segments, info = model.transcribe(
            audio,
            language=language or None,
            word_timestamps=True,
            vad_filter=True,
            condition_on_previous_text=False,
            beam_size=5,
            hotwords=hotwords or None,
        )
        out = []
        for s in segments:
            out.append({
                "start": round(s.start, 3),
                "end": round(s.end, 3),
                "text": s.text,
                "words": [
                    {"start": round(w.start, 3), "end": round(w.end, 3), "text": w.word,
                     "prob": round(w.probability, 3)}
                    for w in (s.words or [])
                ],
            })
        print(json.dumps({"id": job["id"], "language": info.language, "segments": out}), flush=True)
    except Exception as e:  # report and keep the batch going
        print(json.dumps({"id": job["id"], "error": str(e)}), flush=True)
