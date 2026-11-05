#!/usr/bin/env python3
"""Audit real FilmCraft decoding and exports with small synthetic media fixtures.

Requires the adjacent test-agent-workflow.py, Python3 stdlib, external ffmpeg/ffprobe and a
FilmCraft CLI binary. ffmpeg is only a fixture generator and independent test oracle.
Results apply to the generated profiles, not every possible stream in a file extension.
Unsupported streams and unavailable fixture encoders are recorded separately. This audit
never opens user footage and never bundles, links or ships ffmpeg with FilmCraft.
"""
from __future__ import annotations

import argparse
import array
import datetime
import importlib.util
import json
import math
from pathlib import Path
import shutil
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("workflow_smoke", Path(__file__).with_name("test-agent-workflow.py"))
workflow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workflow)


def case(name, extension, video=None, audio=None, *, size="160x96", extra=None, note=None):
    return {"name": name, "extension": extension, "videoEncoderArgs": video, "audioEncoderArgs": audio,
            "size": size, "extra": extra or [], "note": note}


def input_cases():
    h264 = ["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"]
    hevc = ["-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p10le", "-x265-params", "pools=1:frame-threads=1:log-level=error"]
    pcm = ["-c:a", "pcm_s16le"]
    aac = ["-c:a", "aac", "-b:a", "96k"]
    cases = [
        case("mp4-h264-aac", "mp4", h264, aac),
        case("mov-h264-pcm", "mov", h264, pcm),
        case("mov-hevc-main10-aac", "mov", hevc + ["-tag:v", "hvc1"], aac),
        case("mkv-hevc-main10-flac", "mkv", hevc, ["-c:a", "flac"]),
        case("webm-vp8-vorbis", "webm", ["-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8"], ["-c:a", "vorbis", "-strict", "-2", "-ac", "2"]),
        case("webm-vp9-opus", "webm", ["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8"], ["-c:a", "libopus"]),
        case("mp4-av1-aac", "mp4", ["-c:v", "libsvtav1", "-preset", "12", "-crf", "45", "-svtav1-params", "lp=1", "-pix_fmt", "yuv420p"], aac),
        case("mov-prores422-pcm", "mov", ["-c:v", "prores_ks", "-profile:v", "2", "-pix_fmt", "yuv422p10le"], pcm),
        case("mov-prores4444-pcm", "mov", ["-c:v", "prores_ks", "-profile:v", "4", "-pix_fmt", "yuva444p10le"], pcm),
        case("mov-dnxhr-lb-pcm", "mov", ["-c:v", "dnxhd", "-profile:v", "dnxhr_lb", "-pix_fmt", "yuv422p"], pcm, size="320x180"),
        case("avi-mjpeg-pcm", "avi", ["-c:v", "mjpeg", "-q:v", "3", "-pix_fmt", "yuvj422p"], pcm),
        case("avi-bgr24-pcm", "avi", ["-c:v", "rawvideo", "-pix_fmt", "bgr24"], pcm),
        case("ts-h264-aac", "ts", h264, aac, extra=["-f", "mpegts"]),
        case("ts-mpeg2-mp2", "ts", ["-c:v", "mpeg2video", "-pix_fmt", "yuv420p"], ["-c:a", "mp2"], extra=["-f", "mpegts"]),
        case("mpg-mpeg1-mp2", "mpg", ["-c:v", "mpeg1video", "-pix_fmt", "yuv420p"], ["-c:a", "mp2", "-ar", "44100"], extra=["-f", "mpeg"]),
        case("mxf-mpeg2-pcm", "mxf", ["-c:v", "mpeg2video", "-pix_fmt", "yuv422p", "-g", "1", "-b:v", "50000000"], ["-c:a", "pcm_s16le", "-ar", "48000"], size="720x576"),
        case("y4m-420", "y4m", ["-pix_fmt", "yuv420p", "-f", "yuv4mpegpipe"]),
        case("mpeg2-elementary", "m2v", ["-c:v", "mpeg2video", "-f", "mpeg2video"]),
        case("wav-pcm16", "wav", audio=pcm),
        case("wav-float32", "wav", audio=["-c:a", "pcm_f32le"]),
        case("aiff-pcm24", "aiff", audio=["-c:a", "pcm_s24be"]),
        case("mp3", "mp3", audio=["-c:a", "libmp3lame", "-b:a", "128k"]),
        case("flac", "flac", audio=["-c:a", "flac"]),
        case("aac-adts", "aac", audio=aac, extra=["-f", "adts"]),
        case("m4a-aac", "m4a", audio=aac),
        case("m4a-alac", "m4a", audio=["-c:a", "alac"]),
        case("ogg-vorbis", "ogg", audio=["-c:a", "vorbis", "-strict", "-2", "-ac", "2"]),
        case("ogg-opus", "opus", audio=["-c:a", "libopus"]),
        case("ac3-elementary", "ac3", audio=["-c:a", "ac3"], extra=["-f", "ac3"]),
        case("eac3-elementary", "eac3", audio=["-c:a", "eac3"], extra=["-f", "eac3"]),
        case("eac3-mp4", "mp4", audio=["-c:a", "eac3"]),
        case("eac3-mkv", "mkv", audio=["-c:a", "eac3"]),
        case("eac3-ts", "ts", audio=["-c:a", "eac3"], extra=["-f", "mpegts"]),
        case("asf-wmv-wma", "asf", ["-c:v", "wmv2"], ["-c:a", "wmav2"], note="Expected current limitation: no ASF/WMV/WMA importer"),
        case("mkv-ffv1-flac", "mkv", ["-c:v", "ffv1"], ["-c:a", "flac"], note="Expected current limitation: no FFV1 decoder"),
        case("ts-dts", "ts", audio=["-c:a", "dca", "-strict", "-2"], extra=["-f", "mpegts"], note="Expected current limitation: no DTS decoder"),
    ]
    for extension, encoder in [("png", "png"), ("jpg", "mjpeg"), ("tif", "tiff"), ("bmp", "bmp"),
                                ("webp", "libwebp"), ("tga", "targa"), ("dpx", "dpx"), ("exr", "exr"), ("gif", "gif")]:
        cases.append(case("still-" + extension, extension, ["-c:v", encoder, "-frames:v", "1"], extra=["-update", "1"] if extension != "gif" else []))
    return cases


