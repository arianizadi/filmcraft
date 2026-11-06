#!/usr/bin/env python3
"""FilmCraft headless audio smoke: generated fixtures, optionally explicit real recording pairs.

FilmCraft performs native import, inspection, previews, edits, synchronization and exports.
ffmpeg/ffprobe are external test oracles and tiny-fixture generators only. No installed app,
source file, model download, network service or third-party decoder source is modified/read.

  python3 scripts/test-audio-workflow.py --cli /path/to/new/filmcraft-cli \
    --output '/path/to/new/audio-workflow-run' [--actual-dir /path/to/recordings]

The output directory must not exist. Optional actual mode reads GX010005.{MP4,WAV} and
GX010006.{MP4,WAV}; it labels stored streams/channels, never a physical mic from a filename.
"""
from __future__ import annotations
import argparse
import array
import base64
import copy
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import random
import shutil
import struct
import subprocess
import sys
import time
import traceback
import uuid

RATE = 48_000
TPS = 254_016_000_000
DURATION = 3
FRAMES = RATE * DURATION
SHIFT = 12_000
MAX_ORACLE_BYTES = 16 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def write_json(path, value):
    with Path(path).open('w') as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')


def required(value, key, context):
    require(isinstance(value, dict) and key in value,
            f'{context}: required API field {key!r} missing; check this CLI includes the new audio API')
    return value[key]


def identity(path):
    st = path.stat()
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(8 * 1024 * 1024), b''):
            digest.update(block)
    return {'path': str(path), 'bytes': st.st_size, 'mtimeNs': st.st_mtime_ns,
            'device': st.st_dev, 'inode': st.st_ino, 'sha256': digest.hexdigest()}


def ticks(samples):
    require(samples * TPS % RATE == 0, 'Requested sample does not have an exact FilmCraft Tick')
    return samples * TPS // RATE


def planar_wav(path, channels, bits=32, floating=True):
    """Original tiny RIFF/WAVE fixtures from the public PCM/IEEE-float layout, stdlib only."""
    count = len(channels)
    frames = len(channels[0])
    require(count in (1, 2, 4) and all(len(c) == frames for c in channels), 'Bad fixture shape')
    require((bits, floating) in ((24, False), (32, True)), 'Only PCM24/float32 fixtures supported')
    align = count * bits // 8
    data = bytearray()
    for i in range(frames):
        for channel in channels:
            x = float(channel[i])
            require(math.isfinite(x), 'Non-finite generated sample')
            if floating:
                data.extend(struct.pack('<f', x))
            else:
                value = round(max(-1.0, min(1.0 - 2**-23, x)) * 2**23)
                data.extend(int(value).to_bytes(3, 'little', signed=True))
    fmt = struct.pack('<HHIIHH', 3 if floating else 1, count, RATE, RATE * align, align, bits)
    payload = b'WAVEfmt ' + struct.pack('<I', len(fmt)) + fmt + b'data' + struct.pack('<I', len(data)) + data
    with path.open('xb') as stream:
        stream.write(b'RIFF' + struct.pack('<I', len(payload)) + payload)


