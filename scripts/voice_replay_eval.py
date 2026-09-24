"""Voice replay evaluation: recorded dictations through the local engine and Muse.

The measurement gate of the Muse streaming-transcription plan: replays every
recorded dictation under ``<sessions_dir>/*/voice/`` through the local sherpa
engine (optionally hotword-biased) and Meta's Muse Voice Transcribe realtime
API, then reports stop-to-final latency and proper-noun accuracy against the
message the user actually sent.

Run as ``uv run python scripts/voice_replay_eval.py`` from the repository root.
``--dry-run`` prints recording, ground-truth, and audio-minute counts only.

Results contain the user's own speech and sent messages: they are personal
data, written only under ``--out`` (default ``<charliebot_home>/voice_eval/``),
and the script refuses an ``--out`` inside the repository working tree.
"""

from __future__ import annotations

import argparse
import asyncio
import difflib
import json
import re
import sys
import time
import wave
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

import numpy as np

_REPO_ROOT = Path(__file__).resolve().parents[1]
if str(_REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(_REPO_ROOT))

from src.agents import transcriber  # noqa: E402
from src.core.config import CharlieBotConfig, load_config  # noqa: E402
from src.core.credentials import get_credentials  # noqa: E402

SAMPLE_RATE = transcriber.SAMPLE_RATE
# The Muse client sends audio in 2048-sample binary frames (128 ms of PCM16),
# paced against an absolute real-time schedule.
MUSE_FRAME_SAMPLES = 2048
MUSE_MODEL = "muse-voice-transcribe-1.0"
DEFAULT_MUSE_URL = "wss://api.meta.ai/v1/asr/realtime"
# A voice recording pairs with the first voice-flagged user message sent within
# this window after the recording timestamp.
GROUND_TRUTH_WINDOW_S = 900
# recordings/<UTC timestamp>_<hex>.wav, written by the voice upload endpoint
WAV_FILENAME_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{6}\.\d{3})Z_[0-9a-f]+\.wav$")
ENGINES = ("local", "local-hotwords", "muse", "muse-keywords")
SUMMARY_COLUMNS = (
    "engine", "clips", "failures", "stop_to_final_p50_s", "stop_to_final_p90_s", "first_partial_p50_s",
    "offline_wait_p50_s", "incremental_wait_p50_s", "term_correct", "term_total", "sim_mean", "better_vs_local",
    "worse_vs_local")


def _note(message: str) -> None:
  print(message, file=sys.stderr, flush=True)


@dataclass
class Clip:
  path: Path
  session_id: str
  recorded_at: datetime
  samples: np.ndarray

  @property
  def duration_s(self) -> float:
    return self.samples.size / SAMPLE_RATE


def load_clips(cfg: CharlieBotConfig, only: str = "", limit: int = 0) -> list[Clip]:
  """Every 16 kHz mono PCM16 recording under sessions/*/voice/, oldest first.

  Any file that is not that format — or whose name does not carry the upload
  endpoint's UTC timestamp shape — is skipped with the reason logged.
  """
  wavs = sorted(cfg.sessions_dir.glob("*/voice/*.wav"))
  if only:
    wavs = [wav for wav in wavs if only in str(wav)]
  clips: list[Clip] = []
  for wav in wavs:
    match = WAV_FILENAME_RE.match(wav.name)
    if not match:
      _note(f"skip {wav.name}: filename is not <UTC timestamp>_<hex>.wav")
      continue
    try:
      with wave.open(str(wav), "rb") as reader:
        channels, width, rate = reader.getnchannels(), reader.getsampwidth(), reader.getframerate()
        if (channels, width, rate) != (1, 2, SAMPLE_RATE):
          _note(f"skip {wav.name}: not 16 kHz mono PCM16 ({channels}ch/{width * 8}bit/{rate}Hz)")
          continue
        pcm = reader.readframes(reader.getnframes())
    except (wave.Error, EOFError) as exc:
      _note(f"skip {wav.name}: malformed WAV: {exc}")
      continue
    recorded_at = datetime.strptime(match.group(1), "%Y-%m-%dT%H%M%S.%f").replace(tzinfo=UTC)
    clips.append(
        Clip(
            path=wav,
            session_id=wav.parent.parent.name,
            recorded_at=recorded_at,
            samples=np.frombuffer(pcm, dtype="<i2")))
  if limit:
    clips = clips[:limit]
  return clips


