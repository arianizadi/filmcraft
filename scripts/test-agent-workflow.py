#!/usr/bin/env python3
"""Exercise FilmCraft's review workflow against original footage, without modifying it.

Python 3 standard library only. FilmCraft performs every render/edit/export. ffprobe is an
external read-only oracle. Two camera tracks reuse the same source intentionally; the two
range removals are synthetic tests, never claimed to be speech or dialogue analysis.

Example:
  python3 scripts/test-agent-workflow.py --binary target/release/filmcraft-cli \
    --source '/path/to/original.MP4' --output-dir "$HOME/Desktop/FilmCraft Tests"

A unique run directory contains PNGs, a project, a short SDR proxy, requests, metadata, and
report.json. The lost-reply test deliberately discards the apply response at the client;
actual TCP reconnect/lost-response behavior is covered by FilmCraft's bridge regression test.
"""

from __future__ import annotations

import argparse
import base64
import copy
import datetime
from fractions import Fraction
import hashlib
import json
import math
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import threading
import time
import traceback
import uuid

TICKS_PER_SECOND = 254_016_000_000
MAX_REPLY_BYTES = 128 * 1024 * 1024


class SmokeFailure(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise SmokeFailure(message)


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False, allow_nan=False) + "\n")


def source_identity(path):
    """Sampled fingerprint, explicitly not a whole-file content hash."""
    stat = path.stat()
    digest = hashlib.sha256()
    digest.update(str(stat.st_size).encode())
    with path.open("rb") as stream:
        digest.update(stream.read(1024 * 1024))
        if stat.st_size > 1024 * 1024:
            stream.seek(max(0, stat.st_size - 1024 * 1024))
            digest.update(stream.read(1024 * 1024))
    return {"size": stat.st_size, "mtimeNs": stat.st_mtime_ns, "inode": stat.st_ino,
            "device": stat.st_dev, "sampledSha256": digest.hexdigest(),
            "hashCoverage": "size plus first and last MiB; not a full-file checksum"}


def run_json(command, timeout, stderr_path):
    with stderr_path.open("a") as errors:
        process = subprocess.run(command, stdout=subprocess.PIPE, stderr=errors, text=True,
                                 timeout=timeout, check=False)
    require(process.returncode == 0, f"Command failed ({process.returncode}); see {stderr_path}: {command}")
    try:
        return json.loads(process.stdout)
    except json.JSONDecodeError as error:
        raise SmokeFailure(f"Command did not return JSON: {command}: {process.stdout[:1000]}") from error


def ffprobe(binary, path, timeout, stderr_path):
    return run_json([binary, "-v", "error", "-show_streams", "-show_format", "-of", "json", str(path)],
                    timeout, stderr_path)


def video_stream(probe):
    return next((stream for stream in probe.get("streams", []) if stream.get("codec_type") == "video"), None)


def frame_tick(frame, rate):
    numerator = TICKS_PER_SECOND * rate.denominator * frame
    return (numerator + rate.numerator - 1) // rate.numerator


def sequence_structure(sequence):
    """Exclude navigation while retaining all stored edit/grade/timing fields."""
    value = copy.deepcopy(sequence)
    for key in ("playhead", "selection", "transitionSelection", "gapSelection", "revision", "sessionId", "documentEpoch"):
        value.pop(key, None)
    return value