def wav_samples(data):
    """Read bounded native audition/export WAV for assertions, including extensible PCM."""
    require(len(data) <= MAX_ORACLE_BYTES and data[:4] == b'RIFF' and data[8:12] == b'WAVE',
            'Expected a bounded RIFF/WAVE audio block')
    offset, fmt, payload = 12, None, None
    while offset + 8 <= len(data):
        name, size = data[offset:offset+4], struct.unpack_from('<I', data, offset+4)[0]
        begin, end = offset+8, offset+8+size
        require(end <= len(data), 'Truncated WAV chunk')
        if name == b'fmt ':
            require(size >= 16, 'Short WAV format chunk')
            code, channels, rate, _, align, bits = struct.unpack_from('<HHIIHH', data, begin)
            if code == 0xFFFE:
                require(size >= 40, 'Short extensible WAV format')
                code = struct.unpack_from('<H', data, begin+24)[0]
            fmt = (code, channels, rate, align, bits)
        elif name == b'data':
            payload = data[begin:end]
        offset = end + (size & 1)
    require(fmt is not None and payload is not None, 'WAV is missing fmt/data')
    code, channels, rate, align, bits = fmt
    require(0 < channels <= 8 and rate == RATE and align > 0 and len(payload) % align == 0,
            f'Unexpected audition WAV dimensions: {fmt}')
    require((code, bits) in ((3, 32), (1, 24), (1, 16), (1, 32)), f'Unsupported smoke WAV format: {fmt}')
    values = [array.array('f') for _ in range(channels)]
    width = bits // 8
    for frame in range(len(payload) // align):
        for ch in range(channels):
            begin = frame * align + ch * width
            if code == 3:
                x = struct.unpack_from('<f', payload, begin)[0]
            else:
                x = int.from_bytes(payload[begin:begin+width], 'little', signed=True) / 2**(bits-1)
            require(math.isfinite(x), 'Non-finite decoded WAV sample')
            values[ch].append(x)
    return {'code': code, 'channels': channels, 'sampleRate': rate, 'bits': bits,
            'frames': len(payload) // align}, values


def rms(values):
    return math.sqrt(math.fsum(x*x for x in values) / max(1, len(values)))


def correlation(a, b):
    require(len(a) == len(b) and len(a) > 0, 'Correlation arrays differ')
    ma, mb = math.fsum(a)/len(a), math.fsum(b)/len(b)
    covariance = math.fsum((x-ma)*(y-mb) for x, y in zip(a, b))
    den = math.sqrt(math.fsum((x-ma)**2 for x in a) * math.fsum((y-mb)**2 for y in b))
    return covariance / den if den > 1e-24 else None


def clip_rows(sequence):
    return {item['clip']: item for kind in ('video', 'audio') for track in sequence[kind] for item in track['items']}


def normalized_sequence(sequence):
    result = copy.deepcopy(sequence)
    for key in ('playhead', 'selection', 'transitionSelection', 'gapSelection', 'revision', 'sessionId', 'documentEpoch'):
        result.pop(key, None)
    return result


def make_client_class():
    sibling = Path(__file__).with_name('test-agent-workflow.py')
    spec = importlib.util.spec_from_file_location('filmcraft_workflow_smoke', sibling)
    require(spec is not None and spec.loader is not None, f'Cannot load MCP helper {sibling}')
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)  # Its CLI runs only under __main__.

    class Client(helper.McpClient):
        def audition(self, params, output, name):
            response = self.rpc('tools/call', {'name': 'audio_preview', 'arguments': params})
            blocks = response.get('content', [])
            messages = [b.get('text', '') for b in blocks if b.get('type') == 'text']
            require(not response.get('isError'), f'audio_preview failed: {messages}')
            metadata = None
            for text in reversed(messages):
                try:
                    metadata = json.loads(text)
                    break
                except json.JSONDecodeError:
                    pass
            require(isinstance(metadata, dict), 'audio_preview returned no JSON metadata')
            audio = [b for b in blocks if b.get('type') == 'audio']
            expected = 2 if params.get('compare') else 1
            require(len(audio) == expected, f'audio_preview expected {expected} native MCP audio blocks; got {len(audio)}')
            arrays = []
            for index, block in enumerate(audio):
                require(block.get('mimeType') == 'audio/wav', 'audio_preview MIME must be audio/wav')
                data = base64.b64decode(required(block, 'data', 'audio block'), validate=True)
                path = output / f'{name}-{index+1:02}.wav'
                with path.open('xb') as stream:
                    stream.write(data)
                shape, planar = wav_samples(data)
                require(shape['code'] == 3 and shape['bits'] == 32 and shape['channels'] == 2,
                        f'audio_preview lost floating stereo format: {shape}')
                arrays.append(planar)
            write_json(output / f'{name}.json', metadata)
            return metadata, arrays
    return Client


class Run:
    def __init__(self, args):
        self.args, self.out = args, args.output.resolve()
        self.out.mkdir(parents=True, exist_ok=False, mode=0o700)
        self.report = {'status': 'running', 'meaning': 'Generated audio/codec/control smoke, plus optional explicitly named real recording pairs. Physical microphone identity is not inferred.',
                       'binary': str(args.cli), 'commands': [], 'fixtures': {}, 'originalsBefore': {}, 'cases': {},
                       'limits': ['Generated PCM/float/MP4 fixtures are not actual DJI hardware recordings.',
                                  'Waveform correlation is not semantic clap recognition; actual-file ambiguous sync requires real user anchors.',
                                  'Short exports and sample windows do not certify long-program loudness, every source profile, or real-time performance.',
                                  'Actual test projects intentionally retain reference and companion clips for review; they are not approved final delivery mixes.']}
        self.clients, self.protected = [], []

    def save(self):
        write_json(self.out / 'report.json', self.report)

    def external(self, args, label, binary=False):
        self.report['commands'].append(args)
        self.save()
        p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=self.args.timeout)
        (self.out / f'{label}.stderr.log').write_bytes(p.stderr)
        require(p.returncode == 0, f'{label} failed (exit {p.returncode}): {p.stderr.decode(errors="replace")[-2000:]}')
        require(len(p.stdout) <= MAX_ORACLE_BYTES, f'{label} oracle output exceeds bound')
        return p.stdout if binary else p.stdout.decode()

    def probe(self, path, label):
        value = json.loads(self.external([self.args.ffprobe, '-v', 'error', '-show_streams', '-show_format', '-of', 'json', str(path)], label))
        write_json(self.out / f'{label}.json', value)
        return value

    def client(self, name):
        output = self.out / name
        output.mkdir(mode=0o700)
        c = make_client_class()(self.args.cli, output, self.args.timeout)
        self.clients.append(c)
        tools = {x['name'] for x in c.rpc('tools/list', {}).get('tools', [])}
        need = {'audio_inspect', 'audio_preview', 'audio_channels_get', 'prepare_audio', 'audio_sync_inspect', 'prepare_audio_sync', 'plan_edit', 'apply_plan'}
        if self.args.mix:
            need |= {'audio_mix_inspect', 'prepare_audio_mix'}
        require(need <= tools, f'CLI lacks new typed audio tools: {sorted(need-tools)}')
        return c, output

    def protect(self, paths):
        for path in paths:
            path = path.resolve()
            require(path.is_file(), f'Missing input {path}')
            if path not in self.protected:
                self.protected.append(path)
                self.report['originalsBefore'][str(path)] = identity(path)
        self.save()

    def inspect(self, c, output, name, subject, **kwargs):
        value = c.tool('audio_inspect', {'subject': subject, **kwargs})[0]
        require(value.get('projectChanged') is False, f'{name}: inspector did not report a read-only result')
        for field in ('channels', 'loudness', 'recommendation', 'sampleRate', 'coverage'):
            required(value, field, name)
        write_json(output / f'{name}.json', value)
        return value

    def apply(self, c, plan, name, require_undo=True):
        session = c.tool('agent_session')[0]['sessionId']
        op = name + '-' + uuid.uuid4().hex
        params = {'plan': plan, 'session_id': session, 'operation_id': op, 'idempotency_key': op}
        receipt = c.tool('apply_plan', params)[0]
        require(receipt.get('status') == 'committed', f'{name}: apply failed: {receipt}')
        require(receipt.get('result', {}).get('undoSteps') in ((1,) if require_undo else (0, 1)), f'{name}: unexpected undo-step count')
        history = c.command('history.list')
        replay = c.tool('apply_plan', params)[0]
        require(replay.get('status') == 'committed' and replay.get('replayed') is True,
                f'{name}: same operation did not replay safely')
        require(history == c.command('history.list'), f'{name}: replay changed history')
        return {'receipt': receipt, 'replay': replay}

    def sequence(self, c, name, audio=4):
        return c.command('file.newSequence', {'name': name, 'width': 160, 'height': 96,
                                              'frameRate': {'num': 24, 'den': 1}, 'video': 1, 'audio': audio})['sequence']

    def import_one(self, c, path):
        value = c.tool('media_import', {'paths': [str(path)]})[0]
        require(not value.get('errors') and len(value.get('items', [])) == 1, f'Native import failed: {value}')
        return value['items'][0]

    def export_wav(self, c, output, name, sequence, seconds, bits, floating):
        path = output / f'{name}.wav'
        request = {'path': str(path), 'sequence': sequence, 'format': 'wav', 'range': 'custom',
                   'startTime': 0, 'endTime': round(seconds*TPS), 'wait': True,
                   'settings': {'audio': {'codec': 'pcm', 'bits': bits, 'float': floating,
                                          'channels': 2, 'sampleRate': RATE}}}
        result = c.command('file.exportMedia', request)
        require(not result.get('offlineMedia'), f'{name}: exporter used unreadable/offline media')
        require(path.is_file(), f'{name}: no output file')
        shape, samples = wav_samples(path.read_bytes())
        require(shape['bits'] == bits and shape['code'] == (3 if floating else 1), f'{name}: wrong native sample format: {shape}')
        require(abs(shape['frames'] - round(seconds*RATE)) <= 1, f'{name}: sample count differs: {shape}')
        probe = self.probe(path, name+'-ffprobe')
        audio = next(s for s in probe['streams'] if s.get('codec_type') == 'audio')
        require(audio.get('codec_name') == ('pcm_f32le' if floating else 'pcm_s24le'), f'{name}: oracle codec mismatch: {audio}')
        require(int(audio['channels']) == 2 and int(audio['sample_rate']) == RATE, f'{name}: oracle channel/rate mismatch')
        raw = self.external([self.args.ffmpeg, '-nostdin', '-v', 'error', '-i', str(path), '-map', '0:a:0', '-f', 'f32le', '-c:a', 'pcm_f32le', '-'], name+'-oracle-decode', True)
        require(len(raw) == shape['frames']*8, f'{name}: oracle decode frame count differs')
        oracle = array.array('f'); oracle.frombytes(raw)
        if sys.byteorder != 'little': oracle.byteswap()
        diff = max((abs(oracle[i*2+ch] - samples[ch][i]) for i in range(shape['frames']) for ch in range(2)), default=0.0)
        require(diff < 2e-6, f'{name}: native WAV and oracle samples differ by {diff}')
        imported = self.import_one(c, path)
        native = self.inspect(c, output, name+'-native-reopen', {'target': 'source', 'item': imported})
        entry = {'request': request, 'result': result, 'path': str(path), 'shape': shape,
                 'ffprobe': probe, 'oracleMaximumDifference': diff, 'nativeReopen': native}
        write_json(output / f'{name}-delivery.json', entry)
        return entry, samples

    def generate(self):
        fixture = self.out / 'generated'; fixture.mkdir(mode=0o700)
        rng, z = random.Random(0xA0D10), 0.0
        base = array.array('f')
        for i in range(4*RATE):
            z = 0.86*z + 0.14*rng.uniform(-1, 1)
            x = z*0.08
            for center, height in ((RATE, 0.95), (int(2.2*RATE), 0.6)):
                delta = i-center
                if delta == 0: x = height
                elif 0 < delta < 960: x += rng.uniform(-1, 1)*height*0.4*math.exp(-delta/160)
            base.append(x)
        zero = array.array('f', [0.0])*FRAMES
        pcm = [array.array('f', (x*0.25 for x in base[:FRAMES])), zero]
        scratch = [array.array('f', (0.002*math.sin(2*math.pi*173*i/RATE) for i in range(FRAMES))),
                   array.array('f', (x*0.08 for x in base[:FRAMES]))]
        hot = [array.array('f', (x*6 for x in base[SHIFT:SHIFT+FRAMES])), zero]
        four = [array.array('f', (gain*math.sin(2*math.pi*hz*i/RATE) for i in range(RATE)))
                for hz, gain in ((220, .10), (330, .13), (550, .16), (770, .20))]
        paths = {name: fixture / name for name in ('mic-pcm24.wav', 'mic-float32.wav', 'scratch.wav', 'independent-four.wav', 'mix-main.wav', 'music-bed.wav', 'two-audio-streams.mp4')}
        planar_wav(paths['mic-pcm24.wav'], pcm, 24, False)
        planar_wav(paths['mic-float32.wav'], hot)
        planar_wav(paths['scratch.wav'], scratch)
        planar_wav(paths['independent-four.wav'], four)
        # Two sustained synthetic activity regions make ducking measurable. These are tones,
        # not speech evidence or a claim that a semantic voice detector recognized dialogue.
        voiced = array.array('f', ((.12*math.sin(2*math.pi*211*i/RATE) + .06*math.sin(2*math.pi*443*i/RATE))
                                  if .4 <= i/RATE < 1.2 or 1.7 <= i/RATE < 2.4 else 0.0 for i in range(FRAMES)))
        planar_wav(paths['mix-main.wav'], [voiced, zero])
        music = [array.array('f', (gain*math.sin(2*math.pi*hz*i/RATE) for i in range(FRAMES)))
                 for gain, hz in ((.035, 239), (.04, 373))]
        planar_wav(paths['music-bed.wav'], music)
        self.external([self.args.ffmpeg, '-nostdin', '-hide_banner', '-loglevel', 'error', '-n',
                       '-threads', '1', '-f', 'lavfi', '-i', 'testsrc2=size=160x96:rate=24',
                       '-i', str(paths['mic-pcm24.wav']), '-i', str(paths['scratch.wav']),
                       '-map', '0:v:0', '-map', '1:a:0', '-map', '2:a:0', '-t', '3',
                       '-c:v', 'libx264', '-preset', 'ultrafast', '-pix_fmt', 'yuv420p',
                       '-c:a', 'aac', '-b:a', '192k', '-movflags', '+faststart',
                       '-metadata:s:a:0', 'handler_name=Generated main stream0',
                       '-metadata:s:a:1', 'handler_name=Generated scratch stream1',
                       str(paths['two-audio-streams.mp4'])], 'generate-two-streams')
        probe = self.probe(paths['two-audio-streams.mp4'], 'generated-two-streams')
        require(len([s for s in probe['streams'] if s.get('codec_type') == 'audio']) == 2, 'Oracle fixture lacks two audio streams')
        self.report['fixtures'] = {'generatedOnly': True, 'paths': {k: str(v) for k, v in paths.items()},
                                   'knownClapSourceSamples': {'camera': RATE, 'shiftedMic': RATE-SHIFT},
                                   'knownMicTimelineShiftSamples': SHIFT, 'physicalHardwareTested': False}
        self.protect(paths.values())
        return paths, pcm, hot, four

    def generated_case(self, paths, pcm, hot, four):
        c, output = self.client('generated-session')
        sequence = self.sequence(c, 'Generated camera/main/scratch/channel test')
        items = {key: self.import_one(c, path) for key, path in paths.items() if key != 'scratch.wav'}
        pcm_read = self.inspect(c, output, 'source-pcm24', {'target': 'source', 'item': items['mic-pcm24.wav']})
        float_read = self.inspect(c, output, 'source-float32', {'target': 'source', 'item': items['mic-float32.wav']})
        require(pcm_read['sourceFormat']['bitDepth'] == 24, 'Native PCM24 bit depth lost')
        require(float_read['sourceFormat']['sampleFormat'] == 'floating_point' and float_read['channels'][0]['peak'] > 1,
                'Native float source lost above-unity headroom')
        require(float_read['channels'][0]['overFullScaleSamples'] > 0, 'Float overload evidence missing')
        placed = c.command('timeline.place', {'item': items['two-audio-streams.mp4'], 'track': 'V1',
                                             'audioTrack': 'A1', 'seconds': 2, 'duration': 3*TPS})
        routing = c.tool('audio_channels_get', {'clips': placed['clips']})[0]['clips']
        require(len(routing) == 2 and {x['stream'] for x in routing} == {0, 1}, 'Native placement did not preserve both source audio streams')
        main = next(x['clip'] for x in routing if x['stream'] == 0)
        scratch = next(x['clip'] for x in routing if x['stream'] == 1)
        initial = c.tool('sequence_inspect')[0]; rows = clip_rows(initial)
        video = next(it['clip'] for t in initial['video'] for it in t['items'])
        mic = c.command('timeline.place', {'item': items['mic-float32.wav'], 'video': False, 'audioTrack': 'A3',
                                          'seconds': 2, 'duration': 3*TPS})['clips'][0]
        alternate = c.command('timeline.place', {'item': items['independent-four.wav'], 'video': False,
                                                'audioTrack': 'A4', 'seconds': 7, 'duration': TPS})['clips'][0]
        baseline = c.tool('sequence_inspect')[0]
        c.command('audio.channels.set', {'clip': main, 'mode': 'left'})
        changed = clip_rows(c.tool('sequence_inspect')[0])
        baseline_rows = clip_rows(baseline)
        require(changed[scratch] == baseline_rows[scratch] and changed[video] == baseline_rows[video],
                'Selected main-channel repair changed scratch audio or video')
        c.command('edit.undo')
        require(normalized_sequence(c.tool('sequence_inspect')[0]) == normalized_sequence(baseline), 'Direct routing undo did not restore baseline')
        mapping_checks = []
        for picks in ([0, 1], [1], [2, 3]):
            c.command('audio.source.set', {'clip': alternate, 'channels': picks})
            report, audio = c.audition({'subject': {'target': 'clip', 'clip': alternate}}, output, 'pair-'+'-'.join(map(str, picks)))
            expected = [four[picks[0]], four[picks[-1]]]
            scores = [correlation(audio[0][ch], expected[ch]) for ch in range(2)]
            require(all(x is not None and x > .9999 for x in scores), f'Explicit source pair {picks} changed channel identity: {scores}')
            mapping_checks.append({'channels': picks, 'correlations': scores})
        streams = c.command('audio.streams.configure', {'clips': [main], 'mainStream': 0, 'muteSecondary': True})
        before = c.tool('sequence_inspect')[0]; before_rows = clip_rows(before)
        require(before_rows[main]['enabled'] and not before_rows[scratch]['enabled'], 'Secondary scratch was not preserved disabled')
        require(before_rows[video] == baseline_rows[video], 'Stream configuration altered video')
        revision = c.tool('agent_session')[0]['revision']
        plan = c.tool('prepare_audio', {'clip': main, 'routing': 'auto', 'normalize': False,
                                       'base_revision': revision, 'label': 'Generated one-sided mic repair'})[0]
        plan_id = required(plan, 'plan', 'prepare_audio')
        pair = c.tool('audio_inspect', {'subject': {'target': 'clip', 'clip': main, 'plan': plan_id}, 'compare': True})[0]
        write_json(output / 'repair-inspection-pair.json', pair)
        _, audio = c.audition({'subject': {'target': 'clip', 'clip': main, 'plan': plan_id}, 'compare': True}, output, 'repair-before-after')
        require(rms(audio[0][1]) < rms(audio[0][0])*0.01, 'Generated main fixture is not clearly one-sided before repair')
        require(correlation(audio[1][0], audio[1][1]) > .9999, 'Proposed left-to-both repair is not centered dual mono')
        require(normalized_sequence(c.tool('sequence_inspect')[0]) == normalized_sequence(before), 'Plan inspection/audition mutated the live timeline')
        approval = self.apply(c, plan_id, 'repair')
        applied = c.tool('sequence_inspect')[0]
        require(clip_rows(applied)[scratch] == before_rows[scratch] and clip_rows(applied)[video] == before_rows[video], 'Approved repair changed scratch/video')
        c.command('edit.undo')
        require(normalized_sequence(c.tool('sequence_inspect')[0]) == normalized_sequence(before), 'One undo did not restore the whole repair')
        c.command('edit.redo')
        require(normalized_sequence(c.tool('sequence_inspect')[0]) == normalized_sequence(applied), 'Redo differs from the approved repair')
        stale = c.tool('prepare_audio', {'clip': main, 'routing': 'stereo', 'normalize': False})[0]['plan']
        c.command('clip.rename', {'clip': main, 'name': 'Retained independent owner edit'})
        state, history = c.tool('sequence_inspect')[0], c.command('history.list')
        session = c.tool('agent_session')[0]['sessionId']; operation = 'stale-'+uuid.uuid4().hex
        rejected = c.tool('apply_plan', {'plan': stale, 'session_id': session, 'operation_id': operation, 'idempotency_key': operation})[0]
        require(rejected.get('status') == 'failed' and 'stale' in rejected.get('error', '').lower(), 'Stale plan was not refused')
        require(history == c.command('history.list') and normalized_sequence(state) == normalized_sequence(c.tool('sequence_inspect')[0]), 'Stale refusal changed state/history')
        c.tool('discard_plan', {'plan': stale})
        # Correlation aligns matching generated source events; a manual fallback uses KNOWN anchors.
        sync_args = {'reference': scratch, 'clip': mic, 'window_seconds': 2.5, 'max_lag_seconds': 1.0, 'mute_reference': False}
        sync = c.tool('audio_sync_inspect', sync_args)[0]
        write_json(output / 'sync-waveform.json', sync)
        manual = False
        if sync.get('status') != 'ready':
            try:
                c.tool('prepare_audio_sync', sync_args)
                raise RuntimeError('Ambiguous waveform unexpectedly staged synchronization')
            except RuntimeError as error:
                require('needsManual' in str(error), f'Unexpected waveform failure: {error}')
            sync_args.update({'method': 'manual', 'reference_time': ticks(RATE), 'clip_time': ticks(RATE-SHIFT)})
            manual = True
        else:
            require(isinstance(sync.get('confidence'), (int, float)) and sync['confidence'] > 0,
                    'Waveform synchronization has no measured confidence')
            require(abs(sync['offsetTicks']-ticks(SHIFT)) <= ticks(64), f'Confident waveform returned the wrong known shift: {sync}')
        protected = {key: clip_rows(c.tool('sequence_inspect')[0])[key] for key in (video, main, scratch)}
        sync_plan = c.tool('prepare_audio_sync', sync_args)[0]['plan']
        sync_approval = self.apply(c, sync_plan, 'synchronize')
        aligned = clip_rows(c.tool('sequence_inspect')[0])
        require(all(aligned[key] == value for key, value in protected.items()), 'Sync changed video, main, or scratch reference')
        camera_anchor = aligned[scratch]['start']+ticks(RATE)-aligned[scratch]['sourceIn']
        mic_anchor = aligned[mic]['start']+ticks(RATE-SHIFT)-aligned[mic]['sourceIn']
        anchor_error = (mic_anchor-camera_anchor)*RATE/TPS
        require(abs(anchor_error) <= (0 if manual else 64), f'Generated clap anchors remain {anchor_error} samples apart')
        require(aligned[mic]['start']*RATE % TPS == 0, 'Sync did not land on an exact48k sample boundary')
        # Decode the actual audition signals and measure the strongest generated clap too.
        _, ref_audio = c.audition({'subject': {'target': 'source', 'item': items['two-audio-streams.mp4']}, 'stream': 1}, output, 'generated-camera-source')
        _, mic_audio = c.audition({'subject': {'target': 'source', 'item': items['mic-float32.wav']}}, output, 'generated-mic-source')
        def peak_near(values, sample):
            lo, hi = sample-RATE//10, sample+RATE//10
            return max(range(lo, hi), key=lambda i: abs(values[i]))
        ref_peak, mic_peak = peak_near(ref_audio[0][1], RATE), peak_near(mic_audio[0][0], RATE-SHIFT)
        actual_error = ((aligned[mic]['start']-aligned[mic]['sourceIn']) - (aligned[scratch]['start']-aligned[scratch]['sourceIn']))*RATE/TPS+mic_peak-ref_peak
        require(abs(actual_error) <= 128, f'Native-decoded generated clap remains {actual_error} samples apart')
        project = output / 'Generated Audio Workflow.fcproj'; c.command('file.saveAs', {'path': str(project)})
        # New sequences avoid accidental normalization/presets/mixing in the hot-float roundtrip.
        hot_sequence = self.sequence(c, 'Raw float headroom export', 1)
        c.command('timeline.place', {'item': items['mic-float32.wav'], 'video': False, 'audioTrack': 'A1', 'duration': 3*TPS})
        hot_delivery, hot_samples = self.export_wav(c, output, 'float32-headroom', hot_sequence, 3, 32, True)
        require(max(abs(x) for x in hot_samples[0]) > 1.0, 'Float export silently clamped recoverable headroom')
        require(max(abs(x-y) for x, y in zip(hot_samples[0], hot[0])) < 2e-6 and all(x == 0 for x in hot_samples[1]), 'Raw float export changed source sample/channel identity')
        delivery_sequence = self.sequence(c, 'Peak-limited integer delivery', 1)
        delivery_clip = c.command('timeline.place', {'item': items['mic-pcm24.wav'], 'video': False, 'audioTrack': 'A1', 'duration': 3*TPS})['clips'][0]
        normal_plan = c.tool('prepare_audio', {'clip': delivery_clip, 'routing': 'left', 'normalize': True,
                                              'target_lufs': -16, 'max_gain_db': 24, 'true_peak_dbtp': -1})[0]['plan']
        self.apply(c, normal_plan, 'normalize-delivery')
        normalized = self.inspect(c, output, 'normalized-clip', {'target': 'clip', 'clip': delivery_clip})
        require(normalized['loudness']['truePeakDbtp'] <= -.8, 'Peak-limited normalization exceeded the -1dBTP ceiling')
        integer_delivery, integer_samples = self.export_wav(c, output, 'int24-normalized', delivery_sequence, 3, 24, False)
        require(integer_delivery['nativeReopen']['loudness']['truePeakDbtp'] <= -.8, 'Integer delivery exceeds peak ceiling on native reopen')
        require(max(abs(x-y) for x, y in zip(*integer_samples)) < 2**-22, 'Integer delivery lost centered channel identity')
        require(all(x['overFullScaleSamples'] == 0 for x in integer_delivery['nativeReopen']['channels']), 'Integer delivery has full-scale samples')
        final_project = output / 'Audio Delivery Tests.fcproj'; c.command('file.saveAs', {'path': str(final_project)})
        self.report['cases']['generated'] = {'status': 'passed', 'placement': {'sequence': sequence, 'video': video, 'mainStream0': main, 'scratchStream1': scratch},
            'mappingChecks': mapping_checks, 'streamConfiguration': streams, 'repairApproval': approval, 'staleReceipt': rejected,
            'sync': {'waveform': sync, 'manualFallbackUsed': manual, 'approval': sync_approval, 'knownAnchorErrorSamples': anchor_error,
                     'nativeDecodedClapErrorSamples': actual_error, 'lossyClapToleranceSamples': 128},
            'hotFloatExport': {'path': hot_delivery['path'], 'shape': hot_delivery['shape'], 'peak': max(abs(x) for x in hot_samples[0])},
            'integerDelivery': {'path': integer_delivery['path'], 'shape': integer_delivery['shape'], 'measuredLufs': integer_delivery['nativeReopen']['loudness']['integratedLufs'],
                                'truePeakDbtp': integer_delivery['nativeReopen']['loudness']['truePeakDbtp'], 'targetLufs': -16,
                                'limit': 'Target LUFS may remain lower when the true-peak ceiling prevents additional gain.'},
            'projects': [str(project), str(final_project)]}
        self.save()
        if self.args.mix:
            self.generated_mix(c, output, items)

    def snapshot_project(self, c, output, name):
        path = output / (name+'.fcproj')
        c.command('file.saveCopy', {'path': str(path)})
        document = json.loads(path.read_text())
        project = required(document, 'project', name)
        # Issued effect IDs deliberately are never recycled across undo. This allocator floor
        # cannot affect a render or change any edit, so compare the remaining complete project.
        project.pop('next_effect_instance_id', None)
        return project

    @staticmethod
    def stored_sequence(project, sequence):
        item = required(required(project, 'items', 'stored project'), str(sequence), 'stored sequence item')
        return required(required(item, 'kind', 'stored sequence kind'), 'Sequence', 'stored sequence')

    @staticmethod
    def stored_clips(sequence):
        return {item['id']: item for track in sequence['audio_tracks']+sequence['video_tracks'] for item in track['items']}

    @staticmethod
    def owned_counts(sequence, mains):
        clips = Run.stored_clips(sequence)
        names = ('voice_polish_eq', 'voice_polish_multiband', 'voice_polish_limiter')
        result = {str(clip): {name: sum(e['effect'] == name for e in clips[clip]['effects']) for name in names} for clip in mains}
        result['masterLimiter'] = sum(e['effect'] == 'audio_mix_limiter' for e in sequence['master_effects'])
        return result

    def generated_mix(self, c, output, items):
        self.sequence(c, 'Generated voice + retained backup + ducked music', 4)
        main = c.command('timeline.place', {'item': items['mix-main.wav'], 'video': False, 'audioTrack': 'A1', 'seconds': 2, 'duration': 3*TPS})['clips'][0]
        backups = c.command('timeline.place', {'item': items['two-audio-streams.mp4'], 'video': False, 'audioTrack': 'A2', 'seconds': 2, 'duration': 3*TPS})['clips']
        music = c.command('timeline.place', {'item': items['music-bed.wav'], 'video': False, 'audioTrack': 'A4', 'seconds': 2, 'duration': 3*TPS})['clips'][0]
        self.mix_case(c, output, 'generated-mix', [main], backups, [music], [])

    def mix_case(self, c, output, name, mains, backups, music, videos, preserve_stereo=False):
        print('Reviewable voice/music mix smoke:', name, flush=True)
        live = c.tool('sequence_inspect')[0]
        sequence = live['id']
        params = {'main': mains, 'backup': backups, 'music': music, 'tone': 'warm', 'intensity': .35,
                  'repair_channels': True, 'normalize': True, 'target_lufs': -16, 'max_gain_db': 12,
                  'true_peak_dbtp': -1, 'duck_music': bool(music), 'duck_db': -12,
                  'duck_fade_seconds': .4, 'final_limiter': True, 'limiter_ceiling_dbfs': -1.5,
                  'replace_animation': False, 'label': name+' reviewable owned recipe'}
        before = self.snapshot_project(c, output, name+'-before')
        before_seq = self.stored_sequence(before, sequence)
        before_rows = self.stored_clips(before_seq)
        analysis = c.tool('audio_mix_inspect', params)[0]
        write_json(output / (name+'-inspection.json'), analysis)
        staged = c.tool('prepare_audio_mix', params)[0]
        plan = required(staged, 'plan', 'prepare_audio_mix')
        write_json(output / (name+'-plan.json'), staged)
        after_preview = self.snapshot_project(c, output, name+'-after-preview')
        require(before == after_preview, name+': inspection/preparation mutated live project data')
        _, sound = c.audition({'subject': {'target': 'sequence', 'sequence': sequence, 'plan': plan},
                                'start': 2*TPS, 'end': 5*TPS, 'compare': True}, output, name+'-audition')
        # The tone/dynamics recipe is nonlinear. Check finite, bounded audio, never correlation
        # equality against the unprocessed voice.
        require(all(math.isfinite(x) for state in sound for channel in state for x in channel), name+': audition contains non-finite samples')
        post_peak = max(abs(x) for channel in sound[1] for x in channel)
        require(post_peak <= 10**(-1.5/20)+1e-4, name+': final sample limiter exceeded -1.5dBFS')
        require(max(abs(a-b) for ch in range(2) for a,b in zip(sound[0][ch], sound[1][ch])) > 1e-5,
                name+': the mix proposal did not audibly affect the test signal')
        approval = self.apply(c, plan, name)
        applied = self.snapshot_project(c, output, name+'-applied')
        applied_seq = self.stored_sequence(applied, sequence)
        applied_rows = self.stored_clips(applied_seq)
        require(set(before_rows) == set(applied_rows), name+': recipe removed or introduced clips')
        for clip in backups:
            require(not applied_rows[clip]['enabled'], name+': explicit backup was not retained muted')
            for field in ('item','start','duration','source_in','audio_stream','source_channels'):
                default = [] if field == 'source_channels' else 0 if field == 'audio_stream' else None
                require(applied_rows[clip].get(field, default) == before_rows[clip].get(field, default), name+': backup source/timing changed')
        for clip in videos:
            require(applied_rows[clip] == before_rows[clip], name+': voice recipe changed video')
        for clip in mains:
            for field in ('item','start','duration','source_in','audio_stream','source_channels'):
                default = [] if field == 'source_channels' else 0 if field == 'audio_stream' else None
                require(applied_rows[clip].get(field, default) == before_rows[clip].get(field, default), name+': voice recipe changed main source/timing')
        if preserve_stereo:
            choices = c.tool('audio_channels_get', {'clips': mains})[0]['clips']
            require(all(row['routing']['mode'] == 'stereo' for row in choices), name+': recipe forced one-sided/mono routing onto active stereo')
        counts = self.owned_counts(applied_seq, mains)
        require(counts['masterLimiter'] == 1, name+': expected one owned post-master limiter')
        master_limiter = next(e for e in applied_seq['master_effects'] if e['effect'] == 'audio_mix_limiter')
        require(master_limiter.get('post_fader', False), name+': output safety limiter must follow the master fader')
        for clip in mains:
            require(all(n == 1 for n in counts[str(clip)].values()), name+': owned voice stack missing/duplicated')
        ducking = []
        for clip in music:
            volume = next((e for e in applied_rows[clip]['effects'] if e['effect'] == 'volume'), None)
            require(volume is not None, name+': music Volume missing')
            keys = volume['params']['level']['keyframes']
            values = [key['value'].get('Float') for key in keys]
            require(len(keys) >= 2 and all(isinstance(x,(int,float)) and math.isfinite(x) for x in values), name+': music ducking keyframes missing/non-finite')
            base_db = volume['params']['level']['value'].get('Float')
            require(isinstance(base_db, (int,float)) and math.isfinite(base_db), name+': music nominal Volume is invalid')
            depth_db = base_db-min(values)
            require(max(values)-min(values) > .5 and depth_db >= abs(params['duck_db'])-.1,
                    name+': music automation does not meaningfully duck relative to its normalized base')
            require([key['time'] for key in keys] == sorted(key['time'] for key in keys), name+': music duck keyframes not ordered')
            ducking.append({'clip': clip, 'keys': len(keys), 'nominalDb': base_db, 'duckDepthDb':depth_db,
                            'minimumDb': min(values), 'maximumDb': max(values)})
        c.command('edit.undo')
        undone = self.snapshot_project(c, output, name+'-undone')
        require(undone == before, name+': one undo failed to restore full project (including master and automation)')
        c.command('edit.redo')
        repeat_params = copy.deepcopy(params)
        animation_refusal = None
        if music:
            before_refusal = self.snapshot_project(c, output, name+'-before-animation-refusal')
            try:
                c.tool('prepare_audio_mix', repeat_params)
                raise RuntimeError(name+': repeating ducking replaced animation without explicit approval')
            except RuntimeError as error:
                require('animation' in str(error) and 'replaceAnimation' in str(error), name+': unexpected repeat refusal: '+str(error))
                animation_refusal = str(error)
            after_refusal = self.snapshot_project(c, output, name+'-after-animation-refusal')
            require(before_refusal == after_refusal, name+': refused animation replacement mutated the project')
            # The harness explicitly approves replacing its own previously generated curve.
            repeat_params['replace_animation'] = True
        rerun = c.tool('prepare_audio_mix', repeat_params)[0]
        second = self.apply(c, rerun['plan'], name+'-repeat', require_undo=False)
        repeated = self.snapshot_project(c, output, name+'-repeated')
        repeated_seq = self.stored_sequence(repeated, sequence)
        require(self.owned_counts(repeated_seq, mains) == counts, name+': repeating the recipe duplicated owned effects')
        checked = self.inspect(c, output, name+'-final-coverage', {'target':'sequence','sequence':sequence}, start=2*TPS, end=5*TPS)
        require(all(channel['peak'] <= 10**(-1.5/20)+1e-4 for channel in checked['channels']), name+': sampled sequence exceeds final sample ceiling')
        self.report['cases'][name] = {'status':'passed','main':mains,'retainedMutedBackup':backups,'music':music,
                                     'params':params,'approval':approval,'repeatApproval':second,'repeatParams':repeat_params,
                                     'unapprovedAnimationReplacementRefusal':animation_refusal,'ownedEffectCounts':counts,
                                     'musicDucking':ducking,'auditionSamplePeak':post_peak,
                                     'measuredTruePeakDbtp':checked['loudness']['truePeakDbtp'],
                                     'coverage':checked['coverage'],
                                     'limits':['Tone/dynamics are nonlinear; no claim of waveform equality or subjective voice quality.',
                                               'Final limiter assertion is sample-peak dBFS over the explicit3s window; true peak is separately reported.']}
        self.save()

    def actual_cases(self):
        root = self.args.actual_dir.resolve()
        sources = [root / (stem+ext) for stem in ('GX010005', 'GX010006') for ext in ('.MP4', '.WAV')]
        self.protect(sources)
        for stem in ('GX010005', 'GX010006'):
            print(f'Native actual-file audio smoke: {stem} (first3s only)', flush=True)
            c, output = self.client('actual-'+stem)
            sequence = self.sequence(c, stem+' stored-stream audio test', 4)
            movie, wave = root/(stem+'.MP4'), root/(stem+'.WAV')
            probe = self.probe(movie, stem+'-input')
            streams = [x for x in probe['streams'] if x.get('codec_type') == 'audio']
            main_stream = 1 if stem == 'GX010005' and len(streams) > 1 else 0
            picks = [2, 3] if stem == 'GX010005' else [0, 1]
            movie_item, wave_item = self.import_one(c, movie), self.import_one(c, wave)
            native_streams = []
            for index in range(len(streams)):
                native_streams.append(self.inspect(c, output, f'stored-stream-{index}',
                                                  {'target': 'source', 'item': movie_item}, stream=index, start=0, end=3*TPS))
            wave_inspection = self.inspect(c, output, 'companion-native-channels',
                                           {'target': 'source', 'item': wave_item}, start=0, end=3*TPS)
            require(len(wave_inspection['channels']) == (4 if stem == 'GX010005' else 2), f'{stem}: native reader lost companion channels')
            placed = c.command('timeline.place', {'item': movie_item, 'track': 'V1', 'audioTrack': 'A1',
                                                 'seconds': 2, 'duration': 3*TPS})
            routed = c.tool('audio_channels_get', {'clips': placed['clips']})[0]['clips']
            require(len(routed) == len(streams) and {x['stream'] for x in routed} == set(range(len(streams))),
                    f'{stem}: timeline placement dropped or duplicated an audio stream')
            before_config = c.tool('sequence_inspect')[0]
            camera_video = next(x['clip'] for t in before_config['video'] for x in t['items'])
            video_row = copy.deepcopy(clip_rows(before_config)[camera_video])
            main = next(x['clip'] for x in routed if x['stream'] == main_stream)
            configuration = c.command('audio.streams.configure', {'clips': [main], 'mainStream': main_stream, 'muteSecondary': True})
            configured = clip_rows(c.tool('sequence_inspect')[0])
            require(configured[camera_video] == video_row, f'{stem}: stream selection changed camera video')
            for row in routed:
                require(configured[row['clip']]['enabled'] == (row['stream'] == main_stream), f'{stem}: wrong main/secondary stream enable state')
            companion = c.command('timeline.place', {'item': wave_item, 'video': False, 'audioTrack': 'A3',
                                                    'seconds': 2, 'duration': 3*TPS})['clips'][0]
            c.command('audio.source.set', {'clip': companion, 'channels': picks})
            source_choices = c.tool('audio_channels_get', {'clip': companion})[0]['clips'][0]
            require(source_choices['sourceChannels'] == picks, f'{stem}: companion source mapping was not retained')
            pre = self.inspect(c, output, 'chosen-main-before', {'target': 'clip', 'clip': main, 'effects': 'pre'})
            require(pre['recommendation']['mode'] is None, f'{stem}: native inspector falsely diagnosed a missing side in these active/dual-mono channels')
            before_plan = c.tool('sequence_inspect')[0]
            frozen_rows = clip_rows(before_plan)
            preserve = {key: copy.deepcopy(row) for key, row in frozen_rows.items() if key != main}
            plan = c.tool('prepare_audio', {'clip': main, 'routing': 'auto', 'normalize': True,
                                            'target_lufs': -16, 'max_gain_db': 12, 'true_peak_dbtp': -1,
                                            'label': stem+' reviewable stored-stream level test'})[0]['plan']
            pair = c.tool('audio_inspect', {'subject': {'target': 'clip', 'clip': main, 'plan': plan}, 'compare': True})[0]
            write_json(output / 'plan-inspection.json', pair)
            _, auditions = c.audition({'subject': {'target': 'clip', 'clip': main, 'plan': plan}, 'compare': True}, output, 'actual-before-after')
            correlations = [correlation(auditions[0][ch], auditions[1][ch]) for ch in (0, 1)]
            require(all(x is not None and x > .999 for x in correlations), f'{stem}: level-only preparation changed channel content')
            gain_ratios = [rms(auditions[1][ch])/rms(auditions[0][ch]) for ch in (0, 1)]
            require(abs(gain_ratios[0]-gain_ratios[1]) < max(gain_ratios)*0.005, f'{stem}: auto level repair changed left/right balance')
            require(normalized_sequence(before_plan) == normalized_sequence(c.tool('sequence_inspect')[0]), f'{stem}: previews changed the live project')
            approval = self.apply(c, plan, stem+'-levels', require_undo=False)
            after = c.tool('sequence_inspect')[0]
            require(all(clip_rows(after)[key] == row for key, row in preserve.items()), f'{stem}: selected main repair changed another stream/video/companion')
            # A no-op proposal is legitimate if no gated signal was available in the short window.
            steps = approval['receipt']['result']['undoSteps']
            if steps:
                c.command('edit.undo')
                require(normalized_sequence(before_plan) == normalized_sequence(c.tool('sequence_inspect')[0]), f'{stem}: one undo failed')
                c.command('edit.redo')
            inspect_after = self.inspect(c, output, 'chosen-main-after', {'target': 'clip', 'clip': main})
            require(inspect_after['loudness']['truePeakDbtp'] is None or inspect_after['loudness']['truePeakDbtp'] <= -.8,
                    f'{stem}: level proposal exceeded its peak ceiling')
            # Only measured waveform confidence can stage unknown real-file sync. No invented clap anchors.
            sync_args = {'reference': main, 'clip': companion, 'window_seconds': 3, 'max_lag_seconds': .5, 'mute_reference': False}
            sync = c.tool('audio_sync_inspect', sync_args)[0]
            write_json(output / 'native-real-sync.json', sync)
            sync_result = {'inspection': sync, 'knownClapGroundTruth': False,
                           'limit': 'Native waveform execution/guard evidence only; independent acoustic/source anchor ground truth is not provided.'}
            protected = {row['clip']: copy.deepcopy(clip_rows(c.tool('sequence_inspect')[0])[row['clip']]) for row in routed}
            protected[camera_video] = video_row
            if sync.get('status') == 'ready':
                sync_plan = c.tool('prepare_audio_sync', sync_args)[0]['plan']
                sync_result['approval'] = self.apply(c, sync_plan, stem+'-sync', require_undo=False)
                now = clip_rows(c.tool('sequence_inspect')[0])
                require(all(now[key] == value for key, value in protected.items()), f'{stem}: companion sync changed camera video or stored streams')
                require(now[companion]['start']*RATE % TPS == 0, f'{stem}: sync output is not on a48k sample boundary')
                sync_result['targetAfter'] = now[companion]
            else:
                require(sync.get('status') == 'needsManual', f'{stem}: unexpected sync state: {sync.get("status")}')
                try:
                    c.tool('prepare_audio_sync', sync_args)
                    raise RuntimeError(f'{stem}: low-confidence sync unexpectedly created a proposal')
                except RuntimeError as error:
                    require('needsManual' in str(error), f'{stem}: unexpected sync refusal: {error}')
                sync_result['requiresUserSourceAnchors'] = True
            project = output / (stem+' Audio Review.fcproj')
            c.command('file.saveAs', {'path': str(project)})
            self.report['cases'][stem] = {'status': 'passed_with_explicit_sync_limits', 'inputMovie': str(movie), 'inputCompanion': str(wave),
                'windowSeconds': 3, 'nativeStreamCount': len(native_streams), 'chosenStoredMainStream': main_stream,
                'chosenCompanionChannels': picks, 'choiceBasis': 'Explicit regression setting from prior local stored-channel correspondence audit; not a physical hardware identification.',
                'configuration': configuration, 'stereoPreservedCorrelations': correlations, 'uniformGainRatios': gain_ratios,
                'approval': approval, 'sync': sync_result, 'project': str(project)}
            self.save()
            if self.args.mix and stem == 'GX010005':
                mix_sequence = self.sequence(c, stem+' explicit main/backup voice mix', 3)
                mix_placed = c.command('timeline.place', {'item': movie_item, 'track': 'V1', 'audioTrack': 'A1', 'seconds': 2, 'duration': 3*TPS})
                mix_routes = c.tool('audio_channels_get', {'clips': mix_placed['clips']})[0]['clips']
                chosen = next(row['clip'] for row in mix_routes if row['stream'] == 1)
                backups = [row['clip'] for row in mix_routes if row['stream'] != 1]
                mix_video = next(row['clip'] for track in c.tool('sequence_inspect')[0]['video'] for row in track['items'])
                self.mix_case(c, output, 'actual005-mix', [chosen], backups, [], [mix_video], preserve_stereo=True)

    def finish(self):
        cleanup_errors = []
        for c in reversed(self.clients):
            try:
                c.close()
            except Exception as error:
                cleanup_errors.append(str(error))
        self.report['originalsAfter'] = {}
        for path in self.protected:
            try:
                self.report['originalsAfter'][str(path)] = identity(path)
            except Exception as error:
                self.report['originalsAfter'][str(path)] = {'error': str(error)}
        self.report['sourcesPreserved'] = self.report['originalsBefore'] == self.report['originalsAfter']
        if not self.report['sourcesPreserved']:
            self.report['status'] = 'failed'
            self.report['sourcePreservationError'] = 'An input identity changed; inspect before/after full hashes'
        try:
            after = identity(self.args.cli)
            self.report['binaryAfterIdentity'] = after
            self.report['binaryUnchangedDuringRun'] = self.report.get('binaryIdentity') == after
            if not self.report['binaryUnchangedDuringRun']:
                cleanup_errors.append('CLI changed during the run; repeat against a stable binary copy')
        except Exception as error:
            cleanup_errors.append(str(error))
        if cleanup_errors:
            self.report['status'] = 'failed'
            self.report['cleanupErrors'] = cleanup_errors
        self.save()


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cli', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path, help='New exclusive run directory (must not exist)')
    parser.add_argument('--ffmpeg', default=shutil.which('ffmpeg') or '/opt/homebrew/bin/ffmpeg')
    parser.add_argument('--ffprobe', default=shutil.which('ffprobe') or '/opt/homebrew/bin/ffprobe')
    parser.add_argument('--actual-dir', type=Path, help='Explicitly authorized directory containing GX010005/006 MP4+WAV pairs')
    parser.add_argument('--mix', action='store_true', help='Require new typed mic/voice/music mix tools and exercise owned EQ/dynamics/ducking/limiter recipes')
    parser.add_argument('--actual-only', action='store_true', help='Skip generated suite; requires --actual-dir')
    parser.add_argument('--timeout', type=float, default=240)
    args = parser.parse_args()
    require(args.timeout > 0 and math.isfinite(args.timeout), '--timeout must be finite and positive')
    require(not args.actual_only or args.actual_dir is not None, '--actual-only requires --actual-dir')
    args.cli = args.cli.resolve()
    require(args.cli.is_file(), f'CLI not found: {args.cli}')
    return args