def generate(args, item, output):
    command = [args.ffmpeg, "-hide_banner", "-loglevel", "error", "-y", "-threads", "1"]
    if item["videoEncoderArgs"] is not None:
        command += ["-f", "lavfi", "-i", "testsrc2=size=" + item["size"] + ":rate=25"]
    if item["audioEncoderArgs"] is not None:
        command += ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]
    command += ["-t", "0.4"]
    if item["videoEncoderArgs"] is not None:
        command += item["videoEncoderArgs"]
    else:
        command += ["-vn"]
    if item["audioEncoderArgs"] is not None:
        command += item["audioEncoderArgs"]
    else:
        command += ["-an"]
    command += ["-threads", "1"] + item["extra"] + [str(output)]
    process = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=args.timeout)
    return process.returncode == 0 and output.is_file(), {"command": command, "exitCode": process.returncode, "stderr": process.stderr[-5000:]}


def raw_rgb(args, path, size=None):
    command = [args.ffmpeg, "-v", "error", "-i", str(path)]
    if size:
        command += ["-vf", f"scale={size[0]}:{size[1]}:flags=bilinear"]
    command += ["-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "-"]
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=args.timeout)
    workflow.require(result.returncode == 0, f"Oracle could not decode image: {result.stderr.decode(errors='replace')[-1000:]}")
    return result.stdout


def check_video(args, client, item_id, path, directory, still):
    result = client.tool("render_frames", {"target": "source", "item": item_id, "frames": [0] if still else [0, 3], "max_side": 160})
    metadata = workflow.save_tool(directory, "decoded-video", result)
    workflow.assert_online(metadata)
    png = directory / "decoded-video-01.png"
    header = png.read_bytes()[:24]
    size = (int.from_bytes(header[16:20], "big"), int.from_bytes(header[20:24], "big"))
    ours = raw_rgb(args, png)
    reference = raw_rgb(args, path, size)
    workflow.require(len(ours) == len(reference) and len(ours) > 0, "Oracle and FilmCraft image sizes differ")
    mae = sum(abs(a - b) for a, b in zip(ours, reference)) / len(ours)
    # This is a decode/display smoke check, not the bit-exact codec oracle suite. Resizing and
    # display transforms can differ. A slate/black fallback must not count as a decoded pattern.
    workflow.require(max(ours) - min(ours) > 20 and mae < 45.0, f"Rendered pattern does not resemble oracle (MAE {mae:.3f})")
    return {"status": "verified_decode", "framesRequested": [0] if still else [0, 3], "renderSize": size,
            "oracleDisplayMeanAbsoluteError8bit": round(mae, 4), "criterion": "non-flat image, no offline slate, display MAE<45; not bit-exact certification"}


