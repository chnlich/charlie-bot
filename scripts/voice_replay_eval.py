"""Voice replay evaluation: recorded dictations through the registered transcription backends.

Replays every recorded dictation under ``<sessions_dir>/*/voice/`` through the
transcription backends (src/agents/transcription/), then reports stop-to-final
latency and proper-noun accuracy against the message the user actually sent.
Engine names are the registry ids plus variants: ``local-hotwords`` (the local
backend built with --hotwords) and ``<id>-vocab`` for any backend (the same
backend called with --vocabulary). Backends with live_partials=False receive
the whole clip at once; the others receive 2048-sample chunks on a real-time
schedule, so every stop-to-final number measures from the recording's end.

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
from collections.abc import AsyncIterator
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

import numpy as np

_REPO_ROOT = Path(__file__).resolve().parents[1]
if str(_REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(_REPO_ROOT))

from src.agents import transcriber  # noqa: E402
from src.agents.transcription import registry  # noqa: E402
from src.agents.transcription.base import TranscriptionBackend  # noqa: E402
from src.agents.transcription.local import LocalTranscriptionBackend  # noqa: E402
from src.core.config import CharlieBotConfig, load_config  # noqa: E402

SAMPLE_RATE = transcriber.SAMPLE_RATE
# Live backends (live_partials=True) receive 2048-sample chunks — 128 ms of
# PCM16, the browser worklet's cadence — paced against a real-time schedule.
FRAME_SAMPLES = 2048
# A voice recording pairs with the first voice-flagged user message sent within
# this window after the recording timestamp.
GROUND_TRUTH_WINDOW_S = 900
# recordings/<UTC timestamp>_<hex>.wav, written by the voice upload endpoint
WAV_FILENAME_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{6}\.\d{3})Z_[0-9a-f]+\.wav$")
HOTWORDS_ENGINE = "local-hotwords"
VOCAB_SUFFIX = "-vocab"
SUMMARY_COLUMNS = (
    "engine", "clips", "failures", "stop_to_final_p50_s", "stop_to_final_p90_s", "first_partial_p50_s", "term_correct",
    "term_total", "sim_mean", "better_vs_local", "worse_vs_local")


def _note(message: str) -> None:
  print(message, file=sys.stderr, flush=True)


def _variant_engine_ids() -> set[str]:
  """Every engine name the script accepts: registry ids plus the two variant forms."""
  ids = set(registry.backend_ids())
  return ids | {HOTWORDS_ENGINE} | {f"{engine_id}{VOCAB_SUFFIX}" for engine_id in ids}


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


async def _transcribe_clip(backend: TranscriptionBackend, clip: Clip, vocabulary: list[str],
                           languages: list[str]) -> tuple[str, float | None, float | None]:
  """Drive one clip through ``transcribe`` and time it against the chunk hand-offs.

  Returns (final text, stop-to-final seconds, first-partial seconds); the
  latencies measure from the last (respectively first) chunk handed to the
  backend, so a paced feed measures stop-to-final from the recording's end.
  """
  pcm = clip.samples.astype("<i2").tobytes()
  handoffs: list[float] = []

  async def whole_clip() -> AsyncIterator[bytes]:
    handoffs.append(time.monotonic())
    yield pcm

  async def paced_chunks() -> AsyncIterator[bytes]:
    frame_bytes = FRAME_SAMPLES * 2
    started = time.monotonic()
    for offset in range(0, len(pcm), frame_bytes):
      target = started + (offset // 2) / SAMPLE_RATE
      delay = target - time.monotonic()
      if delay > 0:
        await asyncio.sleep(delay)
      handoffs.append(time.monotonic())
      yield pcm[offset:offset + frame_bytes]

  feed = paced_chunks() if backend.live_partials else whole_clip()
  first_partial_at: float | None = None
  final_at: float | None = None
  text = ""
  async for event in backend.transcribe(feed, vocabulary=vocabulary, languages=languages):
    now = time.monotonic()
    if event.kind == "partial":
      if first_partial_at is None:
        first_partial_at = now
    else:
      final_at = now
      text = event.text
  if final_at is None or not handoffs:
    raise RuntimeError("backend produced no final event")
  first_partial_s = max(0.0, first_partial_at - handoffs[0]) if first_partial_at is not None else None
  return text, max(0.0, final_at - handoffs[-1]), first_partial_s


async def run_engine(
    engine: str, backend: TranscriptionBackend, clips: list[Clip], vocabulary: list[str],
    languages: list[str]) -> list[dict]:
  """Replay every clip through one backend; a clip-level failure is recorded, the run continues."""
  records: list[dict] = []
  for index, clip in enumerate(clips, start=1):
    began = time.monotonic()
    record: dict = {"clip": clip.path.name}
    try:
      text, stop_to_final_s, first_partial_s = await _transcribe_clip(backend, clip, vocabulary, languages)
      record.update(ok=True, text=text, stop_to_final_s=stop_to_final_s, first_partial_s=first_partial_s)
    except Exception as exc:  # noqa: BLE001 — recorded as the clip's failure, then the run continues
      record.update(ok=False, error=f"{type(exc).__name__}: {exc}")
    record["wall_s"] = time.monotonic() - began
    records.append(record)
    outcome = (f"stop_to_final={record['stop_to_final_s']:.2f}s" if record.get("ok") else f"failed ({record['error']})")
    _note(f"[{engine} {index}/{len(clips)}] {clip.path.name}: {outcome}")
  return records


def _warmup_pcm() -> bytes:
  """The transcriber's warm-up sine as PCM16 — its shape pays the cold cost, its content none."""
  positions = np.arange(int(SAMPLE_RATE * transcriber.WARMUP_SECONDS), dtype=np.float64)
  samples = np.sin(2 * np.pi * transcriber.WARMUP_FREQUENCY_HZ * positions / SAMPLE_RATE)
  return samples.astype("<i2").tobytes()


