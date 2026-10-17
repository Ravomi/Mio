#!/usr/bin/env python3
"""Opt-in, bounded live Provider handshake through the actual Mio CLI.

Never persists raw stdout/stderr, credentials, prompts or response text. The
report proves only one authenticated streaming completion and reported usage;
429, sustained pressure, recovery and other providers require separate evidence.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/mio'))
    parser.add_argument('--provider', choices=['deepseek', 'anthropic', 'google'], default='deepseek')
    parser.add_argument('--model', default='deepseek-chat')
    parser.add_argument('--output', type=Path, default=Path('target/provider-live.json'))
    args = parser.parse_args()
    credential = {'deepseek': 'DEEPSEEK_API_KEY', 'anthropic': 'ANTHROPIC_API_KEY', 'google': 'GEMINI_API_KEY'}[args.provider]
    if not os.environ.get(credential):
        parser.error(f'{credential} must be configured in the environment')
    binary = args.binary.resolve(strict=True)
    with binary.open('rb') as handle:
        binary_digest = hashlib.file_digest(handle, 'sha256').hexdigest()
    environment = dict(os.environ)
    # Explicit provider/model choose the native preset; inherited MIO overrides
    # must not silently redirect the credential to another endpoint.
    for key in list(environment):
        if key.startswith('MIO_'):
            environment.pop(key)
    if args.provider == 'google':
        environment['GOOGLE_API_KEY'] = environment[credential]
    start = time.monotonic()
    events = []
    returncode = None
    timed_out = False
    with tempfile.TemporaryDirectory(prefix='mio-provider-live-') as workspace:
        command = [str(binary), '--cwd', workspace, '--no-session', '--no-context',
                   '--no-mcp', '--no-shared-limits', '--no-tools', '--json',
                   '--provider', args.provider, '--model', args.model,
                   '--thinking', 'off', '--max-model-calls', '1',
                   '--max-total-tokens', '4096', '--max-output-tokens', '128',
                   '--context-window', '8192', '--max-turns', '1',
                   '-p', 'Respond exactly MIO_LIVE_OK.']
        # The CLI output is limited by this tiny completion; credentials and raw
        # error payloads remain in memory and are discarded, never copied to logs.
        process = subprocess.Popen(command, cwd=workspace, env=environment,
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, start_new_session=True)
        try:
            stdout, _stderr = process.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGKILL)
            stdout, _stderr = process.communicate()
        returncode = process.returncode
        for line in stdout.splitlines():
            try:
                item = json.loads(line)
            except (ValueError, UnicodeError):
                continue
            if isinstance(item, dict):
                events.append(item)
    calls = [e.get('data', {}) for e in events if e.get('type') == 'completion_call']
    usage = calls[0].get('usage', {}) if len(calls) == 1 else {}
    counters = {name: usage.get(name) for name in ('input_tokens', 'output_tokens', 'total_tokens', 'cached_input_tokens', 'cache_creation_input_tokens')}
    observed_usage = all(isinstance(counters[k], int) and not isinstance(counters[k], bool) and counters[k] >= 0 for k in ('input_tokens', 'output_tokens'))
    passed = returncode == 0 and len(calls) == 1 and observed_usage and not timed_out
    report = {'version': 1, 'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'provider': args.provider, 'model': args.model, 'thinking': 'off',
              'binary_sha256': binary_digest, 'observed_model_calls': len(calls),
              'authenticated_stream_completion': returncode == 0 and len(calls) == 1,
              'reported_usage': counters, 'passed': passed, 'returncode': returncode,
              'timeout': timed_out, 'elapsed_seconds': round(time.monotonic()-start, 3),
              'unverified': ['429/Retry-After', 'sustained concurrency', 'cancellation/recovery',
                             'thinking modes', 'other providers', 'cross-platform']}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps(report, ensure_ascii=False))
    return 0 if passed else 1


if __name__ == '__main__':
    raise SystemExit(main())