def check_audio(args, client, sequence, clips, directory):
    workflow.require(bool(clips), "Imported stream did not create an audio clip")
    peak = client.command("clip.audioPeak", {"clips": clips})
    peak_db = peak.get("peakDb")
    workflow.require(isinstance(peak_db, (int, float)) and math.isfinite(peak_db) and -80 < peak_db < 1,
                     f"Decoded generated tone is absent/silent: {peak}")
    path = directory / "decoded-audio.wav"
    exported = client.command("file.exportMedia", {"path": str(path), "format": "wav", "sequence": sequence,
        "range": "custom", "startSeconds": 0, "endSeconds": 0.2, "wait": True})
    workflow.assert_online(exported)
    oracle = subprocess.run([args.ffmpeg, "-v", "error", "-i", str(path), "-map", "0:a:0", "-t", "0.2",
                             "-ac", "1", "-ar", "48000", "-f", "f32le", "-"], stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=args.timeout)
    workflow.require(oracle.returncode == 0 and len(oracle.stdout) > 4, "Decoded audio export cannot be reopened")
    samples = array.array("f")
    samples.frombytes(oracle.stdout[:len(oracle.stdout) // 4 * 4])
    if sys.byteorder != "little":
        samples.byteswap()
    rms = math.sqrt(sum(float(v) ** 2 for v in samples) / len(samples))
    workflow.require(math.isfinite(rms) and rms > 0.001, f"Decoded audio export is silent/nonfinite: {rms}")
    return {"status": "verified_decode_and_pcm_export", "sourcePeakDb": peak_db, "exportRms": rms,
            "sampleCount": len(samples), "sampleRate": 48000, "exportResult": exported}


def check_input(args, client, item, output):
    directory = output / item["name"]
    directory.mkdir()
    path = directory / ("fixture." + item["extension"])
    record = {"case": item["name"], "fixture": str(path), "note": item["note"]}
    generated, generation = generate(args, item, path)
    record["generation"] = generation
    if not generated:
        record["status"] = "fixture_unavailable"
        return record
    probe = workflow.ffprobe(args.ffprobe, path, args.timeout, directory / "oracle-stderr.log")
    workflow.write_json(directory / "ffprobe.json", probe)
    streams = probe.get("streams", [])
    record["streams"] = [{key: stream.get(key) for key in ("codec_type", "codec_name", "profile", "pix_fmt", "width", "height", "sample_rate", "channels")}
                         for stream in streams]
    client.command("file.newProject", {"name": item["name"]})
    try:
        imported = client.tool("media_import", {"paths": [str(path)]})[0]
        workflow.require(imported.get("items") and not imported.get("errors"), f"Import failed: {imported}")
        item_id = imported["items"][0]
        video = workflow.video_stream(probe)
        audio = any(stream.get("codec_type") == "audio" for stream in streams)
        sequence = client.command("file.newSequence", {"name": item["name"], "width": 160, "height": 96, "fps": 25,
                                                       "video": 1 if video else 0, "audio": 1 if audio else 0})["sequence"]
        placed = client.command("timeline.place", {"item": item_id, "frame": 0, "duration": {"seconds": 0.24},
                                                   "video": bool(video), "audio": audio})
        sequence_info = client.tool("sequence_inspect")[0]
        workflow.write_json(directory / "filmcraft-sequence.json", sequence_info)
        if video:
            try:
                record["video"] = check_video(args, client, item_id, path, directory, item["name"].startswith("still-"))
            except Exception as error:
                record["video"] = {"status": "unsupported_or_failed", "error": str(error)}
        if audio:
            clips = [clip["clip"] for track in sequence_info.get("audio", []) for clip in track.get("items", [])]
            try:
                record["audio"] = check_audio(args, client, sequence, clips, directory)
            except Exception as error:
                record["audio"] = {"status": "unsupported_or_failed", "error": str(error)}
        statuses = [record[key]["status"].startswith("verified") for key in ("video", "audio") if key in record]
        record["status"] = "verified" if statuses and all(statuses) else "partial" if any(statuses) else "unsupported_or_failed"
    except Exception as error:
        record["status"] = "unsupported_or_failed"
        record["error"] = str(error)
    workflow.write_json(directory / "result.json", record)
    return record


def check_exports(args, client, source, output):
    client.command("file.newProject", {"name": "Export compatibility"})
    imported = client.tool("media_import", {"paths": [str(source)]})[0]
    workflow.require(imported.get("items") and not imported.get("errors"), f"Cannot import export baseline: {imported}")
    sequence = client.command("file.newSequence", {"name": "Export compatibility", "width": 320, "height": 180, "fps": 25, "video": 1, "audio": 1})["sequence"]
    client.command("timeline.place", {"item": imported["items"][0], "frame": 0, "duration": {"seconds": 0.12}})
    formats = [("h264", "mp4"), ("hevc", "mp4"), ("av1", "mp4"), ("prores", "mov"), ("dnxhr", "mov"), ("apv", "mov"), ("mjpeg", "mov"),
               ("mxf-op1a", "mxf"), ("mxf-opatom", "mxf"), ("wav", "wav"), ("aiff", "aiff"), ("aac", "m4a"), ("flac", "flac"),
               ("png", "png"), ("tiff", "tif"), ("bmp", "bmp"), ("jpg", "jpg"), ("tga", "tga"), ("dpx", "dpx"), ("exr", "exr"), ("gif", "gif")]
    records = []
    for format_name, extension in formats:
        directory = output / ("export-" + format_name)
        directory.mkdir()
        path = directory / ("export." + extension)
        record = {"format": format_name, "requestedPath": str(path)}
        print("Export audit: " + format_name, flush=True)
        try:
            result = client.command("file.exportMedia", {"path": str(path), "format": format_name, "sequence": sequence,
                "range": "entire", "hardwareEncoding": "auto", "sdr": True, "wait": True})
            workflow.assert_online(result)
            written = Path(result["path"])
            workflow.require(written.is_file() and written.stat().st_size > 0, "Export file is missing")
            probe = workflow.ffprobe(args.ffprobe, written, args.timeout, directory / "oracle-stderr.log")
            workflow.write_json(directory / "ffprobe.json", probe)
            record["result"] = result
            record["streams"] = [{key: stream.get(key) for key in ("codec_type", "codec_name", "profile", "pix_fmt", "width", "height", "sample_rate", "channels")}
                                 for stream in probe.get("streams", [])]
            # ffprobe alone is insufficient: make the independent oracle decode every output stream.
            decode = subprocess.run([args.ffmpeg, "-v", "error", "-i", str(written), "-f", "null", "-"],
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=args.timeout)
            workflow.require(decode.returncode == 0, "Oracle decode failed: " + decode.stderr.decode(errors="replace")[-2000:])
            extra_files = result.get("result", {}).get("extra_files", [])
            record["extraFilesVerified"] = []
            for extra in extra_files:
                extra_path = Path(extra)
                extra_probe = workflow.ffprobe(args.ffprobe, extra_path, args.timeout, directory / "oracle-stderr.log")
                extra_decode = subprocess.run([args.ffmpeg, "-v", "error", "-i", str(extra_path), "-f", "null", "-"],
                                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=args.timeout)
                workflow.require(extra_decode.returncode == 0, "Oracle could not decode sidecar stream: " + str(extra_path))
                record["extraFilesVerified"].append({"path":str(extra_path),"streams":extra_probe.get("streams",[])})
            if result.get("lastPath"):
                workflow.require(Path(result["lastPath"]).is_file(), "Last image-sequence output is missing")
            record["status"] = "verified_export_and_oracle_decode"
        except Exception as error:
            record["status"] = "unsupported_or_failed"
            record["error"] = str(error)
        workflow.write_json(directory / "result.json", record)
        records.append(record)
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--ffmpeg", default=shutil.which("ffmpeg"))
    parser.add_argument("--ffprobe", default=shutil.which("ffprobe"))
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--filter", help="Only import fixture names containing this text (exports omitted)")
    parser.add_argument("--skip-exports", action="store_true")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    workflow.require(args.binary.is_file() and args.ffmpeg and args.ffprobe, "Provide an existing CLI and external ffmpeg/ffprobe")
    workflow.require(math.isfinite(args.timeout) and args.timeout > 0, "Timeout must be finite and positive")
    args.output_dir = args.output_dir.resolve()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = args.output_dir / ("format-compatibility-" + stamp + "-" + uuid.uuid4().hex[:8])
    output.mkdir()
    print("Artifacts: " + str(output), flush=True)
    report = {"binary": str(args.binary), "imports": [], "exports": [],
              "scope": "Small generated profiles only; does not certify every file/profile in a container family. Probe-only success is not marked verified.",
              "oracles": {"ffmpeg": args.ffmpeg, "ffprobe": args.ffprobe}}
    started = time.monotonic()
    client = workflow.McpClient(args.binary, output, args.timeout)
    try:
        for item in input_cases():
            if args.filter and args.filter not in item["name"]:
                continue
            print("Import audit: " + item["name"], flush=True)
            report["imports"].append(check_input(args, client, item, output))
            workflow.write_json(output / "report.json", report)
        baseline = output / "mp4-h264-aac" / "fixture.mp4"
        if not args.skip_exports and not args.filter and baseline.is_file():
            report["exports"] = check_exports(args, client, baseline, output)
    except Exception as error:
        report["fatalError"] = str(error)
    finally:
        client.close()
        report["elapsedSeconds"] = round(time.monotonic() - started, 3)
        report["summary"] = {kind: {status: sum(item["status"] == status for item in report[kind]) for status in sorted({item["status"] for item in report[kind]})}
                             for kind in ("imports", "exports")}
        workflow.write_json(output / "report.json", report)
    print(json.dumps({"report": str(output / "report.json"), "summary": report["summary"], "fatalError": report.get("fatalError")}, indent=2))
    return int("fatalError" in report)


if __name__ == "__main__":
    sys.exit(main())