class McpClient:
    def __init__(self, binary, output, timeout):
        self.timeout = timeout
        self.next_id = 1
        self.messages = queue.Queue()
        self.stderr = (output / "mcp-stderr.log").open("a")
        self.requests = (output / "mcp-requests.jsonl").open("a")
        self.process = subprocess.Popen([str(binary), "--data-dir", str(output / "app-data"), "mcp"],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.stderr, text=True, bufsize=1)
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.initialization = self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                                       "clientInfo": {"name": "filmcraft-workflow-smoke", "version": "1"}})
        self.notify("notifications/initialized", {})

    def _read(self):
        try:
            while True:
                line = self.process.stdout.readline(MAX_REPLY_BYTES + 1)
                if not line:
                    self.messages.put(SmokeFailure("MCP server closed stdout; see mcp-stderr.log"))
                    return
                if len(line.encode()) > MAX_REPLY_BYTES or not line.endswith("\n"):
                    self.messages.put(SmokeFailure("MCP reply exceeds the client's 128 MiB bound"))
                    return
                self.messages.put(json.loads(line))
        except Exception as error:
            self.messages.put(error)

    def _send(self, message):
        self.requests.write(json.dumps(message, ensure_ascii=False, allow_nan=False) + "\n")
        self.requests.flush()
        self.process.stdin.write(json.dumps(message, ensure_ascii=False, allow_nan=False) + "\n")
        self.process.stdin.flush()

    def notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def rpc(self, method, params, discard=False):
        request_id = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + self.timeout
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, f"MCP request timed out: {method}")
            try:
                reply = self.messages.get(timeout=remaining)
            except queue.Empty as error:
                raise SmokeFailure(f"MCP request timed out: {method}") from error
            if isinstance(reply, Exception):
                raise reply
            if "method" in reply:
                if "id" in reply:
                    self._send({"jsonrpc": "2.0", "id": reply["id"], "result": {}})
                continue
            if reply.get("id") != request_id:
                raise SmokeFailure(f"Unexpected MCP response ID: {reply.get('id')} (expected {request_id})")
            if discard:
                # Deliberately do not inspect the result or infer whether the write committed.
                return None
            if "error" in reply:
                raise SmokeFailure(f"MCP {method}: {reply['error']}")
            return reply.get("result", {})

    def tool(self, name, arguments=None, discard=False):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments or {}}, discard=discard)
        if discard:
            return None, []
        content = result.get("content", [])
        messages = [block.get("text", "") for block in content if block.get("type") == "text"]
        if result.get("isError"):
            raise SmokeFailure(f"{name}: {'; '.join(messages)}")
        metadata = result.get("structuredContent")
        parsed = "structuredContent" in result
        for message in reversed(messages):
            try:
                metadata = json.loads(message)
                parsed = True
                break
            except json.JSONDecodeError:
                pass
        require(parsed, f"{name} did not return JSON metadata: {messages}")
        images = [block for block in content if block.get("type") == "image"]
        return metadata, images

    def command(self, command, params=None):
        return self.tool("command_run", {"id": command, "params": params or {}})[0]

    def close(self):
        try:
            if self.process.stdin:
                self.process.stdin.close()
            self.process.wait(timeout=5)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        finally:
            self.requests.close()
            self.stderr.close()


def save_tool(output, prefix, result):
    metadata, images = result
    paths = []
    for index, image in enumerate(images, 1):
        require(image.get("mimeType") == "image/png", f"{prefix}: unexpected image type")
        png = base64.b64decode(image["data"], validate=True)
        require(png.startswith(b"\x89PNG\r\n\x1a\n"), f"{prefix}: malformed PNG")
        path = output / f"{prefix}-{index:02d}.png"
        path.write_bytes(png)
        paths.append(path.name)
    write_json(output / f"{prefix}.json", {"metadata": metadata, "imageFilesInContentOrder": paths})
    return metadata


def assert_online(value):
    if isinstance(value, dict):
        if "offlineMedia" in value:
            require(not value["offlineMedia"], f"Render/export used offline media: {value['offlineMedia']}")
        for child in value.values():
            assert_online(child)
    elif isinstance(value, list):
        for child in value:
            assert_online(child)


def apply_args(plan, session, operation):
    return {"plan": plan, "session_id": session, "operation_id": operation, "idempotency_key": operation}