def find_ground_truth(session_dir: Path, recorded_at: datetime) -> str | None:
  """The first voice-flagged string user event within 900 s after the recording.

  Reads the session's ``data/chat_events.jsonl``; None means the clip is scored
  for latency only.
  """
  events_path = session_dir / "data" / "chat_events.jsonl"
  if not events_path.is_file():
    return None
  with events_path.open(encoding="utf-8") as handle:
    for line_number, line in enumerate(handle, start=1):
      try:
        event = json.loads(line)
      except json.JSONDecodeError as exc:
        raise ValueError(f"malformed JSON at {events_path}:{line_number}: {exc}") from exc
      if event.get("type") != "user" or event.get("is_voice") is not True:
        continue
      content = event.get("content")
      if not isinstance(content, str):
        continue
      timestamp = datetime.fromisoformat(event["timestamp"])
      if timestamp < recorded_at:
        continue
      if (timestamp - recorded_at).total_seconds() <= GROUND_TRUTH_WINDOW_S:
        return content
  return None


def _normalize_for_terms(text: str) -> str:
  return text.casefold().replace(" ", "")


def count_term(text: str, term: str) -> int:
  """Case-insensitive occurrence count with spaces ignored ("Charlie Bot" == CharlieBot)."""
  return _normalize_for_terms(text).count(_normalize_for_terms(term))


def score_terms(output: str, sent: str, terms: list[str]) -> dict[str, dict[str, int]]:
  """Per-term {sent, output, correct}; correct is the minimum of the two counts."""
  scores: dict[str, dict[str, int]] = {}
  for term in terms:
    in_sent = count_term(sent, term)
    in_output = count_term(output, term)
    scores[term] = {"sent": in_sent, "output": in_output, "correct": min(in_sent, in_output)}
  return scores


def ensure_out_dir(out: Path, repo_root: Path) -> Path:
  """Resolve ``out`` and refuse any location inside the repository working tree.

  Results carry the user's speech and messages; the repository is public, so the
  guard fires before anything is written. ``resolve()`` follows symlinks, so a
  symlinked path aimed into the tree is caught too.
  """
  resolved = out.resolve()
  repo = repo_root.resolve()
  if resolved == repo or repo in resolved.parents:
    raise SystemExit(
        f"--out {out} resolves inside the repository working tree ({repo}); "
        "replay results are personal data and must be written outside it")
  return resolved


@dataclass
class MuseResult:
  ok: bool
  text: str = ""
  stop_to_final_s: float | None = None
  first_partial_s: float | None = None
  close_code: int | None = None
  close_reason: str = ""
  error: str = ""


def build_handshake(access_token: str, keywords: list[str], language_bias: list[str]) -> dict:
  """The first frame on the Muse socket; keywords/languageBias only when non-empty."""
  handshake: dict = {
      "mode": "PUSH_TO_TALK",
      "authorization": {
          "accessToken": access_token
      },
      "audioEncoding": "PCM_16KHZ",
      "model": MUSE_MODEL,
      "partialMode": "CUMULATIVE",
      "emitAudioProgress": False,
  }
  if keywords:
    handshake["keywords"] = keywords
  if language_bias:
    handshake["languageBias"] = language_bias
  return handshake


def _transcript_event(event: dict) -> str | None:
  if event.get("type") == "transcript":
    return event.get("text", "")
  return None