def main():
    args = arguments()
    run = Run(args)
    started = time.monotonic()
    try:
        run.report['binaryIdentity'] = identity(args.cli)
        run.report['version'] = run.external([str(args.cli), '--version'], 'cli-version').strip()
        run.report['oracles'] = {'ffmpeg': run.external([args.ffmpeg, '-version'], 'ffmpeg-version').splitlines()[0],
                                 'ffprobe': run.external([args.ffprobe, '-version'], 'ffprobe-version').splitlines()[0]}
        if not args.actual_only:
            print('Generating small PCM24/float32/multistream fixtures', flush=True)
            paths, pcm, hot, four = run.generate()
            print('Running generated native audio workflow', flush=True)
            run.generated_case(paths, pcm, hot, four)
        if args.actual_dir:
            run.actual_cases()
        run.report['status'] = 'passed'
    except Exception as error:
        run.report['status'] = 'failed'
        run.report['error'] = str(error)
        run.report['traceback'] = traceback.format_exc()
        print(run.report['traceback'], file=sys.stderr, flush=True)
    finally:
        run.report['elapsedSeconds'] = round(time.monotonic()-started, 3)
        run.finish()
    print(json.dumps({'status': run.report['status'], 'report': str(run.out/'report.json'),
                      'sourcesPreserved': run.report['sourcesPreserved'], 'error': run.report.get('error')}, indent=2))
    return 0 if run.report['status'] == 'passed' else 1


if __name__ == '__main__':
    sys.exit(main())