def run_workflow(args, output, report):
    external_log = output / "external-stderr.log"
    source_probe = ffprobe(args.ffprobe, args.source, args.timeout, external_log)
    write_json(output / "source-ffprobe.json", source_probe)
    stream = video_stream(source_probe)
    require(stream is not None, "Source has no video stream")
    rate = Fraction(stream.get("avg_frame_rate") or stream.get("r_frame_rate") or "0/1")
    require(rate > 0, "Source nominal frame rate is unavailable")
    source_duration = Fraction(str(stream.get("duration") or source_probe.get("format", {}).get("duration") or "0"))
    requested = Fraction(str(args.duration))
    require(source_duration > 0, "Source duration is unavailable")
    frames = int(min(source_duration, requested) * rate)
    require(frames >= 12, "At least 12 source frames are needed for two synthetic range removals")
    duration = frame_tick(frames, rate)
    has_audio = any(item.get("codec_type") == "audio" for item in source_probe.get("streams", []))
    report["sourceMetadata"] = {"width": stream["width"], "height": stream["height"],
                                "nominalFrameRate": str(rate), "color": {key: stream.get(key) for key in
                                ("color_space", "color_transfer", "color_primaries", "color_range")}}
    report["testMeaning"] = {"cameras": "Two tracks intentionally reuse the same original source. Camera B receives a synthetic exposure offset.",
                             "cuts": "Two synthetic frame-range removals test linked edits; no speech/dialogue recognition is claimed.",
                             "lostReply": "The client discards the apply response, recovers its receipt, then replays identical arguments. TCP reconnect loss is a separate bridge regression test."}
    client = McpClient(args.binary, output, args.timeout)
    try:
        report["server"] = client.initialization
        tools = client.rpc("tools/list", {})
        names = {tool["name"] for tool in tools.get("tools", [])}
        required = {"agent_session", "plan_edit", "preview_plan", "apply_plan", "get_operation", "render_frames",
                    "scopes_read", "grade_get", "inspect", "media_import", "command_run"}
        require(required <= names, f"Binary is missing workflow tools: {sorted(required - names)}")
        write_json(output / "tools.json", tools)
        print("Importing original footage (read-only)", flush=True)
        imported = client.tool("media_import", {"paths": [str(args.source)]})[0]
        require(not imported.get("errors"), f"Import errors: {imported.get('errors')}")
        require(len(imported.get("items", [])) == 1, f"Expected one imported item: {imported}")
        item = imported["items"][0]
        session = client.tool("agent_session")[0]
        session_id = session["sessionId"]
        ref = lambda step, path: {"$step": step, "path": path}
        steps = [
            {"id": "file.newSequence", "params": {"name": "Two-camera workflow test (same source)",
                "width": stream["width"], "height": stream["height"], "frameRate": {"num": rate.numerator, "den": rate.denominator},
                "video": 2, "audio": 1 if has_audio else 0}},
            {"id": "timeline.place", "params": {"item": item, "track": "V1", "frame": 0, "duration": duration, "audio": has_audio}},
            {"id": "timeline.place", "params": {"item": item, "track": "V2", "frame": 0, "duration": duration, "audio": False}},
            {"id": "clip.rename", "params": {"clip": ref(1, "/clips/0"), "name": "Camera A — same-source test hero"}},
            {"id": "clip.rename", "params": {"clip": ref(2, "/clips/0"), "name": "Camera B — synthetic exposure offset"}},
            {"id": "grade.patch", "params": {"clip": ref(2, "/clips/0"), "effectInstance": "new",
                "params": {"exposure": {"value": {"Float": -0.8}}}}},
        ]
        setup = client.tool("plan_edit", {"label": "Set up two-camera smoke fixture", "base_revision": session["revision"], "steps": steps})[0]
        write_json(output / "setup-plan.json", setup)
        camera_a = setup["steps"][1]["clips"][0]
        camera_b = setup["steps"][2]["clips"][0]
        sequence = setup["steps"][0]["sequence"]
        setup_receipt = client.tool("apply_plan", apply_args(setup["plan"], session_id, "setup-" + uuid.uuid4().hex))[0]
        require(setup_receipt.get("status") == "committed", f"Setup apply failed: {setup_receipt}")
        write_json(output / "setup-receipt.json", setup_receipt)
        baseline = client.tool("sequence_inspect")[0]
        history_before = client.command("history.list")
        revision = client.tool("agent_session")[0]["revision"]
        write_json(output / "baseline-sequence.json", baseline)
        grade_before = client.tool("grade_get", {"clip": camera_b, "sequence": sequence})[0]
        write_json(output / "baseline-grade.json", grade_before)
        source_image = save_tool(output, "original-source", client.tool("render_frames", {
            "target": "source", "item": item, "frames": [0], "max_side": args.max_side}))
        assert_online(source_image)
        before_scopes = save_tool(output, "before-scopes", client.tool("scopes_read", {
            "target": "clip", "sequence": sequence, "clip": camera_b, "frames": [0], "effects": "post",
            "roi": {"x": 0.1, "y": 0.1, "width": 0.8, "height": 0.8},
            "reference": {"target": "clip", "sequence": sequence, "clip": camera_a, "frame": 0},
            "scopes": ["waveform", "histogram", "vectorscopeYuv"], "bins": False, "images": True}))
        assert_online(before_scopes)
        cut_length = max(1, min(3, frames // 12))
        cuts = [(frames // 3, cut_length), (2 * frames // 3, cut_length)]
        # Remove later ranges first, so both requested positions retain original-timeline meaning.
        proposal_steps = [{"id": "grade.matchBatch", "params": {
            "reference": {"clip": camera_a, "sequence": sequence},
            "targets": [{"clip": camera_b, "sequence": sequence, "effectInstance": "lumetri:0"}],
            "mode": "exposure", "samplesPerClip": args.samples, "minConfidence": 0.0, "skinProtection": False}}]
        for track in baseline.get("video", []) + baseline.get("audio", []):
            proposal_steps.append({"id": "timeline.setTargeting", "params": {"track": track["id"], "targeted": True}})
        for start, length in reversed(cuts):
            proposal_steps.extend([
                {"id": "markers.markIn", "params": {"frame": start, "target": "program"}},
                {"id": "markers.markOut", "params": {"frame": start + length - 1, "target": "program"}},
                {"id": "sequence.extract", "params": {}},
            ])
        print("Staging multi-sample grade match and two synthetic cuts", flush=True)
        proposal = client.tool("plan_edit", {"label": "Match Camera B + two synthetic range cuts", "base_revision": revision,
                                              "steps": proposal_steps})[0]
        write_json(output / "proposal.json", proposal)
        plan = proposal["plan"]
        match_result = proposal["steps"][0]
        report["match"] = match_result
        for match in match_result.get("results", []):
            if match.get("status") == "matched":
                require(match["distanceAfter"] < match["distanceBefore"], f"Match worsened measured distance: {match}")
        require(client.tool("agent_session")[0]["revision"] == revision, "Planning changed live revision")
        require(sequence_structure(client.tool("sequence_inspect")[0]) == sequence_structure(baseline), "Planning changed live timeline")
        require(client.command("history.list") == history_before, "Planning changed undo history")
        preview = client.tool("preview_plan", {"plan": plan})[0]
        write_json(output / "proposal-preview.json", preview)
        preview_frames = sorted({0, max(1, frames // 4), max(2, frames // 2)})
        visual = save_tool(output, "before-after-contact-sheet", client.tool("render_frames", {
            "target": "sequence", "sequence": sequence, "frames": preview_frames, "max_side": args.max_side,
            "plan": plan, "comparison": True, "contact_sheet": True}))
        assert_online(visual)
        pair = save_tool(output, "before-after-clip", client.tool("render_frames", {
            "target": "clip", "sequence": sequence, "clip": camera_b, "frames": [0], "max_side": args.max_side,
            "plan": plan, "comparison": True}))
        assert_online(pair)
        after_scopes = save_tool(output, "proposed-scopes", client.tool("scopes_read", {
            "target": "clip", "sequence": sequence, "clip": camera_b, "frames": [0], "effects": "post", "plan": plan,
            "roi": {"x": 0.1, "y": 0.1, "width": 0.8, "height": 0.8},
            "reference": {"target": "clip", "sequence": sequence, "clip": camera_a, "frame": 0},
            "scopes": ["waveform", "histogram", "vectorscopeYuv"], "bins": False, "images": True}))
        assert_online(after_scopes)
        print("Discarding apply reply, recovering receipt, replaying once, checking undo/redo", flush=True)
        operation = "approval-" + uuid.uuid4().hex
        approval = apply_args(plan, session_id, operation)
        client.tool("apply_plan", approval, discard=True)
        receipt = client.tool("get_operation", {"operation_id": operation, "session_id": session_id})[0]
        require(receipt.get("status") == "committed", f"Lost-reply recovery failed: {receipt}")
        require(receipt.get("result", {}).get("undoSteps") == 1, f"Apply did not report one undo step: {receipt}")
        after_apply = client.tool("sequence_inspect")[0]
        after_apply_revision = client.tool("agent_session")[0]["revision"]
        history_after = client.command("history.list")
        require(len(history_after["undo"]) == len(history_before["undo"]) + 1, "Apply created the wrong number of undo steps")
        replay = client.tool("apply_plan", approval)[0]
        require(replay.get("replayed") is True, f"Retry was not a receipt replay: {replay}")
        require(replay.get("result") == receipt.get("result"), "Retry changed apply result")
        require(client.tool("agent_session")[0]["revision"] == after_apply_revision, "Retry edited again")
        require(client.command("history.list") == history_after, "Retry created another history entry")
        require(sequence_structure(client.tool("sequence_inspect")[0]) == sequence_structure(after_apply), "Retry changed timeline")
        expected_frames = frames - sum(length for _, length in cuts)
        require(after_apply["durationFrames"] == expected_frames, f"Two cuts did not remove expected frames: {after_apply['durationFrames']} vs {expected_frames}")
        client.command("edit.undo")
        require(sequence_structure(client.tool("sequence_inspect")[0]) == sequence_structure(baseline), "One undo did not restore baseline timeline")
        client.command("edit.redo")
        require(sequence_structure(client.tool("sequence_inspect")[0]) == sequence_structure(after_apply), "Redo did not restore approved timeline")
        report["approval"] = {"receipt": receipt, "replay": replay, "undoStepsMeasured": 1, "undoRestoredBaseline": True,
                              "redoRestoredApprovedTimeline": True, "cutsOriginalFrames": [{"start": start, "frames": length} for start, length in cuts],
                              "expectedRemainingFrames": expected_frames}
        write_json(output / "approval-receipts.json", report["approval"])
        print("Checking stale-plan refusal", flush=True)
        stale = client.tool("plan_edit", {"label": "Must be rejected after human edit", "steps": [
            {"id": "clip.rename", "params": {"clip": camera_b, "name": "This stale rename must never land"}}]})[0]
        client.command("clip.rename", {"clip": camera_a, "name": "Camera A — retained independent edit"})
        before_rejection = client.tool("sequence_inspect")[0]
        history_rejection = client.command("history.list")
        rejected = client.tool("apply_plan", apply_args(stale["plan"], session_id, "stale-" + uuid.uuid4().hex))[0]
        require(rejected.get("status") == "failed" and "stale_plan" in rejected.get("error", ""), f"Stale plan was not rejected: {rejected}")
        require(client.command("history.list") == history_rejection, "Stale rejection changed history")
        require(sequence_structure(client.tool("sequence_inspect")[0]) == sequence_structure(before_rejection), "Stale plan altered human edit")
        client.tool("discard_plan", {"plan": stale["plan"]})
        report["stalePlan"] = {"rejected": True, "receipt": rejected, "historyUnchanged": True, "humanEditPreserved": True}
        final_sequence = client.tool("inspect", {"sequence": sequence})[0]
        write_json(output / "final-timeline.json", final_sequence)
        project_path = output / "Two Camera Workflow Test.fcproj"
        client.command("file.saveAs", {"path": str(project_path)})
        report["project"] = str(project_path)
        export_duration = min(args.export_seconds, final_sequence["duration"] / TICKS_PER_SECOND)
        require(export_duration > 0, "Final timeline is empty")
        export_path = output / "workflow-proxy.mp4"
        export_request = {"path": str(export_path), "sequence": sequence, "format": "h264", "width": 640, "height": 360,
                          "fps": 30, "sdr": True, "hardwareEncoding": "auto", "range": "custom", "startSeconds": 0,
                          "endSeconds": export_duration, "audio": has_audio, "wait": True}
        print(f"Exporting {export_duration:.3f}s SDR proxy with FilmCraft", flush=True)
        exported = client.command("file.exportMedia", export_request)
        assert_online(exported)
        require(export_path.is_file() and export_path.stat().st_size > 0, "Export produced no file")
        report["export"] = {"request": export_request, "result": exported, "path": str(export_path)}
        write_json(output / "export-request-and-result.json", report["export"])
    finally:
        client.close()
    print("Reopening the saved project and exported file", flush=True)
    reopened = run_json([str(args.binary), "--project", report["project"], "exec", "agent.inspect", json.dumps({"sequence": sequence})], args.timeout, external_log)
    require(sequence_structure(reopened) == sequence_structure(final_sequence), "Saved/reopened timeline differs")
    write_json(output / "reopened-project.json", reopened)
    # The proxy is small, so the CLI's full-file media probe has bounded memory here.
    filmcraft_probe = run_json([str(args.binary), "probe", str(export_path)], args.timeout, external_log)
    write_json(output / "export-filmcraft-probe.json", filmcraft_probe)
    proxy_probe = ffprobe(args.ffprobe, export_path, args.timeout, external_log)
    write_json(output / "export-ffprobe.json", proxy_probe)
    video = video_stream(proxy_probe)
    require(video is not None, "Export has no video stream")
    actual_rate = Fraction(video.get("avg_frame_rate") or video.get("r_frame_rate") or "0/1")
    actual_duration = float(video.get("duration") or proxy_probe.get("format", {}).get("duration") or "0")
    expected_duration = math.ceil(export_duration * 30 - 1e-9) / 30
    require((video.get("width"), video.get("height")) == (640, 360), "Export dimensions differ from delivery request")
    require(actual_rate == Fraction(30, 1), f"Export frame rate differs: {actual_rate}")
    require(abs(actual_duration - expected_duration) <= 1 / 30 + 1e-6, f"Export duration differs: {actual_duration} vs {expected_duration}")
    require(bool(filmcraft_probe.get("video")), "FilmCraft could not reopen exported video")
    require(any(s.get("codec_type") == "audio" for s in proxy_probe.get("streams", [])) == has_audio, "Export audio presence differs")
    # Verify pixels too, rather than trusting a container-only probe.
    reopened_client = McpClient(args.binary, output, args.timeout)
    try:
        proxy_import = reopened_client.tool("media_import", {"paths": [str(export_path)]})[0]
        require(not proxy_import.get("errors"), f"FilmCraft cannot import its export: {proxy_import}")
        proxy_item = proxy_import["items"][0]
        decoded = save_tool(output, "reopened-export-frame", reopened_client.tool("render_frames", {
            "target": "source", "item": proxy_item, "frames": [0], "max_side": 640}))
        assert_online(decoded)
    finally:
        reopened_client.close()
    report["verification"] = {"savedProjectReopenedExactly": True, "exportReopenedAndDecodedByFilmCraft": True,
        "dimensions": [video["width"], video["height"]], "frameRate": str(actual_rate),
        "durationSeconds": actual_duration, "expectedDurationSeconds": expected_duration,
        "audioExpected": has_audio, "audioVerified": has_audio,
        "colorTags": {key: video.get(key) for key in ("color_space", "color_transfer", "color_primaries", "color_range")},
        "captionStreams": [s for s in proxy_probe.get("streams", []) if s.get("codec_type") == "subtitle"],
        "delivery": "640x360, 30 fps, SDR H.264 short proxy; original timeline retains native size/rate",
        "nativeTimeline": {"width": stream["width"], "height": stream["height"], "frameRate": str(rate)}}
    # A requested SDR proxy must never silently retain source PQ/HLG output tags.
    require(video.get("color_transfer") not in ("smpte2084", "arib-std-b67"), "SDR proxy retained HDR transfer tags")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True, help="Parent of a new unique run directory; existing files are never overwritten")
    parser.add_argument("--ffprobe", default=shutil.which("ffprobe"))
    parser.add_argument("--duration", type=float, default=1.0, help="Seconds of source used in the native test timeline (default 1)")
    parser.add_argument("--export-seconds", type=float, default=0.25, help="Short proxy duration; keeps the real-8K smoke run bounded")
    parser.add_argument("--max-side", type=int, default=960)
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=1200.0, help="Per-call timeout in seconds")
    args = parser.parse_args()
    args.binary, args.source, args.output_dir = args.binary.resolve(), args.source.resolve(), args.output_dir.resolve()
    for name in ("duration", "export_seconds", "timeout"):
        require(math.isfinite(getattr(args, name)) and getattr(args, name) > 0, f"{name} must be finite and positive")
    require(args.binary.is_file(), f"Binary is missing: {args.binary}")
    require(args.source.is_file(), f"Source is missing: {args.source}")
    require(args.ffprobe is not None, "ffprobe must be installed or supplied with --ffprobe")
    require(1 <= args.samples <= 8 and 64 <= args.max_side <= 1920, "samples must be 1..8 and max-side 64..1920")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = args.output_dir / f"agent-workflow-{stamp}-{uuid.uuid4().hex[:8]}"
    output.mkdir(exist_ok=False)
    print(f"Artifacts: {output}", flush=True)
    before = source_identity(args.source)
    report = {"status": "running", "startedUtc": stamp, "source": str(args.source), "binary": str(args.binary),
              "outputDirectory": str(output), "sourceBefore": before}
    started = time.monotonic()
    exit_code = 0
    try:
        run_workflow(args, output, report)
        report["status"] = "passed"
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
        report["traceback"] = traceback.format_exc()
        exit_code = 1
    finally:
        try:
            after = source_identity(args.source)
            report["sourceAfter"] = after
            report["sourcePreserved"] = before == after
            if before != after:
                report["status"] = "failed"
                report["sourcePreservationError"] = "Source identity changed while the test ran"
                exit_code = 1
        except Exception as error:
            report["sourcePreserved"] = False
            report["sourcePreservationError"] = str(error)
            report["status"] = "failed"
            exit_code = 1
        report["elapsedSeconds"] = round(time.monotonic() - started, 3)
        write_json(output / "report.json", report)
    print(json.dumps({"status": report["status"], "report": str(output / "report.json"),
                      "sourcePreserved": report.get("sourcePreserved"), "error": report.get("error")}, indent=2))
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