async def muse_transcribe(
    url: str,
    access_token: str,
    samples: np.ndarray,
    keywords: list[str],
    language_bias: list[str],
) -> MuseResult:
  """Replay one clip through the Muse realtime API at real-time pace.

  Handshake first, its reply read before any audio; then 2048-sample binary
  frames paced against an absolute schedule; then the endStream frame and events
  until the socket closes. Never logs or returns the handshake, so the access
  token cannot reach any output.
  """
  from websockets.asyncio.client import connect
  from websockets.exceptions import ConnectionClosed

  async with connect(url) as socket:
    await socket.send(json.dumps(build_handshake(access_token, keywords, language_bias)))
    try:
      reply = json.loads(await socket.recv())
    except (json.JSONDecodeError, TypeError) as exc:
      return MuseResult(ok=False, error=f"handshake reply is not JSON: {exc}")
    if reply.get("type") == "error" or "error" in reply:
      return MuseResult(ok=False, error=f"handshake rejected: {reply.get('message', reply)}")

    events: list[tuple[float, dict]] = []

    async def read_events() -> None:
      async for message in socket:
        events.append((time.monotonic(), json.loads(message)))  # noqa: PERF401

    reader = asyncio.create_task(read_events())
    pcm = samples.astype("<i2").tobytes()
    frame_bytes = MUSE_FRAME_SAMPLES * 2
    first_frame_at: float | None = None
    try:
      stream_start = time.monotonic()
      for offset in range(0, len(pcm), frame_bytes):
        # Absolute schedule: sleep to stream_start plus the audio already sent,
        # never a fixed per-frame pause.
        target = stream_start + (offset / 2) / SAMPLE_RATE
        delay = target - time.monotonic()
        if delay > 0:
          await asyncio.sleep(delay)
        if first_frame_at is None:
          first_frame_at = time.monotonic()
        await socket.send(pcm[offset:offset + frame_bytes])
      end_stream_at = time.monotonic()
      await socket.send(json.dumps({"type": "endStream"}))
      await reader
    except ConnectionClosed as exc:
      # Reap the reader so its own close exception is not lost to the GC.
      reader.cancel()
      await asyncio.gather(reader, return_exceptions=True)
      code = exc.rcvd.code if exc.rcvd is not None else None
      reason = exc.rcvd.reason if exc.rcvd is not None else ""
      return MuseResult(
          ok=False, close_code=code, close_reason=reason, error=f"connection closed mid-stream (code {code}: {reason})")

    if socket.close_code not in (1000, 1001):
      return MuseResult(
          ok=False,
          close_code=socket.close_code,
          close_reason=socket.close_reason or "",
          error=f"abnormal close (code {socket.close_code}: {socket.close_reason})")
    finals = [
        (at, text) for at, event in events if (text := _transcript_event(event)) is not None and event.get("final")
    ]
    if not finals:
      return MuseResult(ok=False, error="stream closed without a final transcript")
    speech_complete = next(
        (event.get("transcript", "") for _, event in events if event.get("type") == "speechComplete"), None)
    text = speech_complete or " ".join(text for _, text in finals)
    first_partial_at = next(
        (at for at, event in events if _transcript_event(event) is not None and not event.get("final")), None)
    return MuseResult(
        ok=True,
        text=text,
        stop_to_final_s=max(0.0, finals[-1][0] - end_stream_at),
        first_partial_s=(
            max(0.0, first_partial_at -
                first_frame_at) if first_partial_at is not None and first_frame_at is not None else None))


def build_local_bundle(cfg: CharlieBotConfig, hotwords: str) -> transcriber._SpeechModelBundle:
  """One sherpa recognizer for the whole run; the provisioning path is the
  transcriber's own fallback loader (downloads/verifies the CPU artifacts)."""
  paths = transcriber._ensure_sherpa_paths_cached(cfg)
  bundle = transcriber.create_sherpa_bundle(paths, hotwords=hotwords)
  transcriber.warm_up_bundle(bundle)
  return bundle