def _prepare_local_backends(backends: dict[str, TranscriptionBackend], cfg: CharlieBotConfig) -> None:
  """Provision the speech models and warm every local backend's bundle before the timed runs.

  The server does both on its provisioning thread at boot; the replay process
  has no boot, so the first timed clip would otherwise pay the cold decode.
  Each sine goes through the public transcribe and the text is discarded. A run
  without a local backend provisions nothing: the speech models are the local
  backend's alone.
  """
  local_backends = [backend for backend in backends.values() if isinstance(backend, LocalTranscriptionBackend)]
  if not local_backends:
    return
  transcriber.ensure_models_cached(cfg)
  sine = _warmup_pcm()

  async def warm(backend: TranscriptionBackend) -> None:

    async def one_chunk() -> AsyncIterator[bytes]:
      yield sine

    async for _event in backend.transcribe(one_chunk(), vocabulary=[], languages=[]):
      pass

  for backend in local_backends:
    asyncio.run(warm(backend))


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


def _snippet(text: str, term: str) -> str:
  if not text:
    return "(empty)"
  position = text.casefold().find(term.casefold())
  start = 0 if position < 0 else max(0, position - 20)
  return text[start:start + 40]


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
  for clip_name, term, score, text in misses:
    lines.append(f"- `{clip_name}` term `{term}` "
                 f"({score['correct']}/{score['sent']}): `{_snippet(text, term)}`")
  lines.append("")
  (out_dir / "summary.md").write_text("\n".join(lines), encoding="utf-8")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  known_engines = sorted(_variant_engine_ids())
  parser.add_argument(
      "--engines", default=",".join(registry.backend_ids()), help=f"comma list from {', '.join(known_engines)}")
  parser.add_argument("--hotwords", default="", help=f"comma list of local-engine hotwords (engine {HOTWORDS_ENGINE})")
  parser.add_argument(
      "--vocabulary", default="", help="comma list passed as the backend vocabulary by the <id>-vocab variants")
  parser.add_argument(
      "--languages", default="zh,en", help="comma list of BCP-47 base codes passed to every backend as language hints")
  parser.add_argument("--terms", default="CharlieBot,Charlie Code", help="comma list of proper nouns to score")
  parser.add_argument("--only", default="", help="restrict to paths containing SUBSTR")
  parser.add_argument("--limit", type=int, default=0, help="evaluate at most N recordings")
  parser.add_argument(
      "--out", default="", help="output directory "
      "(default <charliebot_home>/voice_eval/<UTC timestamp>/)")
  parser.add_argument(
      "--dry-run", action="store_true", help="print recording, ground-truth, and audio-minute counts only")
  args = parser.parse_args(argv)
  engines = [engine.strip() for engine in args.engines.split(",") if engine.strip()]
  unknown = [engine for engine in engines if engine not in known_engines]
  if unknown:
    parser.error(f"unknown engines {unknown}; choose from {', '.join(known_engines)}")
  if not engines:
    parser.error("--engines is empty")
  if HOTWORDS_ENGINE in engines and not args.hotwords:
    parser.error(f"engine {HOTWORDS_ENGINE} needs --hotwords")
  if any(engine.endswith(VOCAB_SUFFIX) for engine in engines) and not args.vocabulary:
    parser.error("a <id>-vocab engine needs --vocabulary")
  args.engines = engines
  args.hotwords_list = [h.strip() for h in args.hotwords.split(",") if h.strip()]
  args.vocabulary_list = [v.strip() for v in args.vocabulary.split(",") if v.strip()]
  args.languages_list = [code.strip() for code in args.languages.split(",") if code.strip()]
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

  if HOTWORDS_ENGINE in args.engines:
    # The script's own variant: the local backend built with --hotwords,
    # registered like any backend so the runner below drives it through the
    # same registry interface.
    hotwords = ",".join(args.hotwords_list)
    registry.register_transcription_backend(
        HOTWORDS_ENGINE, lambda backend_cfg, **_: LocalTranscriptionBackend(backend_cfg, hotwords=hotwords))

  out_dir.mkdir(parents=True, exist_ok=True)
  backends = {
      engine: registry.build_transcription_backend(engine.removesuffix(VOCAB_SUFFIX), cfg) for engine in args.engines
  }
  _prepare_local_backends(backends, cfg)

  records: dict[str, list[dict]] = {}
  for engine in args.engines:
    vocabulary = args.vocabulary_list if engine.endswith(VOCAB_SUFFIX) else []
    records[engine] = asyncio.run(run_engine(engine, backends[engine], clips, vocabulary, args.languages_list))

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