def run_local(bundle: transcriber._SpeechModelBundle, clip: Clip) -> dict:
  """Decode each VAD window with wall-clock timing; join exactly as production does."""
  vad = transcriber._open_vad(bundle.vad_config, clip.samples.size / SAMPLE_RATE + 10)
  windows = transcriber.offline_decode_windows(vad, clip.samples)
  texts: list[str] = []
  decode_s: list[float] = []
  for _start, _end, left, right in windows:
    began = time.monotonic()
    text = transcriber._decode_samples(bundle, clip.samples[left:right].astype(np.float32) / 32768.0)
    decode_s.append(time.monotonic() - began)
    texts.append(text)
  return {
      "text": transcriber._join_segments(*texts),
      "offline_wait_s": sum(decode_s),
      "incremental_wait_s": _incremental_wait(bundle, clip, windows, decode_s),
      "segments": len(windows),
  }


def _incremental_wait(
    bundle: transcriber._SpeechModelBundle, clip: Clip, windows: list[tuple[int, int, int, int]],
    decode_s: list[float]) -> float:
  """Sequential-decoder simulation: a segment becomes available at its end plus the
  VAD's min_silence_duration (capped at the clip duration); wait = how far past the
  clip's end the last decode completes."""
  min_silence_s = bundle.vad_config.silero_vad.min_silence_duration
  completion = 0.0
  for (_start, end, _left, _right), decode_time in zip(windows, decode_s, strict=True):
    available = min(end / SAMPLE_RATE + min_silence_s, clip.duration_s)
    completion = max(available, completion) + decode_time
  return max(0.0, completion - clip.duration_s)


async def run_muse_engine(
    url: str, access_token: str, clips: list[Clip], keywords: list[str], language_bias: list[str]) -> list[dict]:
  """Replay every clip through Muse; a clip-level failure is recorded, the run continues."""
  records: list[dict] = []
  for index, clip in enumerate(clips, start=1):
    began = time.monotonic()
    try:
      result = await muse_transcribe(url, access_token, clip.samples, keywords, language_bias)
    except Exception as exc:  # noqa: BLE001 — recorded as the clip's failure, then the run continues
      result = MuseResult(ok=False, error=f"{type(exc).__name__}: {exc}")
    records.append(
        {
            "clip": clip.path.name,
            "ok": result.ok,
            "text": result.text if result.ok else "",
            "stop_to_final_s": result.stop_to_final_s,
            "first_partial_s": result.first_partial_s,
            "close_code": result.close_code,
            "close_reason": result.close_reason,
            "error": result.error,
            "wall_s": time.monotonic() - began,
        })
    _note(f"[muse {index}/{len(clips)}] {clip.path.name}: "
          f"{'ok' if result.ok else f'failed ({result.error})'}")
  return records


def _percentile(values: list[float], q: float) -> float | None:
  if not values:
    return None
  ordered = sorted(values)
  position = (len(ordered) - 1) * q / 100
  lower = int(position)
  upper = min(lower + 1, len(ordered) - 1)
  return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def _fmt(value: float | None) -> str:
  return "N/A" if value is None else f"{value:.3f}"


def _snippet(text: str, term: str, width: int = 40) -> str:
  if not text:
    return "(empty)"
  position = text.casefold().find(term.casefold())
  start = 0 if position < 0 else max(0, position - width // 2)
  return text[start:start + width]


def write_outputs(
    out_dir: Path, engines: list[str], clips: list[Clip], ground_truth: dict[str, str | None], terms: list[str],
    records: dict[str, list[dict]]) -> None:
  """Write results.json (one record per clip and engine) and summary.md."""
  clip_records = []
  for engine in engines:
    for clip, record in zip(clips, records[engine], strict=True):
      entry = {
          "engine": engine,
          "clip": record["clip"],
          "session": clip.session_id,
          "duration_s": round(clip.duration_s, 3),
          "sent": ground_truth.get(record["clip"]),
      }
      entry.update(record)
      clip_records.append(entry)
  payload = {
      "generated_at": datetime.now(UTC).isoformat(),
      "engines": engines,
      "terms": terms,
      "clips": clip_records,
  }
  (out_dir / "results.json").write_text(json.dumps(payload, ensure_ascii=False, indent=1), encoding="utf-8")

  local_records = records.get("local")
  rows = []
  for engine in engines:
    engine_records = records[engine]
    failures = sum(1 for record in engine_records if not record.get("ok", True))
    stop_to_final = [
        r["stop_to_final_s"] for r in engine_records if r.get("ok") and r.get("stop_to_final_s") is not None
    ]
    first_partial = [
        r["first_partial_s"] for r in engine_records if r.get("ok") and r.get("first_partial_s") is not None
    ]
    offline_wait = [r["offline_wait_s"] for r in engine_records if r.get("ok") and r.get("offline_wait_s") is not None]
    incremental_wait = [
        r["incremental_wait_s"] for r in engine_records if r.get("ok") and r.get("incremental_wait_s") is not None
    ]
    scored = [r for r in engine_records if r.get("ok") and "sim" in r]
    term_correct = sum(r["terms"][term]["correct"] for r in scored for term in terms)
    term_total = sum(r["terms"][term]["sent"] for r in scored for term in terms)
    sim_mean = (sum(r["sim"] for r in scored) / len(scored)) if scored else None
    better = worse = None
    if local_records is not None and engine != "local":
      pairs = [
          (record, local)
          for record, local in zip(engine_records, local_records, strict=True)
          if record.get("ok") and local.get("ok") and "sim" in record and "sim" in local
      ]
      better = sum(1 for record, local in pairs if record["sim"] > local["sim"])
      worse = sum(1 for record, local in pairs if record["sim"] < local["sim"])
    rows.append(
        {
            "engine": engine,
            "clips": len(engine_records),
            "failures": failures,
            "stop_to_final_p50_s": _percentile(stop_to_final, 50),
            "stop_to_final_p90_s": _percentile(stop_to_final, 90),
            "first_partial_p50_s": _percentile(first_partial, 50),
            "offline_wait_p50_s": _percentile(offline_wait, 50),
            "incremental_wait_p50_s": _percentile(incremental_wait, 50),
            "term_correct": term_correct,
            "term_total": term_total,
            "sim_mean": sim_mean,
            "better_vs_local": better,
            "worse_vs_local": worse,
        })

  lines = [
      "# Voice replay evaluation",
      "",
      f"Generated {datetime.now(UTC).isoformat()}; engines: {', '.join(engines)}; " + f"terms: {', '.join(terms)}",
      "",
      "| " + " | ".join(SUMMARY_COLUMNS) + " |",
      "|" + "---|" * len(SUMMARY_COLUMNS),
  ]
  for row in rows:
    lines.append("| " + " | ".join(  # noqa: PERF401
        _fmt(row[column]) if isinstance(row[column], float) else
        ("N/A" if row[column] is None else str(row[column]))
        for column in SUMMARY_COLUMNS) + " |")
  lines.append("")
  lines.append("## Residual term misses")
  misses = [
      (record["clip"], term, record["terms"][term], record["text"]) for engine in engines for record in records[engine]
      if record.get("ok") and "terms" in record for term in terms
      if record["terms"][term]["correct"] < record["terms"][term]["sent"]
  ]
  if not misses:
    lines.append("(none)")
  for clip_name, term, score, text in misses:
    lines.append(f"- `{clip_name}` term `{term}` "
                 f"({score['correct']}/{score['sent']}): `{_snippet(text, term)}`")
  lines.append("")
  (out_dir / "summary.md").write_text("\n".join(lines), encoding="utf-8")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument("--engines", default="local,muse", help=f"comma list from {','.join(ENGINES)}")
  parser.add_argument("--hotwords", default="", help="comma list of local-engine hotwords")
  parser.add_argument("--keywords", default="", help="comma list of Muse keywords")
  parser.add_argument(
      "--language-bias", default="Mandarin Chinese,English", help="comma list passed as languageBias when non-empty")
  parser.add_argument("--terms", default="CharlieBot,Charlie Code", help="comma list of proper nouns to score")
  parser.add_argument("--only", default="", help="restrict to paths containing SUBSTR")
  parser.add_argument("--limit", type=int, default=0, help="evaluate at most N recordings")
  parser.add_argument("--muse-url", default=DEFAULT_MUSE_URL)
  parser.add_argument(
      "--out", default="", help="output directory "
      "(default <charliebot_home>/voice_eval/<UTC timestamp>/)")
  parser.add_argument(
      "--dry-run", action="store_true", help="print recording, ground-truth, and audio-minute counts only")
  args = parser.parse_args(argv)
  engines = [engine.strip() for engine in args.engines.split(",") if engine.strip()]
  unknown = [engine for engine in engines if engine not in ENGINES]
  if unknown:
    parser.error(f"unknown engines {unknown}; choose from {', '.join(ENGINES)}")
  if not engines:
    parser.error("--engines is empty")
  if "local-hotwords" in engines and not args.hotwords:
    parser.error("engine local-hotwords needs --hotwords")
  if "muse-keywords" in engines and not args.keywords:
    parser.error("engine muse-keywords needs --keywords")
  args.engines = engines
  args.hotwords_list = [h.strip() for h in args.hotwords.split(",") if h.strip()]
  args.keywords_list = [k.strip() for k in args.keywords.split(",") if k.strip()]
  args.language_bias_list = [b.strip() for b in args.language_bias.split(",") if b.strip()]
  args.terms_list = [t.strip() for t in args.terms.split(",") if t.strip()]
  return args


def main(argv: list[str] | None = None) -> int:
  args = parse_args(argv)
  cfg = load_config()
  out_dir = ensure_out_dir(
      Path(args.out) if args.out else cfg.charliebot_home / "voice_eval" / datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ"),
      _REPO_ROOT)
  clips = load_clips(cfg, args.only, args.limit)

  ground_truth = {clip.path.name: find_ground_truth(clip.path.parent.parent, clip.recorded_at) for clip in clips}
  matched = sum(1 for sent in ground_truth.values() if sent is not None)
  total_minutes = sum(clip.duration_s for clip in clips) / 60
  if args.dry_run:
    print(f"recordings: {len(clips)}")
    print(f"ground-truth matches: {matched}")
    print(f"audio minutes: {total_minutes:.1f}")
    return 0

  out_dir.mkdir(parents=True, exist_ok=True)
  records: dict[str, list[dict]] = {}
  for engine in args.engines:
    if engine in ("local", "local-hotwords"):
      bundle = build_local_bundle(cfg, ",".join(args.hotwords_list) if engine == "local-hotwords" else "")
      engine_records = []
      for index, clip in enumerate(clips, start=1):
        began = time.monotonic()
        result = run_local(bundle, clip)
        engine_records.append({"clip": clip.path.name, "ok": True, "wall_s": time.monotonic() - began, **result})
        _note(
            f"[{engine} {index}/{len(clips)}] {clip.path.name}: "
            f"{result['offline_wait_s']:.2f}s decode over {result['segments']} segment(s)")
      records[engine] = engine_records
    else:
      access_token = str(get_credentials().require("meta", "model_api_key"))
      engine_records = asyncio.run(
          run_muse_engine(
              args.muse_url, access_token, clips, args.keywords_list if engine == "muse-keywords" else [],
              args.language_bias_list))
      records[engine] = engine_records

  # Attach ground-truth scoring to each successful record.
  for engine in args.engines:
    for record in records[engine]:
      sent = ground_truth[record["clip"]]
      if sent is None or not record.get("ok"):
        continue
      record["sim"] = difflib.SequenceMatcher(None, record["text"], sent).ratio()
      record["terms"] = score_terms(record["text"], sent, args.terms_list)

  write_outputs(out_dir, args.engines, clips, ground_truth, args.terms_list, records)
  print(f"wrote {out_dir / 'results.json'} and {out_dir / 'summary.md'}")
  return 0


if __name__ == "__main__":
  raise SystemExit(main())
